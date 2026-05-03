/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import {
	Disposable,
	DisposableStore,
	IDisposable,
	toDisposable,
} from '../../../../base/common/lifecycle.js';
import { Position } from '../../../../editor/common/core/position.js';
import { Range } from '../../../../editor/common/core/range.js';
import {
	InlineCompletion,
	InlineCompletionContext,
	InlineCompletions,
	InlineCompletionsProvider,
} from '../../../../editor/common/languages.js';
import { ILanguageFeaturesService } from '../../../../editor/common/services/languageFeatures.js';
import { ITextModel } from '../../../../editor/common/model.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import { STORAGE_KEY_BACKEND_URL } from '../common/distributedModels.js';
import { DEFAULT_BASE_URL } from './agentClient.js';

/**
 * How much code on either side of the cursor to send to the backend. Local
 * coder models accept this comfortably and any more risks blowing past
 * `num_ctx` once the FIM markers are added.
 */
const PREFIX_CHAR_BUDGET = 4_000;
const SUFFIX_CHAR_BUDGET = 1_000;

/**
 * Debounce gap. The model is fast but typing fires lots of position-change
 * events; coalescing keeps the UI snappy.
 */
const DEBOUNCE_MS = 180;

interface CompleteResponse {
	readonly completion: string;
}

/**
 * Provides inline ghost-text completions backed by the Distributed Models
 * `POST /complete` endpoint. Registered for every language at workbench
 * startup; the backend handles the model selection.
 */
export class InlineCompletionsBackend extends Disposable implements InlineCompletionsProvider {
	readonly groupId = 'distributedModels.inline';

	constructor(
		@IConfigurationService private readonly configurationService: IConfigurationService,
		@ILogService private readonly logService: ILogService,
	) {
		super();
	}

	private get baseUrl(): string {
		const fromSettings = this.configurationService.getValue<string>(
			STORAGE_KEY_BACKEND_URL,
		);
		return (fromSettings && fromSettings.trim()) || DEFAULT_BASE_URL;
	}

	async provideInlineCompletions(
		model: ITextModel,
		position: Position,
		context: InlineCompletionContext,
		token: CancellationToken,
	): Promise<InlineCompletions> {
		void context;
		// Debounce typing bursts.
		const debounced = await this.waitDebounced(token);
		if (!debounced || token.isCancellationRequested) {
			return { items: [] };
		}
		try {
			const completion = await this.fetchCompletion(model, position, token);
			return { items: completion ? [completion] : [] };
		} catch (err) {
			this.logService.warn(`distributed-models: inline completion failed: ${err}`);
			return { items: [] };
		}
	}

	disposeInlineCompletions(): void {
		// Stateless — nothing to free.
	}

	handleItemDidShow(): void {
		// no-op
	}

	private waitDebounced(token: CancellationToken): Promise<boolean> {
		return new Promise<boolean>((resolve) => {
			const timer = setTimeout(() => {
				cancelHandle.dispose();
				resolve(true);
			}, DEBOUNCE_MS);
			const cancelHandle = token.onCancellationRequested(() => {
				clearTimeout(timer);
				resolve(false);
			});
		});
	}

	private async fetchCompletion(
		model: ITextModel,
		position: Position,
		token: CancellationToken,
	): Promise<InlineCompletion | undefined> {
		const prefix = readPrefix(model, position, PREFIX_CHAR_BUDGET);
		const suffix = readSuffix(model, position, SUFFIX_CHAR_BUDGET);
		if (!prefix.trim() && !suffix.trim()) {
			return undefined;
		}
		const file = model.uri?.path ?? undefined;
		const language = model.getLanguageId();
		const controller = new AbortController();
		const cancelDisposable = token.onCancellationRequested(() => controller.abort());
		try {
			const response = await fetch(`${this.baseUrl}/complete`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					prefix,
					suffix,
					file,
					language,
					max_tokens: 128,
				}),
				signal: controller.signal,
			});
			if (!response.ok) {
				return undefined;
			}
			const body = (await response.json()) as CompleteResponse;
			const completion = body.completion ?? '';
			if (!completion.trim()) {
				return undefined;
			}
			const item: InlineCompletion = {
				insertText: completion,
				range: new Range(
					position.lineNumber,
					position.column,
					position.lineNumber,
					position.column,
				),
			};
			return item;
		} finally {
			cancelDisposable.dispose();
		}
	}
}

function readPrefix(model: ITextModel, position: Position, budget: number): string {
	let collected = '';
	let line = position.lineNumber;
	let column = position.column;
	while (line >= 1 && collected.length < budget) {
		const lineContent = model.getLineContent(line);
		const slice = line === position.lineNumber ? lineContent.slice(0, column - 1) : lineContent;
		collected = `${slice}${line === position.lineNumber ? '' : '\n'}${collected}`;
		if (line === 1) {
			break;
		}
		line -= 1;
		column = model.getLineMaxColumn(line);
	}
	if (collected.length > budget) {
		return collected.slice(collected.length - budget);
	}
	return collected;
}

function readSuffix(model: ITextModel, position: Position, budget: number): string {
	const total = model.getLineCount();
	let collected = '';
	let line = position.lineNumber;
	let column = position.column;
	while (line <= total && collected.length < budget) {
		const lineContent = model.getLineContent(line);
		const slice =
			line === position.lineNumber ? lineContent.slice(column - 1) : `\n${lineContent}`;
		collected += slice;
		if (line === total) {
			break;
		}
		line += 1;
		column = 1;
	}
	if (collected.length > budget) {
		return collected.slice(0, budget);
	}
	return collected;
}

/**
 * Register the backend-driven inline completion provider for every
 * language. The selector `{ pattern: '**' }` is the standard way to opt
 * into all languages, matching how VS Code's own test fixtures register
 * cross-language providers.
 */
export function registerInlineCompletionProvider(
	languageFeatures: ILanguageFeaturesService,
	provider: InlineCompletionsBackend,
): IDisposable {
	const store = new DisposableStore();
	store.add(
		languageFeatures.inlineCompletionsProvider.register({ pattern: '**' }, provider),
	);
	return toDisposable(() => store.dispose());
}

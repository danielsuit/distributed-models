/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import './media/sidebar.css';

import * as DOM from '../../../../base/browser/dom.js';
import { Button } from '../../../../base/browser/ui/button/button.js';
import { InputBox } from '../../../../base/browser/ui/inputbox/inputBox.js';
import { RunOnceScheduler } from '../../../../base/common/async.js';
import { Emitter, Event } from '../../../../base/common/event.js';
import { DisposableStore } from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import { ITextModel } from '../../../../editor/common/model.js';
import { localize } from '../../../../nls.js';
import { IClipboardService } from '../../../../platform/clipboard/common/clipboardService.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { IContextKeyService } from '../../../../platform/contextkey/common/contextkey.js';
import { IContextMenuService } from '../../../../platform/contextview/browser/contextView.js';
import { IContextViewService } from '../../../../platform/contextview/browser/contextView.js';
import { IHoverService } from '../../../../platform/hover/browser/hover.js';
import { IInstantiationService } from '../../../../platform/instantiation/common/instantiation.js';
import { IKeybindingService } from '../../../../platform/keybinding/common/keybinding.js';
import { IFileService } from '../../../../platform/files/common/files.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import { IOpenerService } from '../../../../platform/opener/common/opener.js';
import {
	IStorageService,
	StorageScope,
	StorageTarget,
} from '../../../../platform/storage/common/storage.js';
import { IThemeService } from '../../../../platform/theme/common/themeService.js';
import {
	defaultButtonStyles,
	defaultInputBoxStyles,
} from '../../../../platform/theme/browser/defaultStyles.js';
import { IViewPaneOptions, ViewPane } from '../../../browser/parts/views/viewPane.js';
import { IViewDescriptorService } from '../../../common/views.js';
import { IEditorService } from '../../../services/editor/common/editorService.js';
import {
	IWorkspaceContextService,
} from '../../../../platform/workspace/common/workspace.js';
import {
	IDistributedModelsService,
} from '../common/distributedModels.js';
import {
	AgentLabel,
	ChatTurn,
	ClientEvent,
	FileAction,
	FileOperation,
	ModelAssignments,
} from '../common/types.js';
import { FileOperationsApplier } from './fileOperations.js';
import {
	collectDirectoryRootsFromEditorService,
	expandWorkbenchEditorSides,
} from './editorRoots.js';

const MAX_TRANSCRIPT_ENTRIES = 200;

/**
 * How many recent user/assistant turns to send back to the orchestrator
 * with each new chat. Older turns are dropped client-side; the backend
 * also caps history defensively.
 */
const MAX_HISTORY_TURNS = 16;

/**
 * Fallback when `/config` is unreachable or older backends omit context fields.
 * Must match backend default `DM_OLLAMA_NUM_CTX`.
 */
const FALLBACK_CONTEXT_WINDOW_TOKENS = 8192;

/**
 * Visible editor buffers counted toward the crude context bar alongside chat.
 */
const CONTEXT_METER_MAX_VISIBLE_EDITORS = 16;
/** Per-buffer char cap so a single huge file does not swamp the meter. */
const CONTEXT_METER_MAX_CHARS_PER_BUFFER = 320_000;

/** Cap diff preview length so giant files do not stall the UI. */
const MAX_REVIEW_PREVIEW_CHARS = 48000;

const TRANSCRIPT_STORAGE_KEY = 'distributedModels.transcript.v1';
/** Last / peak heuristic backend prompt sizes (#bytes÷4); persists per workspace folder. */
const BACKEND_PROMPT_STORAGE_KEY = 'distributedModels.backendPrompt.v1';

const COLLAPSED_AGENT_LABELS = new Set(['orchestrator', 'filestructure']);

interface BackendPromptMemory {
	lastApprox: number;
	peakApprox: number;
	lastAgent: AgentLabel | undefined;
}

function formatTokens(n: number): string {
	if (n >= 1000) {
		return `${(n / 1000).toFixed(n >= 10_000 ? 0 : 1)}k`;
	}
	return String(n);
}

function isCollapsedAgentEntry(entry: TranscriptEntry): boolean {
	return Boolean(
		entry.agent &&
		(entry.kind === 'status' || entry.kind === 'log') &&
		COLLAPSED_AGENT_LABELS.has(entry.agent),
	);
}

function formatAgentLabel(agent: string): string {
	if (agent === 'filestructure') {
		return 'File Structure';
	}
	if (agent === 'integration') {
		return 'Integration';
	}
	return agent.charAt(0).toUpperCase() + agent.slice(1);
}

interface TranscriptEntry {
	readonly id: string;
	readonly kind: 'user' | 'assistant' | 'log' | 'status' | 'error' | 'proposal' | 'edit';
	readonly text: string;
	readonly agent?: string;
	readonly proposal?: {
		readonly proposalId: string;
		readonly operation: FileOperation;
	};
}

interface ModelFieldDef {
	readonly key: keyof ModelAssignments;
	readonly label: string;
}

const MODEL_FIELDS: ReadonlyArray<ModelFieldDef> = [
	{ key: 'orchestrator', label: 'Orchestrator' },
	{ key: 'file_structure', label: 'File Structure' },
	{ key: 'code_writer', label: 'Code Writer' },
	{ key: 'error_agent', label: 'Error Agent' },
	{ key: 'review', label: 'Review' },
	{ key: 'integration', label: 'Integration' },
];

/**
 * Side bar chat panel. Lives in the activity bar as the "Distributed
 * Models" view container; the user types into the input at the bottom and
 * each message expands the transcript above with status badges, log lines
 * and accept/reject prompts for file proposals.
 */
export class DistributedModelsSidebar extends ViewPane {
	private readonly _localDispose = this._register(new DisposableStore());
	private readonly _onDidChangeBusy = this._register(new Emitter<boolean>());
	readonly onDidChangeBusy: Event<boolean> = this._onDidChangeBusy.event;

	private readonly transcript: TranscriptEntry[] = [];
	private currentJobId: string | undefined;

	private rootEl!: HTMLDivElement;
	private transcriptEl!: HTMLDivElement;
	private composerInput!: InputBox;
	private sendButton!: Button;
	private saveModelsButton!: Button;
	private modelSettingsEl!: HTMLDivElement;
	private modelStatusEl!: HTMLDivElement;
	private contextMeterEl!: HTMLDivElement;
	private contextMeterIcon!: HTMLDivElement;
	private contextMeterText!: HTMLSpanElement;
	private runRowEl!: HTMLElement;
	private stopRunButton!: Button;
	private retryButton: Button | undefined;
	private modelInputs: Partial<Record<keyof ModelAssignments, InputBox>> = {};
	private settingsOpen = false;
	private modelsLoaded = false;

	private readonly applier: FileOperationsApplier;

	private pendingReview: {
		readonly proposalId: string;
		readonly operation: FileOperation;
		readonly notes: string | null;
	} | undefined;
	private reviewPanelEl!: HTMLDivElement;
	private reviewMetaEl!: HTMLDivElement;
	private reviewNotesEl!: HTMLDivElement;
	private reviewDiffOld!: HTMLPreElement;
	private reviewDiffNew!: HTMLPreElement;
	private reviewApplyBtn!: Button;
	private reviewDiscardBtn!: Button;
	private reviewDismissRow!: HTMLElement;
	private reviewDismissBtn!: Button;
	private reviewBusy = false;

	private readonly contextMeterRefreshScheduler: RunOnceScheduler;
	private readonly editorModelListeners: DisposableStore;

	private contextBudgetEffective = FALLBACK_CONTEXT_WINDOW_TOKENS;
	private contextOrchestratorLabel = '';
	private contextBudgetNative: number | undefined;
	private contextOllamaNumCtx = FALLBACK_CONTEXT_WINDOW_TOKENS;

	private backendPromptMemory: BackendPromptMemory = {
		lastApprox: 0,
		peakApprox: 0,
		lastAgent: undefined,
	};

	constructor(
		options: IViewPaneOptions,
		@IKeybindingService keybindingService: IKeybindingService,
		@IContextMenuService contextMenuService: IContextMenuService,
		@IConfigurationService configurationService: IConfigurationService,
		@IContextKeyService contextKeyService: IContextKeyService,
		@IViewDescriptorService viewDescriptorService: IViewDescriptorService,
		@IInstantiationService instantiationService: IInstantiationService,
		@IOpenerService openerService: IOpenerService,
		@IThemeService themeService: IThemeService,
		@IHoverService hoverService: IHoverService,
		@ILogService private readonly logService: ILogService,
		@IClipboardService private readonly clipboardService: IClipboardService,
		@IContextViewService private readonly contextViewService: IContextViewService,
		@IStorageService private readonly storageService: IStorageService,
		@IDistributedModelsService
		private readonly distributedModels: IDistributedModelsService,
		@IWorkspaceContextService
		private readonly workspaceContextService: IWorkspaceContextService,
		@IEditorService private readonly editorService: IEditorService,
		@IFileService private readonly fileService: IFileService,
	) {
		super(
			options,
			keybindingService,
			contextMenuService,
			configurationService,
			contextKeyService,
			viewDescriptorService,
			instantiationService,
			openerService,
			themeService,
			hoverService,
		);
		this.applier = this._register(instantiationService.createInstance(FileOperationsApplier));
		this.editorModelListeners = this._register(new DisposableStore());
		this.contextMeterRefreshScheduler = this._register(
			new RunOnceScheduler(() => this.updateContextMeter(), 350),
		);
	}

	protected override renderBody(container: HTMLElement): void {
		super.renderBody(container);

		this.rootEl = DOM.append(container, DOM.$('div.distributed-models-root')) as HTMLDivElement;
		this.renderModelSettings(this.rootEl);
		this.renderReviewPanel(this.rootEl);
		this.transcriptEl = DOM.append(this.rootEl, DOM.$('div.dm-transcript')) as HTMLDivElement;
		this.renderComposer(this.rootEl);

		this.restoreTranscript();
		this.restoreBackendPromptMemory();
		this.updateContextMeter();

		this._register(this.distributedModels.connect());
		this._register(
			this.distributedModels.onEvent((event) => this.handleEvent(event)),
		);
		this._register(
			this.editorService.onDidVisibleEditorsChange(() => this.onWorkbenchEditorsChanged()),
		);
		this._register(
			this.editorService.onDidActiveEditorChange(() => this.onWorkbenchEditorsChanged()),
		);
		this.onWorkbenchEditorsChanged();
		void this.refreshContextBudgetFromBackend();
	}

	/**
	 * Scroll the review panel into view when a proposal needs attention.
	 * Wired to the Diff icon in the view title bar.
	 */
	public focusPendingReview(): void {
		if (!this.pendingReview || !this.reviewPanelEl) {
			return;
		}
		this.reviewPanelEl.classList.remove('dm-hidden');
		this.reviewPanelEl.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
	}

	/**
	 * Copies the full chat transcript: every persisted row including agent logs
	 * and status lines (not only user/assistant).
	 */
	public copyEntireChatPlainText(): void {
		const sep = '\n\n---\n\n';
		const blocks: string[] = [];
		for (const entry of this.transcript) {
			const block = this.formatClipboardBlock(entry);
			if (block) {
				blocks.push(block);
			}
		}
		void this.copyPlainText(blocks.join(sep));
	}

	private formatClipboardBlock(entry: TranscriptEntry): string | null {
		const raw = entry.text;
		const trimmed = raw.trim();
		if (!trimmed && !entry.proposal) {
			return null;
		}
		const agent = entry.agent ? ` (${formatAgentLabel(entry.agent)})` : '';
		switch (entry.kind) {
			case 'user':
				return `${localize(
					'distributedModels.clipboard.segmentUser',
					'User',
				)}:\n${raw}`;
			case 'assistant':
				return `${localize(
					'distributedModels.clipboard.segmentAssistant',
					'Assistant',
				)}:\n${raw}`;
			case 'edit':
				return `${localize(
					'distributedModels.clipboard.segmentEdit',
					'Edit',
				)}:\n${raw}`;
			case 'error':
				return `${localize(
					'distributedModels.clipboard.segmentError',
					'Error',
				)}:\n${raw}`;
			case 'log':
				return `${localize(
					'distributedModels.clipboard.segmentLog',
					'Log',
				)}${agent}:\n${raw}`;
			case 'status':
				return `${localize(
					'distributedModels.clipboard.segmentStatus',
					'Status',
				)}${agent}:\n${raw}`;
			case 'proposal': {
				const head = localize(
					'distributedModels.clipboard.segmentProposal',
					'Proposal',
				);
				if (!entry.proposal) {
					return `${head}${agent}:\n${raw}`;
				}
				const summary = this.proposalSummary(
					entry.proposal.operation,
					null,
				);
				const details = trimmed ? `${summary}\n\n${raw}` : summary;
				return `${head} (${entry.proposal.proposalId})${agent}:\n${details}`;
			}
			default:
				return `${entry.kind}${agent}:\n${raw}`;
		}
	}

	/**
	 * Wipe the in-memory transcript and the persisted copy. Wired up to the
	 * trash icon in the view's title bar.
	 */
	public clearChat(): void {
		const dangling = this.pendingReview;
		if (dangling) {
			void this.distributedModels.respondToProposal(dangling.proposalId, false);
		}
		this.teardownPendingReviewUi();
		this.transcript.length = 0;
		this.currentJobId = undefined;
		this.backendPromptMemory = { lastApprox: 0, peakApprox: 0, lastAgent: undefined };
		this.updateRunIndicator();
		this.rerender();
		this.persistTranscript();
		this.persistBackendPromptMemory();
		this.updateContextMeter();
	}

	/**
	 * Toggle (or set) the inline model-settings panel that lives at the top
	 * of the chat view. Wired up to the gear icon in the view's title bar.
	 */
	public toggleModelSettings(forceOpen?: boolean): void {
		const next = forceOpen ?? !this.settingsOpen;
		this.settingsOpen = next;
		this.modelSettingsEl.classList.toggle('dm-hidden', !next);
		if (next && !this.modelsLoaded) {
			void this.loadModelSettings();
		}
	}

	private renderReviewPanel(parent: HTMLElement): void {
		this.reviewPanelEl = DOM.append(
			parent,
			DOM.$('div.dm-review-panel.dm-hidden'),
		) as HTMLDivElement;

		const header = DOM.append(this.reviewPanelEl, DOM.$('div.dm-review-header'));
		const title = DOM.append(header, DOM.$('div.dm-review-title'));
		title.textContent = localize('distributedModels.review.title', 'Review changes');

		this.reviewMetaEl = DOM.append(this.reviewPanelEl, DOM.$('div.dm-review-meta'));

		this.reviewNotesEl = DOM.append(this.reviewPanelEl, DOM.$('div.dm-review-notes.dm-hidden'));

		const diffGrid = DOM.append(this.reviewPanelEl, DOM.$('div.dm-diff-grid'));

		const colOldWrap = DOM.append(diffGrid, DOM.$('div.dm-diff-col'));
		const labelOld = DOM.append(colOldWrap, DOM.$('div.dm-diff-label'));
		labelOld.textContent = localize('distributedModels.review.current', 'Current');
		this.reviewDiffOld = DOM.append(colOldWrap, DOM.$('pre.dm-diff-pre'));

		const colNewWrap = DOM.append(diffGrid, DOM.$('div.dm-diff-col'));
		const labelNew = DOM.append(colNewWrap, DOM.$('div.dm-diff-label'));
		labelNew.textContent = localize('distributedModels.review.proposed', 'Proposed');
		this.reviewDiffNew = DOM.append(colNewWrap, DOM.$('pre.dm-diff-pre'));

		const actions = DOM.append(this.reviewPanelEl, DOM.$('div.dm-review-actions'));

		this.reviewApplyBtn = this._register(
			new Button(actions, {
				title: localize('distributedModels.review.apply', 'Apply change'),
				supportIcons: false,
				...defaultButtonStyles,
			}),
		);
		this.reviewApplyBtn.label = localize('distributedModels.review.apply', 'Apply');
		this._register(this.reviewApplyBtn.onDidClick(() => void this.resolvePendingReview(true)));

		this.reviewDiscardBtn = this._register(
			new Button(actions, {
				title: localize('distributedModels.review.discard', 'Discard change'),
				secondary: true,
				supportIcons: false,
				...defaultButtonStyles,
			}),
		);
		this.reviewDiscardBtn.label = localize('distributedModels.review.discard', 'Discard');
		this._register(this.reviewDiscardBtn.onDidClick(() => void this.resolvePendingReview(false)));

		this.reviewDismissRow = DOM.append(this.reviewPanelEl, DOM.$('div.dm-review-dismiss'));
		this.reviewDismissBtn = this._register(
			new Button(this.reviewDismissRow, {
				title: localize('distributedModels.review.hideHint', 'Hide review panel'),
				secondary: true,
				supportIcons: false,
				...defaultButtonStyles,
			}),
		);
		this.reviewDismissBtn.label = localize(
			'distributedModels.review.hide',
			'Hide (job waits until Apply or Discard)',
		);
		this._register(this.reviewDismissBtn.onDidClick(() => this.hideReviewPanel()));
	}

	private hideReviewPanel(): void {
		this.reviewPanelEl?.classList.add('dm-hidden');
	}

	private teardownPendingReviewUi(): void {
		this.pendingReview = undefined;
		this.reviewBusy = false;
		this.reviewPanelEl?.classList.add('dm-hidden');
		this.reviewDiffOld.textContent = '';
		this.reviewDiffNew.textContent = '';
		this.reviewMetaEl.textContent = '';
		this.reviewNotesEl.textContent = '';
		this.reviewNotesEl.classList.add('dm-hidden');
		this.reviewApplyBtn.enabled = true;
		this.reviewDiscardBtn.enabled = true;
	}

	private openPendingReview(
		proposalId: string,
		operation: FileOperation,
		notes: string | null,
	): void {
		this.pendingReview = { proposalId, operation, notes };
		this.reviewPanelEl.classList.remove('dm-hidden');
		this.reviewMetaEl.textContent = this.proposalSummary(operation, notes);
		if (notes) {
			this.reviewNotesEl.textContent = notes;
			this.reviewNotesEl.classList.remove('dm-hidden');
		} else {
			this.reviewNotesEl.classList.add('dm-hidden');
		}
		void this.hydrateReviewDiff(operation);
		this.focusPendingReview();
	}

	private truncatePreview(text: string): string {
		if (text.length <= MAX_REVIEW_PREVIEW_CHARS) {
			return text;
		}
		return (
			text.slice(0, MAX_REVIEW_PREVIEW_CHARS) +
			`\n\n… ${localize('distributedModels.review.truncated', '(preview truncated)')}`
		);
	}

	private resolveWorkspaceUri(relative: string): URI {
		const folders = this.workspaceContextService.getWorkspace().folders;

		if (folders.length === 1) {
			return URI.joinPath(folders[0].uri, relative);
		}

		if (folders.length > 1) {
			const slash = relative.indexOf('/');
			if (slash !== -1) {
				const nameSeg = relative.slice(0, slash);
				const rest = relative.slice(slash + 1);
				const hit = folders.find((f) => f.name === nameSeg);
				if (hit) {
					return URI.joinPath(hit.uri, rest);
				}
			}
			return URI.joinPath(folders[0].uri, relative);
		}

		const adhocRoots = collectDirectoryRootsFromEditorService(this.editorService);
		if (adhocRoots.length === 1) {
			return URI.joinPath(adhocRoots[0], relative);
		}
		if (adhocRoots.length > 1) {
			const dirPrefix = /^dir(\d+)\//.exec(relative);
			if (dirPrefix) {
				const idx = Number(dirPrefix[1]) - 1;
				const tail = relative.slice(dirPrefix[0].length);
				if (idx >= 0 && idx < adhocRoots.length) {
					return URI.joinPath(adhocRoots[idx], tail);
				}
			}
			return URI.joinPath(adhocRoots[0], relative);
		}

		return URI.file(relative);
	}

	private async hydrateReviewDiff(operation: FileOperation): Promise<void> {
		const uri = this.resolveWorkspaceUri(operation.file);
		let currentText = '';
		try {
			if (await this.fileService.exists(uri)) {
				const bytes = await this.fileService.readFile(uri);
				currentText = bytes.value.toString();
			}
		} catch (err) {
			this.logService.warn('distributed-models: could not read file for review', err);
		}

		let oldLabel = this.truncatePreview(currentText);
		let newLabel = '';

		switch (operation.action) {
			case FileAction.Create:
				oldLabel = localize('distributedModels.review.emptyFile', '(new file)');
				newLabel = this.truncatePreview(operation.content ?? '');
				break;
			case FileAction.Edit:
				newLabel = this.truncatePreview(operation.content ?? '');
				break;
			case FileAction.Delete:
				oldLabel = this.truncatePreview(currentText);
				newLabel = localize('distributedModels.review.deleteFile', '(delete file)');
				break;
			default:
				newLabel = '';
		}

		this.reviewDiffOld.textContent = oldLabel || ' ';
		this.reviewDiffNew.textContent = newLabel || ' ';
	}

	private setReviewBusy(busy: boolean): void {
		this.reviewBusy = busy;
		this.reviewApplyBtn.enabled = !busy;
		this.reviewDiscardBtn.enabled = !busy;
	}

	private async resolvePendingReview(accept: boolean): Promise<void> {
		if (!this.pendingReview || this.reviewBusy) {
			return;
		}
		const { proposalId, operation, notes } = this.pendingReview;
		this.setReviewBusy(true);
		try {
			if (accept) {
				await this.applier.apply(operation);
				await this.distributedModels.respondToProposal(proposalId, true);
				this.appendEntry({
					id: 'proposal-edit-' + proposalId,
					kind: 'edit',
					text: this.editMemorySummary(operation, notes),
				});
			} else {
				await this.distributedModels.respondToProposal(proposalId, false);
				this.appendEntry({
					id: 'proposal-skipped-' + proposalId,
					kind: 'status',
					text: localize(
						'distributedModels.review.skipped',
						'Skipped change to {0}',
						operation.file,
					),
				});
			}
		} catch (err) {
			await this.distributedModels.respondToProposal(proposalId, false);
			this.logService.error('distributed-models: review resolution failed', err);
			this.appendEntry({
				id: 'proposal-error-' + proposalId,
				kind: 'error',
				text: `Failed to resolve proposal: ${String(err)}`,
			});
		} finally {
			this.teardownPendingReviewUi();
		}
	}

	private renderComposer(parent: HTMLElement): void {
		const composer = DOM.append(parent, DOM.$('div.dm-composer'));
		const shell = DOM.append(composer, DOM.$('div.dm-composer-shell'));

		const inputWrap = DOM.append(shell, DOM.$('div.dm-composer-input'));
		this.composerInput = this._register(
			new InputBox(inputWrap, this.contextViewService, {
				placeholder: localize(
					'distributedModels.input.placeholder',
					'Ask the agents to do something...',
				),
				flexibleHeight: true,
				flexibleMaxHeight: 240,
				inputBoxStyles: defaultInputBoxStyles,
			}),
		);

		const footerRow = DOM.append(shell, DOM.$('div.dm-composer-meta'));
		this.contextMeterEl = DOM.append(
			footerRow,
			DOM.$('div.dm-context-meter'),
		) as HTMLDivElement;
		this.contextMeterIcon = DOM.append(
			this.contextMeterEl,
			DOM.$('div.dm-context-meter-icon'),
		) as HTMLDivElement;
		this.contextMeterText = DOM.append(
			this.contextMeterEl,
			DOM.$('span.dm-context-meter-text'),
		) as HTMLSpanElement;

		this.runRowEl = DOM.append(footerRow, DOM.$('div.dm-run-row.dm-hidden'));
		const runLabel = DOM.append(this.runRowEl, DOM.$('span.dm-run-label'));
		runLabel.textContent = localize(
			'distributedModels.run.running',
			'Running',
		);
		this.stopRunButton = this._register(
			new Button(this.runRowEl, {
				title: localize(
					'distributedModels.run.stop',
					'Stop',
				),
				supportIcons: true,
				secondary: true,
				...defaultButtonStyles,
			}),
		);
		this.stopRunButton.label = '$(debug-stop)';
		this.stopRunButton.element.classList.add('dm-stop-run-button');
		this._register(this.stopRunButton.onDidClick(() => this.requestStopJob()));

		this.sendButton = this._register(
			new Button(footerRow, {
				title: localize('distributedModels.send', 'Send message'),
				supportIcons: true,
				secondary: true,
				...defaultButtonStyles,
			}),
		);
		this.sendButton.label = '$(arrow-up)';
		this.sendButton.element.classList.add('dm-send-button');
		this._register(this.sendButton.onDidClick(() => this.submit()));

		this._localDispose.add(
			DOM.addDisposableListener(
				inputWrap,
				'keydown',
				(event: KeyboardEvent) => {
					const shouldSend =
						event.key === 'Enter' &&
						!event.shiftKey &&
						!event.altKey &&
						!event.ctrlKey &&
						!event.metaKey &&
						!event.isComposing;
					if (shouldSend) {
						event.preventDefault();
						event.stopPropagation();
						void this.submit();
					}
				},
				true,
			),
		);
		this._register(
			this.composerInput.onDidChange(() => this.updateContextMeter()),
		);
	}

	private renderModelSettings(parent: HTMLElement): void {
		this.modelSettingsEl = DOM.append(
			parent,
			DOM.$('div.dm-model-settings.dm-hidden'),
		) as HTMLDivElement;

		const header = DOM.append(this.modelSettingsEl, DOM.$('div.dm-model-header'));
		const backButton = this._register(
			new Button(header, {
				title: localize('distributedModels.models.back', 'Back to chat'),
				secondary: true,
				supportIcons: true,
				...defaultButtonStyles,
			}),
		);
		backButton.label = '$(arrow-left)';
		backButton.element.classList.add('dm-icon-button');
		this._register(backButton.onDidClick(() => this.toggleModelSettings(false)));

		const title = DOM.append(header, DOM.$('div.dm-model-title'));
		title.textContent = localize('distributedModels.models.title', 'Local models');

		const subtitle = DOM.append(this.modelSettingsEl, DOM.$('div.dm-model-subtitle'));
		subtitle.textContent = localize(
			'distributedModels.models.subtitle',
			'Per-agent Ollama model assignments. Saved to distributed-models.yaml.',
		);

		const grid = DOM.append(this.modelSettingsEl, DOM.$('div.dm-model-grid'));

		for (const field of MODEL_FIELDS) {
			const row = DOM.append(grid, DOM.$('div.dm-model-row'));
			const label = DOM.append(row, DOM.$('div.dm-model-label'));
			label.textContent = field.label;

			const input = this._register(
				new InputBox(row, this.contextViewService, {
					placeholder: 'qwen2.5-coder:7b',
					inputBoxStyles: defaultInputBoxStyles,
				}),
			);
			this.modelInputs[field.key] = input;
		}

		const actions = DOM.append(this.modelSettingsEl, DOM.$('div.dm-model-actions'));
		this.saveModelsButton = this._register(
			new Button(actions, {
				title: localize('distributedModels.models.save', 'Save Models'),
				supportIcons: false,
				...defaultButtonStyles,
			}),
		);
		this.saveModelsButton.label = localize(
			'distributedModels.models.save',
			'Save Models',
		);
		this._register(
			this.saveModelsButton.onDidClick(() => this.saveModelSettings()),
		);

		this.modelStatusEl = DOM.append(
			this.modelSettingsEl,
			DOM.$('div.dm-model-status'),
		) as HTMLDivElement;
		this.modelStatusEl.textContent = localize(
			'distributedModels.models.loading',
			'Loading backend model config...',
		);
	}

	private setStatusError(message: string, showRetry: boolean): void {
		DOM.clearNode(this.modelStatusEl);
		const text = DOM.append(this.modelStatusEl, DOM.$('span.dm-model-status-text'));
		text.textContent = message;
		this.modelStatusEl.classList.add('dm-model-status-error');

		if (showRetry) {
			if (!this.retryButton) {
				this.retryButton = this._register(
					new Button(this.modelStatusEl, {
						title: localize('distributedModels.models.retry', 'Retry'),
						secondary: true,
						...defaultButtonStyles,
					}),
				);
				this.retryButton.label = localize('distributedModels.models.retry', 'Retry');
				this._register(this.retryButton.onDidClick(() => this.loadModelSettings()));
			}
			this.modelStatusEl.appendChild(this.retryButton.element);
		}
	}

	private setStatusInfo(message: string): void {
		DOM.clearNode(this.modelStatusEl);
		this.modelStatusEl.classList.remove('dm-model-status-error');
		this.modelStatusEl.textContent = message;
	}

	private async loadModelSettings(): Promise<void> {
		this.setStatusInfo(
			localize(
				'distributedModels.models.loading',
				'Loading backend model config...',
			),
		);
		try {
			const config = await this.distributedModels.readConfig();
			for (const field of MODEL_FIELDS) {
				const input = this.modelInputs[field.key];
				if (input) {
					input.value = config.models[field.key] ?? '';
				}
			}
			this.modelsLoaded = true;
			void this.refreshContextBudgetFromBackend();
			this.setStatusInfo(
				localize(
					'distributedModels.models.loaded',
					'Loaded from distributed-models.yaml. Edit and Save to apply instantly.',
				),
			);
		} catch (err) {
			this.handleConfigError(err, 'load');
		}
	}

	private async saveModelSettings(): Promise<void> {
		const models: ModelAssignments = {
			orchestrator: this.modelInputs.orchestrator?.value.trim() ?? '',
			file_structure: this.modelInputs.file_structure?.value.trim() ?? '',
			code_writer: this.modelInputs.code_writer?.value.trim() ?? '',
			error_agent: this.modelInputs.error_agent?.value.trim() ?? '',
			review: this.modelInputs.review?.value.trim() ?? '',
			integration: this.modelInputs.integration?.value.trim() ?? '',
		};
		this.setStatusInfo(
			localize(
				'distributedModels.models.saving',
				'Saving model assignments...',
			),
		);
		try {
			const updated = await this.distributedModels.updateModels(models);
			for (const field of MODEL_FIELDS) {
				const input = this.modelInputs[field.key];
				if (input) {
					input.value = updated.models[field.key] ?? '';
				}
			}
			this.modelsLoaded = true;
			void this.refreshContextBudgetFromBackend();
			this.setStatusInfo(
				localize(
					'distributedModels.models.saved',
					'Saved. New chats use these models immediately.',
				),
			);
		} catch (err) {
			this.handleConfigError(err, 'save');
		}
	}

	private updateRunIndicator(): void {
		if (!this.runRowEl) {
			return;
		}
		this.runRowEl.classList.toggle('dm-hidden', !this.currentJobId);
	}

	private requestStopJob(): void {
		const jobId = this.currentJobId;
		if (!jobId) {
			return;
		}
		void this.distributedModels.cancelJob(jobId).catch((err) => {
			this.logService.warn('distributed-models: cancel request failed', err);
		});
	}

	private handleConfigError(err: unknown, op: 'load' | 'save'): void {
		const message = err instanceof Error ? err.message : String(err);
		const isFetchFailure =
			message.toLowerCase().includes('failed to fetch') ||
			message.toLowerCase().includes('networkerror') ||
			message.toLowerCase().includes('ecconrefused') ||
			message.toLowerCase().includes('econnrefused');
		const action = op === 'load' ? 'load' : 'save';
		const friendly = isFetchFailure
			? `Backend unreachable. Start it with \`make serve\` (or \`cargo run -- serve\`), then retry.`
			: `Could not ${action} models: ${message}`;
		this.setStatusError(friendly, true);
		this.logService.error(`distributed-models: failed to ${op} model config`, err);
	}

	/**
	 * When there is no opened folder, derive a canonical root URI from file-backed editors
	 * so `workspace_root` matches the merged snapshot WorkspaceFileWatcher pushed.
	 */
	private fallbackWorkspaceRootFromOpenEditors(): string | undefined {
		const roots = collectDirectoryRootsFromEditorService(this.editorService);
		return roots.length > 0 ? roots[0].toString(true) : undefined;
	}

	private async submit(): Promise<void> {
		const text = this.composerInput.value.trim();
		if (!text) {
			return;
		}
		const history = this.buildHistory();
		this.composerInput.value = '';
		this.appendEntry({
			id: `user-${Date.now()}`,
			kind: 'user',
			text,
		});
		this._onDidChangeBusy.fire(true);
		try {
			const workspaceRoot =
				this.workspaceContextService.getWorkspace().folders[0]?.uri.toString(true) ??
				this.fallbackWorkspaceRootFromOpenEditors();
			this.currentJobId = await this.distributedModels.sendChat(
				text,
				workspaceRoot,
				history,
			);
			this.updateRunIndicator();
		} catch (err) {
			this.logService.error(`distributed-models: chat failed`, err);
			this.appendEntry({
				id: `err-${Date.now()}`,
				kind: 'error',
				text: `Failed to send: ${String(err)}`,
			});
			this._onDidChangeBusy.fire(false);
		}
	}

	/**
	 * Pull effective context-window budget from `/config`. The orchestrator drives
	 * chat; backend queries Ollama `POST /api/show` when possible (`*.context_length`)
	 * and reports `context_window_effective = min(native, ollama_num_ctx)`.
	 */
	private async refreshContextBudgetFromBackend(): Promise<void> {
		try {
			const c = await this.distributedModels.readConfig();
			const eff = c.context_window_effective;
			const native = c.context_window_native;
			const req = c.ollama_num_ctx;
			if (typeof eff === 'number' && eff > 0) {
				this.contextBudgetEffective = eff;
			}
			this.contextOrchestratorLabel =
				c.models.orchestrator.trim() ||
				localize(
					'distributedModels.context.modelUnknown',
					'(orchestrator model)',
				);
			this.contextBudgetNative =
				typeof native === 'number' && native > 0 ? native : undefined;
			this.contextOllamaNumCtx =
				typeof req === 'number' && req > 0 ? req : FALLBACK_CONTEXT_WINDOW_TOKENS;
			this.contextMeterRefreshScheduler.schedule();
		} catch (err) {
			this.logService.warn(
				'distributed-models: keeping default context meter budget (backend unreachable or error)',
				err,
			);
		}
	}

	/**
	 * Convert the most recent transcript turns into the wire format the
	 * orchestrator expects. Status / log / proposal / error rows are
	 * skipped — they're UI noise, not conversation.
	 */
	private buildHistory(): ChatTurn[] {
		const turns: ChatTurn[] = [];
		for (const entry of this.transcript) {
			if (entry.kind === 'user') {
				turns.push({ role: 'user', text: entry.text });
			} else if (entry.kind === 'assistant' || entry.kind === 'edit') {
				turns.push({ role: 'assistant', text: entry.text });
			}
		}
		return turns.slice(-MAX_HISTORY_TURNS);
	}

	private onWorkbenchEditorsChanged(): void {
		this.editorModelListeners.clear();
		for (const control of this.editorService.visibleTextEditorControls) {
			for (const ed of expandWorkbenchEditorSides(control)) {
				const model = ed.getModel();
				if (!model) {
					continue;
				}
				const tm = model as ITextModel;
				if (typeof tm.getValue !== 'function' || tm.isDisposed()) {
					continue;
				}
				this.editorModelListeners.add(
					tm.onDidChangeContent(() =>
						this.contextMeterRefreshScheduler.schedule(),
					),
				);
			}
		}
		this.contextMeterRefreshScheduler.schedule();
	}

	/**
	 * Sum text in visible editor models (deduped by URI). This approximates
	 * code the user is actively working alongside chat when estimating context.
	 */
	private estimateVisibleEditorCodeChars(): {
		readonly chars: number;
		readonly bufferCount: number;
	} {
		const seen = new Set<string>();
		let chars = 0;
		let counted = 0;
		for (const control of this.editorService.visibleTextEditorControls) {
			if (counted >= CONTEXT_METER_MAX_VISIBLE_EDITORS) {
				break;
			}
			for (const ed of expandWorkbenchEditorSides(control)) {
				if (counted >= CONTEXT_METER_MAX_VISIBLE_EDITORS) {
					break;
				}
				const model = ed.getModel();
				if (!model) {
					continue;
				}
				const tm = model as ITextModel;
				if (typeof tm.getValue !== 'function' || tm.isDisposed()) {
					continue;
				}
				const key = tm.uri?.toString() ?? `scratch-${counted}-${seen.size}`;
				if (seen.has(key)) {
					continue;
				}
				seen.add(key);
				const text = tm.getValue();
				chars += Math.min(text.length, CONTEXT_METER_MAX_CHARS_PER_BUFFER);
				counted++;
			}
		}
		return { chars, bufferCount: seen.size };
	}

	private async handleEvent(event: ClientEvent): Promise<void> {
		if (this.currentJobId && event.job_id !== this.currentJobId) {
			return;
		}
		switch (event.type) {
			case 'agent_status':
				this.appendEntry({
					id: event.job_id + '-status-' + Date.now(),
					kind: 'status',
					agent: event.agent,
					text: event.status,
				});
				break;
			case 'log':
				this.appendEntry({
					id: event.job_id + '-log-' + Date.now(),
					kind: 'log',
					agent: event.agent,
					text: event.message,
				});
				break;
			case 'assistant_message':
				this.appendEntry({
					id: event.job_id + '-assistant-' + Date.now(),
					kind: 'assistant',
					text: event.text,
				});
				break;
			case 'prompt_estimate':
				this.recordBackendPromptEstimate(event.agent, event.approximate_tokens);
				break;
			case 'file_proposal':
				this.openPendingReview(event.proposal_id, event.operation, event.review_notes);
				break;
			case 'error':
				this.appendEntry({
					id: 'err-' + Date.now(),
					kind: 'error',
					text: event.message,
				});
				break;
			case 'job_complete':
				this._onDidChangeBusy.fire(false);
				if (event.job_id === this.currentJobId) {
					this.currentJobId = undefined;
					this.updateRunIndicator();
				}
				break;
		}
	}

	private proposalSummary(op: FileOperation, notes: string | null): string {
		const verb =
			op.action === FileAction.Create
				? 'Create'
				: op.action === FileAction.Edit
					? 'Edit'
					: 'Delete';
		return notes ? `${verb} ${op.file} — ${notes}` : `${verb} ${op.file}`;
	}

	private editMemorySummary(op: FileOperation, notes: string | null): string {
		const verb =
			op.action === FileAction.Create
				? 'Created'
				: op.action === FileAction.Edit
					? 'Updated'
					: 'Deleted';
		if (op.action === FileAction.Delete) {
			return notes
				? `Applied edit: ${verb} ${op.file}. Notes: ${notes}`
				: `Applied edit: ${verb} ${op.file}.`;
		}
		const size = op.content?.length ?? 0;
		return notes
			? `Applied edit: ${verb} ${op.file} (${size} chars). Notes: ${notes}`
			: `Applied edit: ${verb} ${op.file} (${size} chars).`;
	}

	private appendEntry(entry: TranscriptEntry): void {
		this.transcript.push(entry);
		if (this.transcript.length > MAX_TRANSCRIPT_ENTRIES) {
			this.transcript.splice(0, this.transcript.length - MAX_TRANSCRIPT_ENTRIES);
			this.rerender();
		} else {
			this.transcriptEl.appendChild(this.renderEntry(entry));
			this.transcriptEl.scrollTop = this.transcriptEl.scrollHeight;
		}
		this.persistTranscript();
		this.updateContextMeter();
	}

	private restoreTranscript(): void {
		const raw = this.storageService.get(
			TRANSCRIPT_STORAGE_KEY,
			StorageScope.WORKSPACE,
		);
		if (!raw) {
			return;
		}
		try {
			const parsed = JSON.parse(raw) as TranscriptEntry[];
			if (!Array.isArray(parsed)) {
				return;
			}
			// Only restore "stable" entries: user / assistant / status / log.
			// Open proposals and live errors are dropped because they only
			// make sense in the context of an in-flight job.
			const restorable = parsed.filter(
				(e) =>
					e &&
					typeof e.text === 'string' &&
					(e.kind === 'user' ||
						e.kind === 'assistant' ||
						e.kind === 'edit' ||
						e.kind === 'status' ||
						e.kind === 'log'),
			);
			for (const entry of restorable) {
				this.transcript.push(entry);
				this.transcriptEl.appendChild(this.renderEntry(entry));
			}
			this.transcriptEl.scrollTop = this.transcriptEl.scrollHeight;
		} catch (err) {
			this.logService.warn(
				'distributed-models: failed to restore transcript, ignoring',
				err,
			);
		}
	}

	private persistTranscript(): void {
		try {
			// Only persist serialisable, "stable" rows. Skip open proposals
			// since the proposal_id is no longer valid after a restart.
			const persistable = this.transcript
				.filter(
					(e) =>
						e.kind === 'user' ||
						e.kind === 'assistant' ||
						e.kind === 'edit' ||
						e.kind === 'status' ||
						e.kind === 'log',
				)
				.map((e) => ({ id: e.id, kind: e.kind, text: e.text, agent: e.agent }));
			this.storageService.store(
				TRANSCRIPT_STORAGE_KEY,
				JSON.stringify(persistable),
				StorageScope.WORKSPACE,
				StorageTarget.MACHINE,
			);
		} catch (err) {
			this.logService.warn(
				'distributed-models: failed to persist transcript',
				err,
			);
		}
	}

	private persistBackendPromptMemory(): void {
		try {
			this.storageService.store(
				BACKEND_PROMPT_STORAGE_KEY,
				JSON.stringify(this.backendPromptMemory),
				StorageScope.WORKSPACE,
				StorageTarget.MACHINE,
			);
		} catch (err) {
			this.logService.warn(
				'distributed-models: failed to persist backend prompt stats',
				err,
			);
		}
	}

	private restoreBackendPromptMemory(): void {
		try {
			const raw = this.storageService.get(
				BACKEND_PROMPT_STORAGE_KEY,
				StorageScope.WORKSPACE,
			);
			if (!raw) {
				return;
			}
			const parsed = JSON.parse(raw) as {
				lastApprox?: unknown;
				peakApprox?: unknown;
				lastAgent?: unknown;
			};
			if (typeof parsed.lastApprox !== 'number' || typeof parsed.peakApprox !== 'number') {
				return;
			}
			const lastAgent =
				typeof parsed.lastAgent === 'string'
					? (parsed.lastAgent as AgentLabel)
					: undefined;
			this.backendPromptMemory = {
				lastApprox: Math.max(0, Math.floor(parsed.lastApprox)),
				peakApprox: Math.max(0, Math.floor(parsed.peakApprox)),
				lastAgent,
			};
		} catch (err) {
			this.logService.warn(
				'distributed-models: failed to restore backend prompt stats',
				err,
			);
		}
	}

	private recordBackendPromptEstimate(
		agent: AgentLabel,
		approximateTokens: number,
	): void {
		const approx = Math.max(0, Math.floor(approximateTokens));
		this.backendPromptMemory = {
			lastApprox: approx,
			peakApprox: Math.max(this.backendPromptMemory.peakApprox, approx),
			lastAgent: agent,
		};
		this.persistBackendPromptMemory();
		this.contextMeterRefreshScheduler.schedule();
	}

	/**
	 * Re-paint the context-window indicator next to the composer.
	 *
	 * Sidebar count uses transcript + composer + visible editors (`chars ÷ 4`);
	 * the bar compares that against the orchestrator budget. Persisted backend
	 * counts come from SSE `prompt_estimate` (≈ UTF-8 bytes ÷ 4 each Ollama call).
	 */
	private updateContextMeter(): void {
		if (!this.contextMeterEl) {
			return;
		}
		const draft = this.composerInput?.value ?? '';
		const historyChars = this.buildHistory().reduce(
			(acc, turn) => acc + turn.text.length,
			0,
		);
		const { chars: editorChars, bufferCount } =
			this.estimateVisibleEditorCodeChars();
		const chatChars = historyChars + draft.length;
		const totalChars = chatChars + editorChars;
		const tokens = Math.ceil(totalChars / 4);
		const chatTokens = Math.ceil(chatChars / 4);
		const editorTokens = Math.ceil(editorChars / 4);
		const budget = Math.max(1, this.contextBudgetEffective);
		const fraction = Math.min(1, tokens / budget);
		const percent = Math.round(fraction * 100);

		const bk = this.backendPromptMemory;

		this.contextMeterIcon.style.setProperty('--dm-fill', `${percent}%`);
		this.contextMeterEl.classList.toggle('dm-context-meter-warn', fraction >= 0.75);
		this.contextMeterEl.classList.toggle('dm-context-meter-full', fraction >= 0.95);
		if (bk.lastApprox > 0) {
			this.contextMeterText.textContent = localize(
				'distributedModels.context.meterLineWithBackend',
				'{0} / {1} · sidebar · bk ~{2} (max ~{3})',
				formatTokens(tokens),
				formatTokens(budget),
				formatTokens(bk.lastApprox),
				formatTokens(bk.peakApprox),
			);
		} else {
			this.contextMeterText.textContent = localize(
				'distributedModels.context.meterLine',
				'{0} / {1} · sidebar',
				formatTokens(tokens),
				formatTokens(budget),
			);
		}
		const nativeLabel =
			this.contextBudgetNative !== undefined
				? this.contextBudgetNative.toLocaleString()
				: localize('distributedModels.context.nativeUnknown', 'unknown');
		const orch = this.contextOrchestratorLabel;
		const sidebarDetail = localize(
			'distributedModels.context.sidebarDetail',
			'~{0} sidebar tokens (~{1} transcript+composer draft, ~{2} from {3} visible editor buffers); heuristic chars ÷ 4.',
			tokens.toLocaleString(),
			chatTokens.toLocaleString(),
			editorTokens.toLocaleString(),
			bufferCount,
		);
		const backendDetail =
			bk.lastApprox > 0
				? localize(
						'distributedModels.context.backendDetail',
						'Workspace-persisted backend prompt (SSE `prompt_estimate`): last ~{0} tok ({1}); peak ~{2}. Same heuristic as server.',
						bk.lastApprox.toLocaleString(),
						bk.lastAgent ? formatAgentLabel(bk.lastAgent) : '—',
						bk.peakApprox.toLocaleString(),
				  )
				: localize(
						'distributedModels.context.backendDetailNone',
						'No backend estimate in this workspace yet — run any agent chat job with a DM server that emits `prompt_estimate`.',
				  );
		const capExplain = localize(
			'distributedModels.context.effectiveCapExplain',
			'Bar denominator: orchestrator `{0}` — effective {1} tokens (min native {2}, backend num_ctx {3}). Native comes from Ollama /api/show when reachable.',
			orch,
			budget.toLocaleString(),
			nativeLabel,
			this.contextOllamaNumCtx.toLocaleString(),
		);
		this.contextMeterEl.title = `${localize(
			'distributedModels.context.titlePrefix',
			'Sidebar context estimate',
		)} — ${sidebarDetail} ${backendDetail} ${capExplain}`;
	}

	private async copyPlainText(text: string): Promise<void> {
		try {
			await this.clipboardService.writeText(text);
		} catch (err) {
			this.logService.warn(
				'distributed-models: clipboard write failed',
				err,
			);
		}
	}

	private appendEntryCopyControl(target: HTMLElement, text: string): void {
		const label = localize('distributedModels.copy', 'Copy');
		const btn = DOM.append(target, DOM.$('button.dm-entry-copy')) as HTMLButtonElement;
		btn.type = 'button';
		btn.textContent = label;
		btn.title = label;
		btn.setAttribute('aria-label', label);
		btn.addEventListener('click', (ev: MouseEvent) => {
			ev.preventDefault();
			ev.stopPropagation();
			void this.copyPlainText(text);
		});
	}

	private rerender(): void {
		while (this.transcriptEl.firstChild) {
			this.transcriptEl.removeChild(this.transcriptEl.firstChild);
		}
		for (const entry of this.transcript) {
			this.transcriptEl.appendChild(this.renderEntry(entry));
		}
	}

	private renderEntry(entry: TranscriptEntry): HTMLElement {
		const el = DOM.$(`div.dm-entry.dm-entry-${entry.kind}`);
		const wantsCopy =
			entry.kind === 'user' ||
			entry.kind === 'assistant' ||
			entry.kind === 'edit' ||
			entry.kind === 'error';

		if (isCollapsedAgentEntry(entry) && entry.agent) {
			el.classList.add('dm-entry-collapsed-agent');
			const row = DOM.append(el, DOM.$('div.dm-collapsed-row'));
			const dropdown = DOM.append(row, DOM.$('details.dm-agent-dropdown'));
			const summary = DOM.$('summary.dm-agent-dropdown-summary');
			summary.textContent = `${formatAgentLabel(entry.agent)} updates`;
			dropdown.appendChild(summary);
			const content = DOM.$('div.dm-agent-dropdown-content');
			content.textContent = entry.text;
			dropdown.appendChild(content);
			this.appendEntryCopyControl(row, entry.text);
			return el;
		}

		const text = DOM.$('div.dm-text');
		text.textContent = entry.text;

		if (!(wantsCopy || entry.agent)) {
			el.appendChild(text);
			return el;
		}

		const head = DOM.append(el, DOM.$('div.dm-entry-head'));
		const main = DOM.append(head, DOM.$('div.dm-entry-head-main'));
		if (entry.agent) {
			main.appendChild(DOM.$('span.dm-tag', undefined, entry.agent));
		}
		if (wantsCopy) {
			this.appendEntryCopyControl(head, entry.text);
		}
		el.appendChild(text);
		return el;
	}

	override layoutBody(height: number, width: number): void {
		super.layoutBody(height, width);
		this.rootEl.style.height = `${height}px`;
		this.rootEl.style.width = `${width}px`;
	}
}

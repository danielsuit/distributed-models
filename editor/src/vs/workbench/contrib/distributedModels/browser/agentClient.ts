/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { Emitter, Event } from '../../../../base/common/event.js';
import {
	Disposable,
	IDisposable,
	toDisposable,
} from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import {
	IDistributedModelsService,
	STORAGE_KEY_BACKEND_URL,
} from '../common/distributedModels.js';
import {
	ChatRequest,
	ChatResponse,
	ChatTurn,
	ClientEvent,
	DiagnosticEntry,
	FileChange,
	FileEntry,
	ModelAssignments,
	RuntimeConfigResponse,
} from '../common/types.js';

/**
 * Default base URL for the Rust backend. Overrideable via the
 * `distributedModels.backendUrl` setting so power users can run the daemon
 * remotely.
 */
export const DEFAULT_BASE_URL = 'http://127.0.0.1:3000';

/**
 * Concrete client. Owns one EventSource that fans events out through an
 * Emitter so multiple sidebar views can listen at once.
 */
export class AgentClient extends Disposable implements IDistributedModelsService {
	declare readonly _serviceBrand: undefined;

	private readonly _onEvent = this._register(new Emitter<ClientEvent>());
	readonly onEvent: Event<ClientEvent> = this._onEvent.event;

	private _eventSource: EventSource | undefined;

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

	connect(): IDisposable {
		this.openStream();
		return toDisposable(() => this.closeStream());
	}

	private openStream(): void {
		this.closeStream();
		const url = `${this.baseUrl}/events`;
		try {
			this._eventSource = new EventSource(url);
		} catch (err) {
			this.logService.error(`distributed-models: failed to open ${url}`, err);
			return;
		}
		this._eventSource.onmessage = (msg) => {
			try {
				const parsed = JSON.parse(msg.data) as ClientEvent;
				this._onEvent.fire(parsed);
			} catch (err) {
				this.logService.warn(
					`distributed-models: dropped malformed event: ${String(err)} :: ${msg.data}`,
				);
			}
		};
		this._eventSource.onerror = () => {
			this.logService.warn('distributed-models: SSE stream error, will retry');
			this.closeStream();
			setTimeout(() => this.openStream(), 2000);
		};
	}

	private closeStream(): void {
		this._eventSource?.close();
		this._eventSource = undefined;
	}

	async sendChat(
		text: string,
		workspaceRoot?: string,
		history?: ChatTurn[],
	): Promise<string> {
		const body: ChatRequest = {
			text,
			workspace_root: workspaceRoot
				? this.normaliseWorkspace(workspaceRoot)
				: workspaceRoot,
			history: history ?? [],
		};
		const response = await this.post('/chat', body);
		const parsed = (await response.json()) as ChatResponse;
		return parsed.job_id;
	}

	async pushSnapshot(workspaceRoot: string, files: FileEntry[]): Promise<void> {
		await this.post('/file-snapshot', {
			workspace_root: this.normaliseWorkspace(workspaceRoot),
			files,
		});
	}

	async pushChange(workspaceRoot: string, change: FileChange): Promise<void> {
		await this.post('/file-change', {
			workspace_root: this.normaliseWorkspace(workspaceRoot),
			change,
		});
	}

	async pushDiagnostics(
		workspaceRoot: string,
		diagnostics: DiagnosticEntry[],
	): Promise<void> {
		await this.post('/diagnostics', {
			workspace_root: this.normaliseWorkspace(workspaceRoot),
			diagnostics,
		});
	}

	async respondToProposal(proposalId: string, accepted: boolean): Promise<void> {
		await this.post(`/proposal/${encodeURIComponent(proposalId)}`, { accepted });
	}

	async cancelJob(jobId: string): Promise<void> {
		await this.post(`/job/${encodeURIComponent(jobId)}/cancel`, {});
	}

	async readConfig(): Promise<RuntimeConfigResponse> {
		const response = await fetch(`${this.baseUrl}/config`);
		if (!response.ok) {
			const detail = await response.text().catch(() => '');
			throw new Error(`GET /config failed (${response.status}): ${detail}`);
		}
		return (await response.json()) as RuntimeConfigResponse;
	}

	async updateModels(models: ModelAssignments): Promise<RuntimeConfigResponse> {
		const response = await this.post('/config/models', { models });
		return (await response.json()) as RuntimeConfigResponse;
	}

	private normaliseWorkspace(root: string): string {
		try {
			return URI.parse(root).fsPath;
		} catch {
			return root;
		}
	}

	private async post(path: string, body: unknown): Promise<Response> {
		const url = `${this.baseUrl}${path}`;
		const response = await fetch(url, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(body),
		});
		if (!response.ok) {
			const detail = await response.text().catch(() => '');
			throw new Error(`POST ${path} failed (${response.status}): ${detail}`);
		}
		return response;
	}
}

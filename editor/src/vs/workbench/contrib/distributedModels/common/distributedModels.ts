/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { Event } from '../../../../base/common/event.js';
import { IDisposable } from '../../../../base/common/lifecycle.js';
import { createDecorator } from '../../../../platform/instantiation/common/instantiation.js';
import {
	ChatTurn,
	ClientEvent,
	DiagnosticEntry,
	FileChange,
	FileEntry,
	ModelAssignments,
	RuntimeConfigResponse,
} from './types.js';

// Reuse Copilot's former panel slot so our chat opens exactly where
// users expect it from the default VS Code UX.
export const VIEW_CONTAINER_ID = 'workbench.panel.chat';
export const SIDEBAR_VIEW_ID = 'workbench.panel.chat.view.distributedModels';
export const STORAGE_KEY_BACKEND_URL = 'distributedModels.backendUrl';

export const IDistributedModelsService = createDecorator<IDistributedModelsService>(
	'distributedModelsService'
);

/**
 * Editor-side facade for the multi-agent backend. Implementations live in
 * `browser/agentClient.ts` and are wired in via the workbench DI container.
 */
export interface IDistributedModelsService {
	readonly _serviceBrand: undefined;

	/** Streams every event from any active job. */
	readonly onEvent: Event<ClientEvent>;

	/**
	 * Send a user chat message; resolves with the new job id.
	 * `history` carries recent conversation turns so the orchestrator
	 * has memory of the dialogue.
	 */
	sendChat(
		text: string,
		workspaceRoot?: string,
		history?: ChatTurn[],
	): Promise<string>;

	/** Push a complete workspace file snapshot. */
	pushSnapshot(workspaceRoot: string, files: FileEntry[]): Promise<void>;

	/** Push a single file change event. */
	pushChange(workspaceRoot: string, change: FileChange): Promise<void>;

	/** Push the latest diagnostics list. */
	pushDiagnostics(workspaceRoot: string, diagnostics: DiagnosticEntry[]): Promise<void>;

	/** Resolve a pending file proposal. */
	respondToProposal(proposalId: string, accepted: boolean): Promise<void>;

	/** Request cooperative cancellation for a running chat job. */
	cancelJob(jobId: string): Promise<void>;

	/** Read current runtime/backend config, including model assignments. */
	readConfig(): Promise<RuntimeConfigResponse>;

	/** Update model assignments used by all agents at runtime. */
	updateModels(models: ModelAssignments): Promise<RuntimeConfigResponse>;

	/** Subscribe to lifecycle events; returns a disposable that closes the SSE stream. */
	connect(): IDisposable;
}

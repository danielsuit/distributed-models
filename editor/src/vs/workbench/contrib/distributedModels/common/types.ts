/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

/**
 * Wire types shared between the agent client, file operations, and sidebar
 * UI. They mirror the JSON the Rust backend emits over REST and SSE.
 */

export const enum FileAction {
	Create = 'create',
	Edit = 'edit',
	Delete = 'delete'
}

export interface FileOperation {
	readonly action: FileAction;
	readonly file: string;
	readonly content?: string;
}

export interface FileEntry {
	readonly path: string;
	readonly size: number;
	readonly is_dir: boolean;
}

export interface FileChange {
	readonly kind: 'created' | 'changed' | 'deleted';
	readonly path: string;
}

export interface DiagnosticEntry {
	readonly file: string;
	readonly line: number;
	readonly column: number;
	readonly severity: string;
	readonly message: string;
	readonly source?: string;
}

export interface ChatTurn {
	readonly role: 'user' | 'assistant';
	readonly text: string;
}

export interface ChatRequest {
	readonly text: string;
	readonly workspace_root?: string;
	readonly history?: ChatTurn[];
}

export interface ChatResponse {
	readonly job_id: string;
}

export type AgentLabel =
	| 'orchestrator'
	| 'filestructure'
	| 'codewriter'
	| 'erroragent'
	| 'review'
	| 'integration'
	| 'client';

export type ClientEvent =
	| { type: 'agent_status'; job_id: string; agent: AgentLabel; status: string }
	| { type: 'log'; job_id: string; agent: AgentLabel; message: string }
	| { type: 'assistant_message'; job_id: string; text: string }
	| {
		type: 'file_proposal';
		job_id: string;
		proposal_id: string;
		operation: FileOperation;
		review_notes: string | null;
	}
	| {
			type: 'prompt_estimate';
			job_id: string;
			agent: AgentLabel;
			approximate_tokens: number;
	}
	| { type: 'error'; job_id: string; message: string }
	| { type: 'job_complete'; job_id: string };

export interface AgentClientConfig {
	readonly baseUrl: string;
}

export interface ModelAssignments {
	readonly orchestrator: string;
	readonly file_structure: string;
	readonly code_writer: string;
	readonly error_agent: string;
	readonly review: string;
	readonly integration: string;
}

export interface RuntimeConfigResponse {
	readonly host: string;
	readonly port: number;
	readonly redis_url: string;
	readonly ollama_endpoint: string;
	readonly models: ModelAssignments;
	/** Ollama generate `num_ctx`; each agent request uses at most this. */
	readonly ollama_num_ctx?: number;
	/** GGUF `*.context_length` from `POST /api/show` when Ollama returns it. */
	readonly context_window_native?: number;
	/** Sidebar meter denominator: `min(native, ollama_num_ctx)` vs chat + editors. */
	readonly context_window_effective?: number;
}

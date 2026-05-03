/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { Disposable } from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import { IMarkerService, MarkerSeverity } from '../../../../platform/markers/common/markers.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import {
	IWorkspaceContextService,
} from '../../../../platform/workspace/common/workspace.js';
import { IDistributedModelsService } from '../common/distributedModels.js';
import { DiagnosticEntry } from '../common/types.js';

const FLUSH_DEBOUNCE_MS = 500;

/**
 * Watches editor diagnostics (compiler errors, lint output, etc.) and pushes
 * the latest list to the Error Agent every time markers change. The Error
 * Agent reacts to "new errors" transitions and triggers an automated fix.
 */
export class DiagnosticsWatcher extends Disposable {
	private _flushTimer: ReturnType<typeof setTimeout> | undefined;

	constructor(
		@IMarkerService private readonly markerService: IMarkerService,
		@IWorkspaceContextService private readonly workspace: IWorkspaceContextService,
		@ILogService private readonly logService: ILogService,
		@IDistributedModelsService
		private readonly distributedModels: IDistributedModelsService,
	) {
		super();
		this._register(
			this.markerService.onMarkerChanged(() => this.scheduleFlush()),
		);
	}

	/** Force an immediate push, e.g. on activation. */
	async flushNow(): Promise<void> {
		this.cancelTimer();
		await this.flush();
	}

	private scheduleFlush(): void {
		this.cancelTimer();
		this._flushTimer = setTimeout(() => {
			void this.flush().catch((err) =>
				this.logService.warn(
					`distributed-models: diagnostics flush failed: ${String(err)}`,
				),
			);
		}, FLUSH_DEBOUNCE_MS);
	}

	private cancelTimer(): void {
		if (this._flushTimer) {
			clearTimeout(this._flushTimer);
			this._flushTimer = undefined;
		}
	}

	private async flush(): Promise<void> {
		const folders = this.workspace.getWorkspace().folders;
		if (folders.length === 0) {
			return;
		}
		const root = folders[0].uri;
		const markers = this.markerService.read();
		const diagnostics: DiagnosticEntry[] = markers.map((marker) => ({
			file: this.relative(root, marker.resource),
			line: marker.startLineNumber,
			column: marker.startColumn,
			severity: this.severityToString(marker.severity),
			message: marker.message,
			source: marker.source ?? undefined,
		}));
		await this.distributedModels.pushDiagnostics(root.toString(), diagnostics);
	}

	private severityToString(severity: MarkerSeverity): string {
		switch (severity) {
			case MarkerSeverity.Error:
				return 'error';
			case MarkerSeverity.Warning:
				return 'warning';
			case MarkerSeverity.Info:
				return 'info';
			case MarkerSeverity.Hint:
				return 'hint';
			default:
				return 'unknown';
		}
	}

	private relative(root: URI, resource: URI): string {
		if (resource.scheme !== root.scheme || resource.authority !== root.authority) {
			return resource.toString();
		}
		if (!resource.path.startsWith(root.path)) {
			return resource.path;
		}
		const rootPath = root.path.replace(/\/+$/, '') + '/';
		return resource.path.startsWith(rootPath)
			? resource.path.slice(rootPath.length)
			: resource.path.slice(root.path.length).replace(/^\/+/, '');
	}

	override dispose(): void {
		this.cancelTimer();
		super.dispose();
	}
}

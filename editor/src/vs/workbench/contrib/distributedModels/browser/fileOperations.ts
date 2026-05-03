/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { VSBuffer } from '../../../../base/common/buffer.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import {
	IBulkEditService,
	ResourceFileEdit,
	ResourceTextEdit,
} from '../../../../editor/browser/services/bulkEditService.js';
import { Range } from '../../../../editor/common/core/range.js';
import { IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { IFileService } from '../../../../platform/files/common/files.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import { INotificationService, Severity } from '../../../../platform/notification/common/notification.js';
import {
	IWorkspaceContextService,
} from '../../../../platform/workspace/common/workspace.js';
import { FileAction, FileOperation } from '../common/types.js';

const BULK_EDIT_LABEL = 'Distributed Models';

/**
 * Result of presenting a proposal to the user.
 */
export interface ProposalDecision {
	readonly accepted: boolean;
	readonly reason?: string;
}

/**
 * Applies the JSON file operations the agents return. This is the key piece
 * that lets non-tool-using local models still write files: the model returns
 * `{action, file, content}`, the editor parses it, the user accepts, and we
 * use VS Code's `IFileService` to perform the actual write.
 */
export class FileOperationsApplier extends Disposable {
	constructor(
		@IFileService private readonly fileService: IFileService,
		@IBulkEditService private readonly bulkEditService: IBulkEditService,
		@IWorkspaceContextService
		private readonly workspaceContextService: IWorkspaceContextService,
		@IDialogService private readonly dialogService: IDialogService,
		@INotificationService private readonly notificationService: INotificationService,
		@ILogService private readonly logService: ILogService,
	) {
		super();
	}

	/**
	 * Confirm a proposal with the user (Accept / Reject + optional diff).
	 * Implementations of the sidebar can also call directly into `apply` if
	 * they handle their own UI; this helper exists for plain commands.
	 */
	async confirmAndApply(operation: FileOperation): Promise<ProposalDecision> {
		const decision = await this.confirm(operation);
		if (!decision.accepted) {
			return decision;
		}
		try {
			await this.apply(operation);
			this.notificationService.info(
				this.localizeApplied(operation),
			);
			return { accepted: true };
		} catch (err) {
			this.logService.error('distributed-models: apply failed', err);
			this.notificationService.error(
				`Failed to apply ${operation.action} for ${operation.file}: ${String(err)}`,
			);
			return { accepted: false, reason: String(err) };
		}
	}

	/**
	 * Apply an operation without prompting. Throws on failure so callers can
	 * decide how to surface the error. Always uses workspace-relative paths.
	 */
	async apply(operation: FileOperation): Promise<void> {
		const target = this.resolve(operation.file);
		const bulkOpts = { label: BULK_EDIT_LABEL, showPreview: false as const };
		switch (operation.action) {
			case FileAction.Create: {
				const content = operation.content ?? '';
				await this.bulkEditService.apply(
					[
						new ResourceFileEdit(undefined, target, {
							overwrite: true,
							contents: Promise.resolve(VSBuffer.fromString(content)),
						}),
					],
					bulkOpts,
				);
				return;
			}
			case FileAction.Edit: {
				const content = operation.content ?? '';
				const exists = await this.fileService.exists(target);
				if (!exists) {
					await this.bulkEditService.apply(
						[
							new ResourceFileEdit(undefined, target, {
								overwrite: true,
								contents: Promise.resolve(VSBuffer.fromString(content)),
							}),
						],
						bulkOpts,
					);
					return;
				}
				await this.bulkEditService.apply(
					[
						new ResourceTextEdit(target, {
							range: new Range(1, 1, Number.MAX_SAFE_INTEGER, 1),
							text: content,
						}),
					],
					bulkOpts,
				);
				return;
			}
			case FileAction.Delete: {
				await this.bulkEditService.apply(
					[new ResourceFileEdit(target, undefined, { recursive: false })],
					bulkOpts,
				);
				return;
			}
			default:
				throw new Error(`Unknown action ${(operation as FileOperation).action}`);
		}
	}

	/**
	 * Resolve a workspace-relative path to a real URI. Falls back to file://
	 * with the input verbatim when there is no active workspace.
	 */
	private resolve(relative: string): URI {
		const folders = this.workspaceContextService.getWorkspace().folders;
		const root = folders[0]?.uri;
		if (root) {
			return URI.joinPath(root, relative);
		}
		return URI.file(relative);
	}

	private async confirm(operation: FileOperation): Promise<ProposalDecision> {
		const verb =
			operation.action === FileAction.Create
				? 'Create'
				: operation.action === FileAction.Edit
					? 'Edit'
					: 'Delete';
		const result = await this.dialogService.confirm({
			message: `${verb} ${operation.file}?`,
			detail:
				operation.action === FileAction.Delete
					? `${operation.file} will be moved to the trash.`
					: `Distributed Models will write ${operation.content?.length ?? 0} bytes to ${operation.file}.`,
			primaryButton: 'Accept',
			cancelButton: 'Reject',
		});
		return { accepted: result.confirmed };
	}

	private localizeApplied(operation: FileOperation): string {
		switch (operation.action) {
			case FileAction.Create:
				return `Created ${operation.file}`;
			case FileAction.Edit:
				return `Updated ${operation.file}`;
			case FileAction.Delete:
				return `Deleted ${operation.file}`;
			default:
				return `Applied operation on ${operation.file}`;
		}
	}
}

// Tag the unused Severity import as referenced; future users of this module
// surface notifications through the same import.
void Severity;

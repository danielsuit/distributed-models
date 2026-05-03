/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { RunOnceScheduler } from '../../../../base/common/async.js';
import {
	Disposable,
	DisposableStore,
} from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import { FileChangeType, IFileService } from '../../../../platform/files/common/files.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import {
	IWorkspaceContextService,
	IWorkspaceFolder,
} from '../../../../platform/workspace/common/workspace.js';
import { IEditorService } from '../../../services/editor/common/editorService.js';
import { collectDirectoryRootsFromEditorService } from './editorRoots.js';
import { IDistributedModelsService } from '../common/distributedModels.js';
import { FileEntry } from '../common/types.js';

/**
 * Streams workspace file changes into the File Structure agent. We push a
 * full snapshot once at startup (so the agent has the entire tree) and then
 * forward incremental file events.
 *
 * Multi-root workspaces send one merged snapshot (prefixing folder names) so
 * later folder walks do not erase earlier ones. When no folder is open we
 * still index directory trees around file-backed editors.
 */
export class WorkspaceFileWatcher extends Disposable {
	private readonly _watchers = new DisposableStore();
	private readonly _adhocRefresh = this._register(
		new RunOnceScheduler(() => void this.refreshAdhocIfNeeded(), 450),
	);
	private _initialised = false;
	private _lastAdhocKey = '';

	constructor(
		@IWorkspaceContextService private readonly workspace: IWorkspaceContextService,
		@IFileService private readonly fileService: IFileService,
		@ILogService private readonly logService: ILogService,
		@IEditorService private readonly editorService: IEditorService,
		@IDistributedModelsService
		private readonly distributedModels: IDistributedModelsService,
	) {
		super();
		this._register(this._watchers);
		this._register(
			this.workspace.onDidChangeWorkspaceFolders(() => {
				void this.bootstrap().catch((err) =>
					this.logService.warn(
						`distributed-models: workspace re-bootstrap failed: ${String(err)}`,
					),
				);
			}),
		);
		this._register(
			this.editorService.onDidVisibleEditorsChange(() =>
				this._adhocRefresh.schedule(),
			),
		);
		this._register(
			this.editorService.onDidActiveEditorChange(() => this._adhocRefresh.schedule()),
		);
	}

	async start(): Promise<void> {
		await this.bootstrap();
	}

	/**
	 * When the editor has zero workspace folders, re-scan when the active file
	 * changes so indexing catches up quickly.
	 */
	private async refreshAdhocIfNeeded(): Promise<void> {
		if (this.workspace.getWorkspace().folders.length !== 0) {
			return;
		}
		const roots = collectDirectoryRootsFromEditorService(this.editorService);
		const key = roots.map((u) => u.toString()).sort().join('\0');
		if (this._initialised && key === this._lastAdhocKey) {
			return;
		}
		await this.bootstrap();
	}

	private async bootstrap(): Promise<void> {
		this._watchers.clear();
		const folders = this.workspace.getWorkspace().folders;
		try {
			if (folders.length > 0) {
				this._lastAdhocKey = '';
				await this.bootstrapWorkspaceFolders(folders);
			} else {
				await this.bootstrapFromOpenEditors();
			}
		} finally {
			this._initialised = true;
		}
	}

	private canonicalUriString(uri: URI): string {
		return uri.toString(true);
	}

	private async bootstrapWorkspaceFolders(
		folders: readonly IWorkspaceFolder[],
	): Promise<void> {
		const merged: FileEntry[] = [];
		const multi = folders.length > 1;
		for (const folder of folders) {
			const prefix = multi ? `${folder.name}/` : '';
			await this.walk(folder.uri, folder.uri, merged, prefix);
		}
		const primaryRoot = this.canonicalUriString(folders[0].uri);
		await this.distributedModels.pushSnapshot(primaryRoot, merged);
		this.logService.info(
			`distributed-models: merged snapshot ${primaryRoot} (${merged.length} entries from ${folders.length} folder(s))`,
		);
		for (const folder of folders) {
			const prefix = multi ? `${folder.name}/` : '';
			this._watchers.add(
				this.fileService.onDidFilesChange((event) => {
					const rawChanges =
						(
							event as {
								rawChanges?: Array<{ resource: URI; type: FileChangeType }>;
							}
						).rawChanges ?? [];
					for (const change of rawChanges) {
						const rel = this.relative(folder, change.resource);
						if (rel === undefined) {
							continue;
						}
						const kind =
							change.type === FileChangeType.ADDED
								? 'created'
								: change.type === FileChangeType.DELETED
									? 'deleted'
									: 'changed';
						const path = prefix + rel;
						this.distributedModels
							.pushChange(primaryRoot, { kind, path })
							.catch((err) =>
								this.logService.warn(
									`distributed-models: pushChange failed: ${String(err)}`,
								),
							);
					}
				}),
			);
		}
	}

	private async bootstrapFromOpenEditors(): Promise<void> {
		const roots = collectDirectoryRootsFromEditorService(this.editorService);
		this._lastAdhocKey = roots.map((u) => u.toString()).sort().join('\0');
		if (roots.length === 0) {
			this.logService.warn(
				'distributed-models: no workspace folder and no file-backed editors — file index empty; open a folder or save files to disk so agents can see the project.',
			);
			return;
		}
		const merged: FileEntry[] = [];
		const multi = roots.length > 1;
		for (let i = 0; i < roots.length; i++) {
			const root = roots[i];
			const prefix = multi ? `dir${i + 1}/` : '';
			await this.walk(root, root, merged, prefix);
		}
		const primaryRoot = this.canonicalUriString(roots[0]);
		await this.distributedModels.pushSnapshot(primaryRoot, merged);
		this.logService.info(
			`distributed-models: ad-hoc snapshot ${primaryRoot} (${merged.length} entries from ${roots.length} directory root(s), no workspace folder)`,
		);
		for (let i = 0; i < roots.length; i++) {
			const root = roots[i];
			const prefix = multi ? `dir${i + 1}/` : '';
			const syntheticFolder = { uri: root } as IWorkspaceFolder;
			this._watchers.add(
				this.fileService.onDidFilesChange((event) => {
					const rawChanges =
						(
							event as {
								rawChanges?: Array<{ resource: URI; type: FileChangeType }>;
							}
						).rawChanges ?? [];
					for (const change of rawChanges) {
						const rel = this.relative(syntheticFolder, change.resource);
						if (rel === undefined) {
							continue;
						}
						const kind =
							change.type === FileChangeType.ADDED
								? 'created'
								: change.type === FileChangeType.DELETED
									? 'deleted'
									: 'changed';
						const path = prefix + rel;
						this.distributedModels
							.pushChange(primaryRoot, { kind, path })
							.catch((err) =>
								this.logService.warn(
									`distributed-models: pushChange failed: ${String(err)}`,
								),
							);
					}
				}),
			);
		}
	}

	private async walk(
		root: URI,
		current: URI,
		out: FileEntry[],
		pathPrefix: string,
	): Promise<void> {
		try {
			const stat = await this.fileService.resolve(current);
			const rel = this.relative({ uri: root } as IWorkspaceFolder, current);
			if (rel === undefined) {
				return;
			}
			const path = pathPrefix ? `${pathPrefix}${rel}` : rel;
			out.push({
				path,
				size: stat.size ?? 0,
				is_dir: !!stat.isDirectory,
			});
			if (stat.isDirectory) {
				let childPaths: URI[] | undefined =
					stat.children?.length ?
						stat.children
							.filter((c) => !this.isIgnored(c.name))
							.map((c) => c.resource)
					:	undefined;
				if (!childPaths?.length) {
					/*
					 * Some file system providers omit `children` on the first
					 * resolve(). Re-resolve with `resolveTo: [current]` to
					 * force a one-level descent — `IFileService` does not
					 * expose `readdir` directly, so this is the supported
					 * way to get folder contents back.
					 */
					try {
						const detailed = await this.fileService.resolve(current, {
							resolveTo: [current],
						});
						childPaths = (detailed.children ?? [])
							.filter((c) => !this.isIgnored(c.name))
							.map((c) => c.resource);
					} catch (rdErr) {
						this.logService.warn(
							`distributed-models: resolve fallback failed for ${current.toString()}: ${String(rdErr)}`,
						);
					}
				}
				for (const childUri of childPaths ?? []) {
					await this.walk(root, childUri, out, pathPrefix);
				}
			}
		} catch (err) {
			this.logService.warn(
				`distributed-models: walk failed for ${current.toString()}: ${String(err)}`,
			);
		}
	}

	private relative(folder: IWorkspaceFolder, resource: URI): string | undefined {
		const root = folder.uri;
		if (resource.scheme !== root.scheme || resource.authority !== root.authority) {
			return undefined;
		}
		const rootPath = root.path.replace(/\/+$/, '') + '/';
		if (!resource.path.startsWith(root.path)) {
			return undefined;
		}
		if (resource.path === root.path) {
			return '.';
		}
		return resource.path.startsWith(rootPath)
			? resource.path.slice(rootPath.length)
			: resource.path.slice(root.path.length).replace(/^\/+/, '');
	}

	private isIgnored(name: string): boolean {
		return (
			name === '.git' ||
			name === 'node_modules' ||
			name === 'target' ||
			name === '.DS_Store' ||
			name.startsWith('.cache')
		);
	}

	get isInitialised(): boolean {
		return this._initialised;
	}
}

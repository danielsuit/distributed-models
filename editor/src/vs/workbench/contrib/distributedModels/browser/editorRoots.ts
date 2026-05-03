/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { dirname } from '../../../../base/common/resources.js';
import { URI } from '../../../../base/common/uri.js';
import { IEditor, IDiffEditor } from '../../../../editor/common/editorCommon.js';
import { IEditorService } from '../../../services/editor/common/editorService.js';

/**
 * Expand a workbench editor control (possibly a diff/composite editor) into
 * concrete editors that own a text model.
 */
export function expandWorkbenchEditorSides(control: IEditor | IDiffEditor): IEditor[] {
	const d = control as IEditor & {
		getModifiedEditor?: () => IEditor;
		getOriginalEditor?: () => IEditor;
	};
	if (
		typeof d.getModifiedEditor === 'function' &&
		typeof d.getOriginalEditor === 'function'
	) {
		return [d.getOriginalEditor(), d.getModifiedEditor()];
	}
	return [d];
}

/**
 * When there is no workspace folder, approximate "project roots" from file-backed
 * editors so the backend can receive a filesystem snapshot anyway.
 */
export function collectDirectoryRootsFromEditorService(editorService: IEditorService): URI[] {
	const dirs = new Map<string, URI>();
	const maybeAddFile = (u: URI | undefined) => {
		if (!u) {
			return;
		}
		/* Remote workspaces use vscode-remote URIs while still resolving to disk in the EH. */
		if (u.scheme !== 'file' && u.scheme !== 'vscode-remote') {
			return;
		}
		const dir = dirname(u);
		dirs.set(dir.toString(true), dir);
	};

	maybeAddFile(editorService.activeEditor?.resource);
	for (const control of editorService.visibleTextEditorControls) {
		for (const ed of expandWorkbenchEditorSides(control)) {
			// Diff editors expose their concrete sides via expandWorkbenchEditorSides.
			// On those concrete sides, getModel() returns ITextModel which has a
			// `.uri`, but the IEditor interface narrows to IEditorModel which
			// doesn't. Read it through a structural check so the typecheck is
			// satisfied without losing the URI when present.
			const model = ed.getModel() as { uri?: URI } | null | undefined;
			maybeAddFile(model?.uri);
		}
	}
	return [...dirs.values()];
}

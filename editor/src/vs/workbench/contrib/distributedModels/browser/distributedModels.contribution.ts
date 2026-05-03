/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Distributed Models Contributors.
 *  Licensed under the MIT License.
 *--------------------------------------------------------------------------------------------*/

import { Codicon } from '../../../../base/common/codicons.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import { localize, localize2 as nlsLocalize2 } from '../../../../nls.js';
import {
	registerWorkbenchContribution2,
	WorkbenchPhase,
	type IWorkbenchContribution,
} from '../../../common/contributions.js';
import {
	IConfigurationRegistry,
	Extensions as ConfigurationExtensions,
} from '../../../../platform/configuration/common/configurationRegistry.js';
import { ContextKeyExpr } from '../../../../platform/contextkey/common/contextkey.js';
import {
	IInstantiationService,
	ServicesAccessor,
} from '../../../../platform/instantiation/common/instantiation.js';
import {
	registerSingleton,
	InstantiationType,
} from '../../../../platform/instantiation/common/extensions.js';
import { MenuId, registerAction2 } from '../../../../platform/actions/common/actions.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { IViewsService } from '../../../services/views/common/viewsService.js';
import {
	Extensions as ViewContainerExtensions,
	IViewContainersRegistry,
	IViewsRegistry,
	ViewContainer,
	ViewContainerLocation,
} from '../../../common/views.js';
import { ViewPaneContainer } from '../../../browser/parts/views/viewPaneContainer.js';
import { ViewAction } from '../../../browser/parts/views/viewPane.js';
import { SyncDescriptor } from '../../../../platform/instantiation/common/descriptors.js';
import {
	IDistributedModelsService,
	SIDEBAR_VIEW_ID,
	STORAGE_KEY_BACKEND_URL,
	VIEW_CONTAINER_ID,
} from '../common/distributedModels.js';
import { AgentClient, DEFAULT_BASE_URL } from './agentClient.js';
import { DiagnosticsWatcher } from './diagnosticsWatcher.js';
import { WorkspaceFileWatcher } from './fileWatcher.js';
import {
	InlineCompletionsBackend,
	registerInlineCompletionProvider,
} from './inlineCompletions.js';
import { DistributedModelsSidebar } from './sidebar.js';
import { ILanguageFeaturesService } from '../../../../editor/common/services/languageFeatures.js';

// ---------------------------------------------------------------------------
// Service registration
// ---------------------------------------------------------------------------
registerSingleton(IDistributedModelsService, AgentClient, InstantiationType.Delayed);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
const configurationRegistry = Registry.as<IConfigurationRegistry>(
	ConfigurationExtensions.Configuration,
);

configurationRegistry.registerConfiguration({
	id: 'distributedModels',
	order: 1000,
	type: 'object',
	title: localize('distributedModels.title', 'Distributed Models'),
	properties: {
		[STORAGE_KEY_BACKEND_URL]: {
			type: 'string',
			default: DEFAULT_BASE_URL,
			description: localize(
				'distributedModels.backendUrl.description',
				'Base URL of the Distributed Models Rust backend.',
			),
		},
	},
});

// Distributed Models is its own product surface: hide the upstream welcome
// page / walkthroughs / chat onboarding so users land on a clean editor.
configurationRegistry.registerDefaultConfigurations([
	{
		overrides: {
			'workbench.startupEditor': 'none',
			'workbench.welcomePage.walkthroughs.openOnInstall': false,
			'workbench.tips.enabled': false,
			'workbench.welcomePage.experimentalOnboarding': false,
			'chat.setupFromDialog': false,
		},
	},
]);

// ---------------------------------------------------------------------------
// View container + view
// ---------------------------------------------------------------------------
const viewContainerRegistry = Registry.as<IViewContainersRegistry>(
	ViewContainerExtensions.ViewContainersRegistry,
);
const VIEW_CONTAINER: ViewContainer = viewContainerRegistry.registerViewContainer(
	{
		id: VIEW_CONTAINER_ID,
		title: localize2('distributedModels.title', 'Distributed Models'),
		ctorDescriptor: new SyncDescriptor(ViewPaneContainer, [
			VIEW_CONTAINER_ID,
			{ mergeViewWithContainerWhenSingleView: true },
		]),
		hideIfEmpty: false,
		icon: Codicon.commentDiscussion,
		order: 7,
	},
	ViewContainerLocation.Panel,
	{ doNotRegisterOpenCommand: false, isDefault: false },
);

Registry.as<IViewsRegistry>(ViewContainerExtensions.ViewsRegistry).registerViews(
	[
		{
			id: SIDEBAR_VIEW_ID,
			name: localize2('distributedModels.chat', 'Distributed Models'),
			ctorDescriptor: new SyncDescriptor(DistributedModelsSidebar),
			canToggleVisibility: false,
			canMoveView: true,
			containerIcon: Codicon.commentDiscussion,
			when: undefined,
		},
	],
	VIEW_CONTAINER,
);


const COPILOT_CHAT_VIEW_ID = 'workbench.panel.chat.view.copilot';

// Remove the built-in Copilot chat view from this panel container so users
// see only Distributed Models in the panel list.
const existingCopilotView = Registry.as<IViewsRegistry>(
	ViewContainerExtensions.ViewsRegistry,
).getView(COPILOT_CHAT_VIEW_ID);
if (existingCopilotView) {
	Registry.as<IViewsRegistry>(ViewContainerExtensions.ViewsRegistry).deregisterViews(
		[existingCopilotView],
		VIEW_CONTAINER,
	);
}

// ---------------------------------------------------------------------------
// View-title actions
// ---------------------------------------------------------------------------
const OPEN_MODEL_SETTINGS_ID = 'distributedModels.openModelSettings';
const CLEAR_CHAT_ID = 'distributedModels.clearChat';
const COPY_ENTIRE_CHAT_ID = 'distributedModels.copyEntireChat';
const FOCUS_REVIEW_ID = 'distributedModels.focusPendingReview';

registerAction2(
	class ClearChat extends ViewAction<DistributedModelsSidebar> {
		constructor() {
			super({
				viewId: SIDEBAR_VIEW_ID,
				id: CLEAR_CHAT_ID,
				title: nlsLocalize2('distributedModels.clearChat', 'Clear Chat'),
				f1: true,
				icon: Codicon.trash,
				menu: {
					id: MenuId.ViewTitle,
					group: 'navigation',
					when: ContextKeyExpr.equals('view', SIDEBAR_VIEW_ID),
					order: 0,
				},
			});
		}

		runInView(_accessor: ServicesAccessor, view: DistributedModelsSidebar): void {
			view.clearChat();
		}
	},
);

registerAction2(
	class CopyEntireChat extends ViewAction<DistributedModelsSidebar> {
		constructor() {
			super({
				viewId: SIDEBAR_VIEW_ID,
				id: COPY_ENTIRE_CHAT_ID,
				title: nlsLocalize2(
					'distributedModels.copyEntireChat',
					'Copy entire chat',
				),
				f1: true,
				icon: Codicon.copy,
				menu: {
					id: MenuId.ViewTitle,
					group: 'navigation',
					when: ContextKeyExpr.equals('view', SIDEBAR_VIEW_ID),
					order: 1,
				},
			});
		}

		runInView(_accessor: ServicesAccessor, view: DistributedModelsSidebar): void {
			view.copyEntireChatPlainText();
		}
	},
);

registerAction2(
	class FocusPendingReview extends ViewAction<DistributedModelsSidebar> {
		constructor() {
			super({
				viewId: SIDEBAR_VIEW_ID,
				id: FOCUS_REVIEW_ID,
				title: nlsLocalize2('distributedModels.focusReview', 'Review changes'),
				f1: true,
				icon: Codicon.diff,
				menu: {
					id: MenuId.ViewTitle,
					group: 'navigation',
					when: ContextKeyExpr.equals('view', SIDEBAR_VIEW_ID),
					order: 2,
				},
			});
		}

		runInView(_accessor: ServicesAccessor, view: DistributedModelsSidebar): void {
			view.focusPendingReview();
		}
	},
);

registerAction2(
	class OpenModelSettings extends ViewAction<DistributedModelsSidebar> {
		constructor() {
			super({
				viewId: SIDEBAR_VIEW_ID,
				id: OPEN_MODEL_SETTINGS_ID,
				title: nlsLocalize2(
					'distributedModels.openModelSettings',
					'Configure Agent Models',
				),
				f1: true,
				icon: Codicon.gear,
				menu: {
					id: MenuId.ViewTitle,
					group: 'navigation',
					when: ContextKeyExpr.equals('view', SIDEBAR_VIEW_ID),
					order: 3,
				},
			});
		}

		runInView(_accessor: ServicesAccessor, view: DistributedModelsSidebar): void {
			view.toggleModelSettings();
		}
	},
);

// ---------------------------------------------------------------------------
// Boot contribution: starts the file/diagnostic watchers as soon as the
// workbench is ready, so the agents have data to work with from the moment
// the editor opens.
// ---------------------------------------------------------------------------
class DistributedModelsBoot extends Disposable implements IWorkbenchContribution {
	static readonly ID = 'workbench.contrib.distributedModels.boot';

	constructor(
		@IInstantiationService instantiationService: IInstantiationService,
		@IViewsService viewsService: IViewsService,
		@ILanguageFeaturesService languageFeaturesService: ILanguageFeaturesService,
	) {
		super();
		const fileWatcher = this._register(
			instantiationService.createInstance(WorkspaceFileWatcher),
		);
		const diagnosticsWatcher = this._register(
			instantiationService.createInstance(DiagnosticsWatcher),
		);

		// Inline ghost-text completions (Cursor Tab equivalent) backed by
		// the local Ollama coder model via the backend's /complete endpoint.
		const inlineProvider = this._register(
			instantiationService.createInstance(InlineCompletionsBackend),
		);
		this._register(
			registerInlineCompletionProvider(languageFeaturesService, inlineProvider),
		);

		// Keep the old Copilot chat view hidden so this panel shows only
		// Distributed Models by default.
		viewsService.closeView(COPILOT_CHAT_VIEW_ID);
		this._register(
			viewsService.onDidChangeViewVisibility((e) => {
				if (e.id === COPILOT_CHAT_VIEW_ID && e.visible) {
					viewsService.closeView(COPILOT_CHAT_VIEW_ID);
				}
			}),
		);

		void fileWatcher.start();
		void diagnosticsWatcher.flushNow();
	}
}

registerWorkbenchContribution2(
	DistributedModelsBoot.ID,
	DistributedModelsBoot,
	WorkbenchPhase.AfterRestored,
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function localize2(key: string, value: string): { value: string; original: string } {
	return nlsLocalize2(key, value);
}

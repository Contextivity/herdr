use crate::api::schema::{
    AgentProviderClearParams, AgentProviderReplaceParams, AgentProviderSource, AgentProviderTarget,
    ResponseResult,
};
use crate::app::provider_agents::{
    normalize_id, normalize_source, provider_record, validate_records, AgentProviderState,
    ProviderViewerTarget,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_agent_provider_snapshot(
        &mut self,
        id: String,
        mut params: AgentProviderSource,
    ) -> String {
        params.source = match normalize_source(&params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        let Some(snapshot) = self
            .state
            .agent_providers
            .get(&params.source)
            .map(|provider| provider.snapshot(&params.source))
        else {
            return encode_error(id, "agent_provider_not_found", "agent provider not found");
        };
        encode_success(id, ResponseResult::AgentProvider { snapshot })
    }

    pub(super) fn handle_agent_provider_focused(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentProviderFocus {
                target: self.state.focused_provider_agent.clone(),
            },
        )
    }

    pub(super) fn handle_agent_provider_replace(
        &mut self,
        id: String,
        mut params: AgentProviderReplaceParams,
    ) -> String {
        let source = match normalize_source(&params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        if params.revision == 0 {
            return encode_error(
                id,
                "invalid_agent_provider",
                "agent provider revision must be at least 1",
            );
        }
        if let Err(message) = validate_records(&mut params.agents) {
            return encode_error(id, "invalid_agent_provider", message);
        }
        if self
            .state
            .agent_providers
            .get(&source)
            .is_some_and(|provider| params.revision <= provider.revision)
        {
            return encode_error(
                id,
                "stale_agent_provider_revision",
                "agent provider revision must increase",
            );
        }

        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.viewer.pane_id) else {
            return encode_error(
                id,
                "provider_viewer_not_found",
                "provider viewer pane not found",
            );
        };
        if self.public_workspace_id(ws_idx) != params.viewer.workspace_id
            || self.public_pane_id(ws_idx, pane_id).as_deref()
                != Some(params.viewer.pane_id.as_str())
        {
            return encode_error(
                id,
                "provider_viewer_not_found",
                "provider viewer workspace and pane do not identify the same live pane",
            );
        }

        let viewer = ProviderViewerTarget {
            workspace_id: params.viewer.workspace_id,
            pane_id,
            public_pane_id: params.viewer.pane_id,
        };
        let selected = self.state.focused_provider_agent.clone().filter(|target| {
            target.source == source && params.agents.iter().any(|record| record.id == target.id)
        });
        let provider = AgentProviderState {
            revision: params.revision,
            viewer: Some(viewer),
            agents: params.agents,
        };
        let snapshot = provider.snapshot(&source);
        self.state.agent_providers.insert(source.clone(), provider);
        if let Some(selected) = selected {
            self.focus_agent_panel_target_via_api(ws_idx, pane_id, Some(selected));
        }
        self.clear_invalid_provider_focus();
        self.sync_provider_focus_event();

        encode_success(id, ResponseResult::AgentProvider { snapshot })
    }

    pub(super) fn handle_agent_provider_clear(
        &mut self,
        id: String,
        params: AgentProviderClearParams,
    ) -> String {
        let source = match normalize_source(&params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        if params.revision == 0 {
            return encode_error(
                id,
                "invalid_agent_provider",
                "agent provider revision must be at least 1",
            );
        }
        if self
            .state
            .agent_providers
            .get(&source)
            .is_some_and(|provider| params.revision <= provider.revision)
        {
            return encode_error(
                id,
                "stale_agent_provider_revision",
                "agent provider revision must increase",
            );
        }

        self.state.agent_providers.insert(
            source.clone(),
            AgentProviderState {
                revision: params.revision,
                viewer: None,
                agents: Vec::new(),
            },
        );
        self.clear_invalid_provider_focus();
        self.sync_provider_focus_event();
        encode_success(
            id,
            ResponseResult::AgentProviderCleared {
                source,
                revision: params.revision,
            },
        )
    }

    pub(super) fn handle_agent_provider_get(
        &mut self,
        id: String,
        mut target: AgentProviderTarget,
    ) -> String {
        target.source = match normalize_source(&target.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        target.id = match normalize_id(&target.id) {
            Ok(id_value) => id_value,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        let Some(record) = provider_record(&self.state.agent_providers, &target).cloned() else {
            return encode_error(id, "provider_agent_not_found", "provider agent not found");
        };
        encode_success(id, ResponseResult::AgentProviderRecord { target, record })
    }

    pub(super) fn handle_agent_provider_focus(
        &mut self,
        id: String,
        mut target: AgentProviderTarget,
    ) -> String {
        target.source = match normalize_source(&target.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        target.id = match normalize_id(&target.id) {
            Ok(id_value) => id_value,
            Err(message) => return encode_error(id, "invalid_agent_provider", message),
        };
        let Some(provider) = self.state.agent_providers.get(&target.source) else {
            return encode_error(id, "provider_agent_not_found", "provider agent not found");
        };
        let Some(record) = provider
            .agents
            .iter()
            .find(|record| record.id == target.id)
            .cloned()
        else {
            return encode_error(id, "provider_agent_not_found", "provider agent not found");
        };
        let Some(viewer) = provider.viewer.clone() else {
            return encode_error(
                id,
                "provider_viewer_not_found",
                "provider viewer pane not found",
            );
        };
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == viewer.workspace_id)
        else {
            return encode_error(
                id,
                "provider_viewer_not_found",
                "provider viewer pane not found",
            );
        };
        if self.state.workspaces[ws_idx]
            .pane_state(viewer.pane_id)
            .is_none()
        {
            return encode_error(
                id,
                "provider_viewer_not_found",
                "provider viewer pane not found",
            );
        }

        self.focus_agent_panel_target_via_api(ws_idx, viewer.pane_id, Some(target.clone()));
        encode_success(id, ResponseResult::AgentProviderRecord { target, record })
    }

    fn clear_invalid_provider_focus(&mut self) {
        if self
            .state
            .focused_provider_agent
            .as_ref()
            .is_some_and(|target| provider_record(&self.state.agent_providers, target).is_none())
        {
            self.state.focused_provider_agent = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        AgentProviderRecord, AgentProviderViewer, AgentStatus, SuccessResponse,
    };
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn app_with_viewer() -> (App, String, String) {
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(api_tx);
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let workspace = Workspace::test_new("viewer");
        let pane_id = workspace.root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let workspace_id = app.public_workspace_id(0);
        let pane_id = app.public_pane_id(0, pane_id).unwrap();
        (app, workspace_id, pane_id)
    }

    fn replace(workspace_id: &str, pane_id: &str, revision: u64) -> AgentProviderReplaceParams {
        AgentProviderReplaceParams {
            source: "fleet:test".into(),
            revision,
            viewer: AgentProviderViewer {
                workspace_id: workspace_id.into(),
                pane_id: pane_id.into(),
            },
            agents: vec![AgentProviderRecord {
                id: "one".into(),
                name: "review issue 302".into(),
                agent: Some("codex".into()),
                title: None,
                display_agent: None,
                agent_status: AgentStatus::Working,
                state_labels: Default::default(),
                tokens: Default::default(),
                state_change_seq: Some(1),
            }],
        }
    }

    #[test]
    fn provider_agent_registry_replace_is_atomic_and_rejects_stale_revision() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        let response = app
            .handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 2));
        let response: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            response.result,
            ResponseResult::AgentProvider { .. }
        ));

        let stale =
            app.handle_agent_provider_replace("stale".into(), replace(&workspace_id, &pane_id, 1));
        assert!(stale.contains("stale_agent_provider_revision"));
        assert_eq!(app.state.agent_providers["fleet:test"].revision, 2);

        let zero =
            app.handle_agent_provider_replace("zero".into(), replace(&workspace_id, &pane_id, 0));
        assert!(zero.contains("revision must be at least 1"));
    }

    #[test]
    fn provider_agent_cleanup_clear_leaves_revision_tombstone_and_clears_focus() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 2));
        app.state.focused_provider_agent = Some(AgentProviderTarget {
            source: "fleet:test".into(),
            id: "one".into(),
        });
        app.handle_agent_provider_clear(
            "clear".into(),
            AgentProviderClearParams {
                source: "fleet:test".into(),
                revision: 3,
            },
        );

        let provider = &app.state.agent_providers["fleet:test"];
        assert_eq!(provider.revision, 3);
        assert!(provider.agents.is_empty());
        assert!(provider.viewer.is_none());
        assert!(app.state.focused_provider_agent.is_none());
        let snapshot = app.handle_agent_provider_snapshot(
            "snapshot".into(),
            AgentProviderSource {
                source: "fleet:test".into(),
            },
        );
        let snapshot: SuccessResponse = serde_json::from_str(&snapshot).unwrap();
        assert!(matches!(
            snapshot.result,
            ResponseResult::AgentProvider {
                snapshot: crate::api::schema::AgentProviderSnapshot {
                    revision: 3,
                    viewer: None,
                    ref agents,
                    ..
                }
            } if agents.is_empty()
        ));
    }

    #[test]
    fn provider_agent_inventory_stays_out_of_physical_agent_list() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 1));

        let response = app.handle_agent_list("list".into());
        let response: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentList { agents } = response.result else {
            panic!("agent.list response");
        };
        assert!(agents
            .iter()
            .all(|agent| agent.name.as_deref() != Some("review issue 302")));
        assert_eq!(app.state.agent_providers["fleet:test"].agents.len(), 1);
    }

    #[test]
    fn provider_agent_registry_rejects_invalid_target_ids_before_lookup() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 1));
        let response = app.handle_agent_provider_get(
            "get".into(),
            AgentProviderTarget {
                source: "fleet:test".into(),
                id: "bad\u{7}id".into(),
            },
        );
        assert!(response.contains("invalid_agent_provider"));
        assert!(!response.contains("provider_agent_not_found"));
    }

    #[test]
    fn provider_agent_focus_targets_shared_viewer_and_emits_provider_identity() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 1));
        let target = AgentProviderTarget {
            source: "fleet:test".into(),
            id: "one".into(),
        };

        let response = app.handle_agent_provider_focus("focus".into(), target.clone());
        assert!(!response.contains("error"));
        assert_eq!(app.state.focused_provider_agent, Some(target.clone()));
        assert!(app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| matches!(
                    &event.data,
                    crate::api::schema::EventData::AgentProviderFocused { target: Some(seen) }
                        if seen == &target
            )));
    }

    #[test]
    fn provider_agent_focus_viewer_migration_preserves_selection() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 1));
        let target = AgentProviderTarget {
            source: "fleet:test".into(),
            id: "one".into(),
        };
        app.handle_agent_provider_focus("focus".into(), target.clone());

        let second = Workspace::test_new("replacement-viewer");
        let second_pane = second.root_pane;
        let second_workspace = second.id.clone();
        app.state.workspaces.push(second);
        let second_public_pane = app.public_pane_id(1, second_pane).unwrap();
        app.handle_agent_provider_replace(
            "replace-viewer".into(),
            replace(&second_workspace, &second_public_pane, 2),
        );

        assert_eq!(app.state.focused_provider_agent, Some(target.clone()));
        assert_eq!(app.state.active, Some(1));
        assert_eq!(app.state.workspaces[1].focused_pane_id(), Some(second_pane));
        assert_eq!(app.last_provider_focus, Some(target));
    }

    #[test]
    fn provider_agent_cleanup_viewer_loss_clears_logical_selection() {
        let (mut app, workspace_id, pane_id) = app_with_viewer();
        app.handle_agent_provider_replace("replace".into(), replace(&workspace_id, &pane_id, 1));
        app.handle_agent_provider_focus(
            "focus".into(),
            AgentProviderTarget {
                source: "fleet:test".into(),
                id: "one".into(),
            },
        );

        app.state.workspaces.clear();
        app.state.active = None;
        app.sync_provider_focus_event();
        assert!(app.state.focused_provider_agent.is_none());
        assert!(app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| matches!(
                event.data,
                crate::api::schema::EventData::AgentProviderFocused { target: None }
            )));
    }
}

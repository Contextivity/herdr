#[cfg(unix)]
use serde::{Deserialize, Serialize};

/// Long-lived pane runtime transferred during server replacement.
///
/// Handoff preserves server-owned session state such as PTYs, processes, agent
/// identity, and durable plugin/session metadata. It intentionally does not
/// preserve transient coordination such as in-flight requests, waits,
/// subscriptions, client sockets, or pane-to-pane messages; clients reconnect
/// and retry those operations after replacement.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRuntimeState {
    pub pane_id: u32,
    #[serde(default)]
    pub terminal_id: Option<crate::terminal::TerminalId>,
    #[serde(default)]
    pub metadata: HandoffMetadata,
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<crate::pane::InputState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_history_ansi: Option<String>,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    pub fn with_pane_id(mut self, pane_id: crate::layout::PaneId) -> Self {
        self.pane_id = pane_id.raw();
        self
    }
}

#[derive(Debug)]
pub(crate) struct ImportedHandoffRuntime {
    #[cfg(unix)]
    pub master_fd: std::os::fd::RawFd,
    #[cfg(unix)]
    pub state: HandoffRuntimeState,
}

/// Runtime metadata is carried only by live handoff. Ordinary session
/// snapshots must remain portable across machines and boot sessions.
#[cfg(unix)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HandoffMetadata {
    pub tokens: std::collections::HashMap<String, crate::metadata_tokens::HandoffToken>,
    pub sequences: std::collections::HashMap<String, u64>,
    #[serde(default)]
    pub sequence_agents: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub token_sequence_sources: std::collections::HashSet<String>,
}

#[cfg(unix)]
impl HandoffMetadata {
    pub(crate) fn capture_workspace(workspace: &crate::workspace::Workspace) -> Self {
        // Sample the shared clock first so capture cannot extend a deadline.
        let monotonic = crate::platform::handoff_monotonic_time();
        Self {
            tokens: workspace
                .metadata_tokens
                .capture_handoff(std::time::Instant::now(), monotonic),
            sequences: workspace.metadata_token_sequences.clone(),
            ..Default::default()
        }
    }

    pub(crate) fn restore_workspace(self, workspace: &mut crate::workspace::Workspace) {
        workspace.metadata_tokens = crate::metadata_tokens::MetadataTokens::restore_handoff(
            self.tokens,
            std::time::Instant::now(),
            crate::platform::handoff_monotonic_time(),
        );
        workspace.metadata_token_sequences = self.sequences;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn workspace_handoff_preserves_sequences_and_drops_expired_tokens() {
        let mut source = crate::workspace::Workspace::test_new("source");
        source.metadata_token_sequences.insert("writer".into(), 7);
        source.metadata_tokens.patch(
            std::collections::HashMap::from([("permanent".into(), Some("group".into()))]),
            None,
            std::time::Instant::now(),
        );
        source.metadata_tokens.patch(
            std::collections::HashMap::from([("expired".into(), Some("temporary".into()))]),
            Some(std::time::Duration::ZERO),
            std::time::Instant::now(),
        );
        let transfer = serde_json::from_str::<HandoffMetadata>(
            &serde_json::to_string(&HandoffMetadata::capture_workspace(&source)).unwrap(),
        )
        .unwrap();
        let mut target = crate::workspace::Workspace::test_new("target");
        transfer.restore_workspace(&mut target);
        assert_eq!(
            target
                .metadata_tokens
                .values()
                .get("permanent")
                .map(String::as_str),
            Some("group")
        );
        assert!(!target.metadata_tokens.values().contains_key("expired"));
        assert_eq!(
            crate::metadata_tokens::accept_sequence(
                &mut target.metadata_token_sequences,
                "writer",
                Some(7)
            ),
            Ok(false)
        );
        assert_eq!(
            crate::metadata_tokens::accept_sequence(
                &mut target.metadata_token_sequences,
                "writer",
                Some(8)
            ),
            Ok(true)
        );
    }
}

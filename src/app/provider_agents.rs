use std::collections::HashSet;

use crate::api::schema::{
    AgentProviderRecord, AgentProviderSnapshot, AgentProviderTarget, AgentProviderViewer,
};
use crate::layout::PaneId;

const MAX_PROVIDER_AGENTS: usize = 2_000;
const MAX_ID_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 128;
const MAX_AGENT_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 256;
const MAX_METADATA_VALUE_CHARS: usize = 256;
const MAX_STATE_LABELS: usize = 8;
const MAX_TOKENS: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct ProviderViewerTarget {
    pub workspace_id: String,
    pub pane_id: PaneId,
    pub public_pane_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentProviderState {
    pub revision: u64,
    pub viewer: Option<ProviderViewerTarget>,
    pub agents: Vec<AgentProviderRecord>,
}

impl AgentProviderState {
    pub(crate) fn snapshot(&self, source: &str) -> AgentProviderSnapshot {
        AgentProviderSnapshot {
            source: source.to_string(),
            revision: self.revision,
            viewer: self.viewer.as_ref().map(|viewer| AgentProviderViewer {
                workspace_id: viewer.workspace_id.clone(),
                pane_id: viewer.public_pane_id.clone(),
            }),
            agents: self.agents.clone(),
        }
    }
}

pub(crate) fn normalize_source(source: &str) -> Result<String, String> {
    crate::app::agent_view::validate_agent_view_source(source)
}

pub(crate) fn validate_records(records: &mut [AgentProviderRecord]) -> Result<(), String> {
    if records.len() > MAX_PROVIDER_AGENTS {
        return Err(format!(
            "agent provider may contain at most {MAX_PROVIDER_AGENTS} records"
        ));
    }

    let mut ids = HashSet::with_capacity(records.len());
    for record in records {
        record.id = normalize_id(&record.id)?;
        record.name = normalized_text(&record.name, "provider agent name", MAX_NAME_CHARS)?;
        record.agent = normalize_optional(record.agent.take(), "agent", MAX_AGENT_CHARS)?;
        record.title = normalize_optional(record.title.take(), "title", MAX_TITLE_CHARS)?;
        record.display_agent = normalize_optional(
            record.display_agent.take(),
            "display agent",
            MAX_TITLE_CHARS,
        )?;
        if !ids.insert(record.id.clone()) {
            return Err(format!("duplicate provider agent id `{}`", record.id));
        }
        if record.state_labels.len() > MAX_STATE_LABELS {
            return Err(format!(
                "provider agent {} has more than {MAX_STATE_LABELS} state labels",
                record.id
            ));
        }
        if record.tokens.len() > MAX_TOKENS {
            return Err(format!(
                "provider agent {} has more than {MAX_TOKENS} metadata tokens",
                record.id
            ));
        }
        for (key, value) in record.state_labels.iter().chain(record.tokens.iter()) {
            validate_metadata_key(key)?;
            if value.chars().count() > MAX_METADATA_VALUE_CHARS
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "provider agent {} metadata value is invalid",
                    record.id
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn normalize_id(id: &str) -> Result<String, String> {
    normalized_text(id, "provider agent id", MAX_ID_CHARS)
}

fn normalize_optional(
    value: Option<String>,
    label: &str,
    max_chars: usize,
) -> Result<Option<String>, String> {
    value
        .map(|value| normalized_text(&value, label, max_chars))
        .transpose()
}

fn normalized_text(value: &str, label: &str, max_chars: usize) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max_chars || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{label} must be non-empty, at most {max_chars} characters, and contain no control characters"
        ));
    }
    Ok(value.to_string())
}

fn validate_metadata_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 32
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("provider metadata key is invalid".into());
    }
    Ok(())
}

pub(crate) fn provider_record<'a>(
    providers: &'a std::collections::BTreeMap<String, AgentProviderState>,
    target: &AgentProviderTarget,
) -> Option<&'a AgentProviderRecord> {
    providers
        .get(&target.source)?
        .agents
        .iter()
        .find(|record| record.id == target.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;

    fn record(id: &str) -> AgentProviderRecord {
        AgentProviderRecord {
            id: id.into(),
            name: format!("agent-{id}"),
            agent: Some("codex".into()),
            title: None,
            display_agent: None,
            agent_status: AgentStatus::Working,
            state_labels: Default::default(),
            tokens: Default::default(),
            state_change_seq: Some(1),
        }
    }

    #[test]
    fn provider_agent_registry_validates_and_normalizes_records() {
        let mut records = vec![record(" lane-1 ")];
        records[0].name = " Review issue 302 ".into();
        validate_records(&mut records).unwrap();
        assert_eq!(records[0].id, "lane-1");
        assert_eq!(records[0].name, "Review issue 302");
    }

    #[test]
    fn provider_agent_registry_rejects_duplicate_ids_and_excessive_snapshots() {
        let mut duplicate = vec![record("same"), record("same")];
        assert!(validate_records(&mut duplicate)
            .unwrap_err()
            .contains("duplicate"));

        let mut oversized = (0..=MAX_PROVIDER_AGENTS)
            .map(|index| record(&format!("agent-{index}")))
            .collect::<Vec<_>>();
        assert!(validate_records(&mut oversized)
            .unwrap_err()
            .contains("at most"));
    }

    #[test]
    fn provider_agent_protocol_snapshot_preserves_source_viewer_and_records() {
        let state = AgentProviderState {
            revision: 7,
            viewer: Some(ProviderViewerTarget {
                workspace_id: "w1".into(),
                pane_id: PaneId::from_raw(2),
                public_pane_id: "w1:p2".into(),
            }),
            agents: vec![record("one")],
        };
        let snapshot = state.snapshot("fleet");
        assert_eq!(snapshot.source, "fleet");
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.viewer.as_ref().unwrap().pane_id, "w1:p2");
        assert_eq!(snapshot.agents[0].id, "one");
    }
}

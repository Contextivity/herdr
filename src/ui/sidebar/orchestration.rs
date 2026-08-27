use std::collections::{HashMap, HashSet};

use super::{agent_panel_status_key, AgentPanelEntry};

const ORCHESTRATION_ID: &str = "orchestration_id";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OrchestrationCounts {
    pub working: usize,
    pub waiting: usize,
    pub done: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentPanelItem {
    RunHeader {
        id: String,
        label: String,
        owner: String,
        lifecycle: String,
        counts: OrchestrationCounts,
        collapsed: bool,
    },
    RepositoryHeader {
        label: String,
        count: usize,
    },
    Agent {
        entry_index: usize,
        responsibility: String,
        metadata: String,
        details: String,
        status: String,
        selected: bool,
    },
    RunFooter,
    LegacyAgent {
        entry_index: usize,
    },
}

impl AgentPanelItem {
    pub(crate) fn height(&self) -> u16 {
        match self {
            Self::RunHeader { .. } => 2,
            Self::RepositoryHeader { .. } | Self::RunFooter => 1,
            Self::Agent { selected, .. } => {
                if *selected {
                    3
                } else {
                    2
                }
            }
            Self::LegacyAgent { .. } => 0,
        }
    }

    pub(crate) fn entry_index(&self) -> Option<usize> {
        match self {
            Self::Agent { entry_index, .. } | Self::LegacyAgent { entry_index } => {
                Some(*entry_index)
            }
            _ => None,
        }
    }

    pub(crate) fn orchestration_id(&self) -> Option<&str> {
        match self {
            Self::RunHeader { id, .. } => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentPanelLayout {
    pub items: Vec<AgentPanelItem>,
}

impl AgentPanelLayout {
    pub(crate) fn item_index_for_agent(&self, entry_index: usize) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.entry_index() == Some(entry_index))
    }
}

#[derive(Debug)]
struct RepositoryGroup {
    label: String,
    entries: Vec<usize>,
}

#[derive(Debug)]
struct RunGroup {
    id: String,
    label: String,
    owner: String,
    lifecycle: String,
    repositories: Vec<RepositoryGroup>,
    repository_index: HashMap<String, usize>,
    priority: u8,
}

pub(crate) fn collapse_key(orchestration_id: &str) -> String {
    format!("orchestration:{orchestration_id}")
}

pub(crate) fn build(
    entries: &[AgentPanelEntry],
    collapsed_keys: &HashSet<String>,
    active: Option<(usize, usize, crate::layout::PaneId)>,
) -> AgentPanelLayout {
    let mut runs = Vec::<RunGroup>::new();
    let mut run_index = HashMap::<String, usize>::new();
    let mut legacy = Vec::new();

    for (entry_index, entry) in entries.iter().enumerate() {
        let Some(id) = entry
            .tokens
            .get(ORCHESTRATION_ID)
            .filter(|id| !id.is_empty())
        else {
            legacy.push(entry_index);
            continue;
        };
        let index = match run_index.get(id) {
            Some(index) => *index,
            None => {
                let index = runs.len();
                run_index.insert(id.clone(), index);
                runs.push(RunGroup {
                    id: id.clone(),
                    label: token(entry, "orchestration_label", id),
                    owner: token(entry, "orchestration_owner", "agent"),
                    lifecycle: token(entry, "orchestration_state", "active"),
                    repositories: Vec::new(),
                    repository_index: HashMap::new(),
                    priority: 0,
                });
                index
            }
        };
        let run = &mut runs[index];
        run.priority = run.priority.max(entry_priority(entry));
        let repository = token(entry, "repository", &entry.primary_label);
        let repository_index = match run.repository_index.get(&repository) {
            Some(index) => *index,
            None => {
                let index = run.repositories.len();
                run.repository_index.insert(repository.clone(), index);
                run.repositories.push(RepositoryGroup {
                    label: repository,
                    entries: Vec::new(),
                });
                index
            }
        };
        run.repositories[repository_index].entries.push(entry_index);
    }

    runs.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut items = Vec::new();
    for run in runs {
        let collapsed = collapsed_keys.contains(&collapse_key(&run.id));
        let counts = orchestration_counts(&run, entries);
        items.push(AgentPanelItem::RunHeader {
            id: run.id,
            label: run.label,
            owner: run.owner,
            lifecycle: run.lifecycle,
            counts,
            collapsed,
        });
        if !collapsed {
            for mut repository in run.repositories {
                repository.entries.sort_by(|left, right| {
                    entry_priority(&entries[*right])
                        .cmp(&entry_priority(&entries[*left]))
                        .then_with(|| {
                            responsibility(&entries[*left]).cmp(&responsibility(&entries[*right]))
                        })
                });
                items.push(AgentPanelItem::RepositoryHeader {
                    label: repository.label,
                    count: repository.entries.len(),
                });
                for entry_index in repository.entries {
                    let entry = &entries[entry_index];
                    let selected = active.is_some_and(|target| {
                        target == (entry.ws_idx, entry.tab_idx, entry.pane_id)
                    });
                    items.push(AgentPanelItem::Agent {
                        entry_index,
                        responsibility: responsibility(entry),
                        metadata: metadata_line(entry),
                        details: detail_line(entry),
                        status: status(entry),
                        selected,
                    });
                }
            }
        }
        items.push(AgentPanelItem::RunFooter);
    }
    items.extend(
        legacy
            .into_iter()
            .map(|entry_index| AgentPanelItem::LegacyAgent { entry_index }),
    );
    AgentPanelLayout { items }
}

fn token(entry: &AgentPanelEntry, name: &str, fallback: &str) -> String {
    entry
        .tokens
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

fn responsibility(entry: &AgentPanelEntry) -> String {
    token(entry, "responsibility", &entry.primary_label)
}

fn status(entry: &AgentPanelEntry) -> String {
    entry
        .tokens
        .get("remote_state")
        .cloned()
        .unwrap_or_else(|| agent_panel_status_key(entry.state, entry.seen).to_string())
}

fn metadata_line(entry: &AgentPanelEntry) -> String {
    let state = status(entry).to_ascii_uppercase();
    if state == "DONE" {
        return "DONE · awaiting cleanup".into();
    }
    let harness = entry
        .tokens
        .get("harness")
        .cloned()
        .or_else(|| entry.agent_kind_label.clone())
        .unwrap_or_else(|| "agent".into());
    match entry.tokens.get("remote_host") {
        Some(host) => format!("{state} · {harness} · {host}"),
        None => format!("{state} · {harness}"),
    }
}

fn detail_line(entry: &AgentPanelEntry) -> String {
    let mut details = Vec::new();
    if let Some(name) = entry
        .tokens
        .get("remote_agent")
        .or(entry.agent_label.as_ref())
    {
        details.push(name.clone());
    }
    if let Some(parent) = entry.tokens.get("parent_agent") {
        details.push(format!("parent: {parent}"));
    }
    if let Some(branch) = entry.tokens.get("branch") {
        details.push(branch.clone());
    }
    if let Some(worktree) = entry.tokens.get("worktree") {
        details.push(worktree.clone());
    }
    details.join(" · ")
}

fn entry_priority(entry: &AgentPanelEntry) -> u8 {
    match status(entry).as_str() {
        "question" | "blocked" => 4,
        "working" => 3,
        "idle" => 2,
        "done" => 1,
        _ => 0,
    }
}

fn orchestration_counts(run: &RunGroup, entries: &[AgentPanelEntry]) -> OrchestrationCounts {
    let mut counts = OrchestrationCounts::default();
    for entry in run
        .repositories
        .iter()
        .flat_map(|repository| repository.entries.iter())
        .map(|index| &entries[*index])
    {
        match status(entry).as_str() {
            "working" => counts.working += 1,
            "done" => counts.done += 1,
            _ => counts.waiting += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;
    use std::time::{Duration, Instant};

    fn entry(name: &str, run: Option<&str>, repository: &str, state: &str) -> AgentPanelEntry {
        let mut tokens = HashMap::from([
            ("repository".into(), repository.into()),
            ("responsibility".into(), format!("work {name}")),
            ("remote_agent".into(), name.into()),
            ("remote_state".into(), state.into()),
            ("remote_host".into(), "ai-dev-w1".into()),
            ("harness".into(), "codex-cli".into()),
        ]);
        if let Some(run) = run {
            tokens.insert("orchestration_id".into(), run.into());
            tokens.insert("orchestration_label".into(), "Backlog wave".into());
            tokens.insert("orchestration_owner".into(), "t3".into());
            tokens.insert("orchestration_state".into(), "active".into());
        }
        AgentPanelEntry {
            ws_idx: name.len(),
            tab_idx: 0,
            pane_id: crate::layout::PaneId::from_raw(name.len() as u32 + 1),
            primary_label: repository.into(),
            primary_tab_label: None,
            pane_label: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_label: Some(name.into()),
            agent_kind_label: Some("codex".into()),
            agent: None,
            state: if state == "working" {
                AgentState::Working
            } else {
                AgentState::Idle
            },
            seen: state != "done",
            last_agent_state_change_seq: None,
            state_labels: HashMap::new(),
            tokens,
        }
    }

    #[test]
    fn orchestration_sidebar_groups_run_repository_and_agents() {
        let entries = vec![
            entry("one", Some("run-1"), "stack", "working"),
            entry("two", Some("run-1"), "frontend", "question"),
            entry("legacy", None, "legacy", "idle"),
        ];
        let layout = build(&entries, &HashSet::new(), None);

        assert!(matches!(
            &layout.items[0],
            AgentPanelItem::RunHeader { id, counts, .. }
                if id == "run-1" && counts.working == 1 && counts.waiting == 1
        ));
        assert!(layout.items.iter().any(|item| matches!(
            item,
            AgentPanelItem::RepositoryHeader { label, count } if label == "stack" && *count == 1
        )));
        assert!(matches!(
            layout.items.last(),
            Some(AgentPanelItem::LegacyAgent { entry_index: 2 })
        ));
    }

    #[test]
    fn orchestration_sidebar_keeps_done_agent_until_entry_is_removed() {
        let entries = vec![entry("done", Some("run-1"), "stack", "done")];
        let layout = build(&entries, &HashSet::new(), None);
        assert!(layout.items.iter().any(|item| matches!(
            item,
            AgentPanelItem::Agent { metadata, .. } if metadata == "DONE · awaiting cleanup"
        )));
    }

    #[test]
    fn orchestration_sidebar_collapses_children_but_keeps_card() {
        let entries = vec![entry("one", Some("run-1"), "stack", "working")];
        let collapsed = HashSet::from([collapse_key("run-1")]);
        let layout = build(&entries, &collapsed, None);
        assert_eq!(layout.items.len(), 2);
        assert!(matches!(
            layout.items[0],
            AgentPanelItem::RunHeader {
                collapsed: true,
                ..
            }
        ));
        assert!(matches!(layout.items[1], AgentPanelItem::RunFooter));
    }

    #[test]
    fn orchestration_sidebar_selected_agent_expands_to_three_lines() {
        let entries = vec![entry("one", Some("run-1"), "stack", "working")];
        let target = (entries[0].ws_idx, entries[0].tab_idx, entries[0].pane_id);
        let layout = build(&entries, &HashSet::new(), Some(target));
        let item = layout
            .items
            .iter()
            .find(|item| matches!(item, AgentPanelItem::Agent { .. }))
            .expect("agent row");
        assert_eq!(item.height(), 3);
    }

    #[test]
    fn orchestration_sidebar_scale_500_stays_interactive() {
        let entries = (0..500)
            .map(|index| {
                let run = format!("run-{}", index % 10);
                let repository = format!("repo-{}", index % 20);
                entry(
                    &format!("agent-{index}"),
                    Some(&run),
                    &repository,
                    if index % 7 == 0 {
                        "question"
                    } else {
                        "working"
                    },
                )
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        let layout = build(&entries, &HashSet::new(), None);
        let elapsed = started.elapsed();

        assert_eq!(
            layout
                .items
                .iter()
                .filter(|item| matches!(item, AgentPanelItem::Agent { .. }))
                .count(),
            500
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "500-agent layout took {elapsed:?}"
        );
    }
}

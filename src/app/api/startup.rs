//! Receipted startup is a two-phase operation: prepare, persist receipt, submit once.
use super::responses::{encode_error, encode_error_body, encode_success};
use crate::api::schema::{
    AgentStartParams, AgentStartupParams, PaneTarget, ResponseResult, StartupReceipt,
};
use crate::app::App;
use crate::platform::StartupScript;
use crate::terminal::TerminalId;

pub(in crate::app) struct StartupTicket {
    receipt: StartupReceipt,
    terminal_id: TerminalId,
    start: AgentStartParams,
    script: Option<StartupScript>,
    submitted: bool,
    closed_at: Option<std::time::Instant>,
    cleanup_completed: bool,
}

impl App {
    /// Startup tickets are issuing-daemon authority, not reconstructed from a
    /// wrapper's receipt. Until their transfer is supported, keep that daemon
    /// alive while any unexpired ticket still needs reconciliation or cleanup.
    #[cfg(unix)]
    pub(crate) fn has_handoff_startup_hold(&self) -> bool {
        self.startup_tickets.values().any(|ticket| {
            !ticket.cleanup_completed
                && ticket
                    .closed_at
                    .is_none_or(|closed| closed.elapsed() < std::time::Duration::from_secs(86_400))
        })
    }

    pub(in crate::app) fn retire_startup_files(&mut self, terminal_id: &TerminalId) {
        for ticket in self
            .startup_tickets
            .values_mut()
            .filter(|t| &t.terminal_id == terminal_id)
        {
            ticket.script.take();
            ticket.closed_at = Some(std::time::Instant::now());
        }
    }

    pub(super) fn handle_agent_startup(
        &mut self,
        id: String,
        params: AgentStartupParams,
    ) -> String {
        // Inspection never mutates the ticket map, submits input or adopts a process.
        if let AgentStartupParams::Inspect { receipt } = &params {
            return self.inspect_startup(id, receipt.clone());
        }
        // Closed tickets retain receipt proof for a day. Expiration only loses
        // authority (unknown tickets fail closed), never authorizes deletion.
        self.startup_tickets.retain(|_, ticket| {
            ticket
                .closed_at
                .is_none_or(|closed| closed.elapsed() < std::time::Duration::from_secs(86_400))
        });
        match params {
            AgentStartupParams::Prepare { start, preparation } => {
                self.prepare_startup(id, start, preparation)
            }
            AgentStartupParams::Inspect { receipt } => self.inspect_startup(id, receipt),
            AgentStartupParams::Launch { receipt } => self.launch_startup(id, receipt),
            AgentStartupParams::Cleanup { receipt } => self.cleanup_startup(id, receipt),
        }
    }

    fn inspect_startup(&self, id: String, receipt: StartupReceipt) -> String {
        let Some(ticket) = self.startup_tickets.get(&receipt.ticket).filter(|ticket| {
            ticket
                .closed_at
                .is_none_or(|closed| closed.elapsed() < std::time::Duration::from_secs(86_400))
        }) else {
            return encode_error(
                id,
                "startup_ticket_unknown",
                "original daemon ticket unavailable; retain ownership hold",
            );
        };
        let mismatch = || {
            encode_error(
                id.clone(),
                "startup_ownership_mismatch",
                "original startup identity is not proven",
            )
        };
        if ticket.receipt != receipt
            || self.collect_agent_infos().iter().any(|agent| {
                agent.name.as_deref() == Some(&receipt.name)
                    && agent.terminal_id != receipt.terminal_id
            })
        {
            return mismatch();
        }
        let state = if self
            .parse_current_public_pane_id(&receipt.pane_id)
            .is_some()
        {
            if !self.startup_location_matches(ticket)
                || observed_shell_cwd(receipt.shell_pid).as_deref()
                    != Some(std::path::Path::new(&receipt.cwd))
                || self
                    .state
                    .terminals
                    .get(&ticket.terminal_id)
                    .is_none_or(|terminal| {
                        terminal
                            .agent_name
                            .as_deref()
                            .is_some_and(|name| name != receipt.name)
                    })
            {
                return mismatch();
            }
            if !ticket.submitted {
                if !self.startup_shell_only(ticket) {
                    return mismatch();
                }
                "prepared"
            } else if ticket.script.as_ref().is_some_and(StartupScript::finished) {
                if !self.startup_shell_only(ticket) {
                    return mismatch();
                }
                "finished"
            } else {
                "submitted"
            }
        } else {
            let attached = self.state.workspaces.iter().any(|workspace| {
                workspace.tabs.iter().any(|tab| {
                    tab.panes
                        .values()
                        .any(|pane| pane.attached_terminal_id == ticket.terminal_id)
                })
            });
            if ticket.closed_at.is_none()
                || attached
                || self.terminal_runtimes.get(&ticket.terminal_id).is_some()
                || observed_process_exists(receipt.shell_pid)
            {
                return mismatch();
            }
            "closed"
        };
        encode_success(
            id,
            ResponseResult::AgentStartup {
                receipt,
                state: state.into(),
            },
        )
    }

    fn prepare_startup(
        &mut self,
        id: String,
        start: AgentStartParams,
        preparation: Vec<String>,
    ) -> String {
        let timeout = match crate::app::agents::validated_start_timeout(start.timeout_ms) {
            Ok(timeout) => timeout,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };
        let invalid = || {
            encode_error(
                id.clone(),
                "startup_invalid_request",
                "invalid startup name, kind, arguments or preparation",
            )
        };
        let Some(kind) = crate::detect::parse_agent_label(&start.kind) else {
            return invalid();
        };
        if !crate::app::agents::valid_agent_name(&start.name)
            || start.args.iter().any(|a| a.chars().any(char::is_control))
            || preparation.len() > 32
            || preparation.iter().any(|p| p.contains('\0'))
        {
            return invalid();
        }
        if self
            .collect_agent_infos()
            .iter()
            .any(|a| a.name.as_deref() == Some(&start.name))
        {
            return encode_error(
                id,
                "agent_name_taken",
                "startup name already belongs to an agent",
            );
        }
        let Some((ws, pane)) = self.parse_current_public_pane_id(&start.pane_id) else {
            return encode_error(id, "pane_not_found", "startup pane is absent");
        };
        let Some(info) = self.pane_info(ws, pane) else {
            return invalid();
        };
        let Some(terminal_id) = self.state.workspaces[ws].terminal_id(pane).cloned() else {
            return invalid();
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return invalid();
        };
        if terminal.is_agent_terminal()
            || terminal.managed_agent_kind().is_some()
            || self
                .startup_tickets
                .values()
                .any(|t| t.terminal_id == terminal_id)
        {
            return encode_error(
                id,
                "startup_ownership_mismatch",
                "terminal already has an agent or startup ticket",
            );
        }
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            return invalid();
        };
        let Some(pid) = runtime.child_pid() else {
            return invalid();
        };
        let Some(lifetime) = observed_shell_lifetime(pid) else {
            return encode_error(
                id,
                "startup_identity_unavailable",
                "original shell lifetime unavailable on this platform",
            );
        };
        let Some(shell) = crate::app::agents::available_shell_name(runtime) else {
            return encode_error(
                id,
                "agent_pane_busy",
                "original shell is still initializing or busy",
            );
        };
        let Some(cwd) = info.cwd else {
            return invalid();
        };
        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(start.args.clone());
        let Some(command) = crate::platform::interactive_shell_command(&argv, &shell) else {
            return invalid();
        };
        let script = match StartupScript::create(&shell, &command, &preparation) {
            Ok(script) => script,
            Err(err) => return encode_error(id, "startup_script_failed", err.to_string()),
        };
        if observed_shell_lifetime(pid).as_deref() != Some(&lifetime) {
            return encode_error(
                id,
                "startup_ownership_mismatch",
                "shell changed while preparing startup",
            );
        }
        let receipt = StartupReceipt {
            ticket: script.ticket(),
            pane_id: info.pane_id,
            workspace_id: info.workspace_id,
            terminal_id: info.terminal_id,
            cwd,
            name: start.name.clone(),
            kind: crate::detect::agent_label(kind).to_string(),
            timeout_ms: timeout.as_millis() as u64,
            shell_pid: pid,
            shell_lifetime: lifetime,
        };
        self.startup_tickets.insert(
            receipt.ticket.clone(),
            StartupTicket {
                receipt: receipt.clone(),
                terminal_id,
                start,
                script: Some(script),
                submitted: false,
                closed_at: None,
                cleanup_completed: false,
            },
        );
        encode_success(
            id,
            ResponseResult::AgentStartup {
                receipt,
                state: "prepared".into(),
            },
        )
    }

    fn startup_location_matches(&self, ticket: &StartupTicket) -> bool {
        let Some((ws, pane)) = self.parse_current_public_pane_id(&ticket.receipt.pane_id) else {
            return false;
        };
        self.state.workspaces[ws].terminal_id(pane) == Some(&ticket.terminal_id)
            && self.public_workspace_id(ws) == ticket.receipt.workspace_id
            && self
                .terminal_runtimes
                .get(&ticket.terminal_id)
                .and_then(|r| r.child_pid())
                == Some(ticket.receipt.shell_pid)
            && observed_shell_lifetime(ticket.receipt.shell_pid).as_deref()
                == Some(&ticket.receipt.shell_lifetime)
    }

    fn launch_startup(&mut self, id: String, receipt: StartupReceipt) -> String {
        let Some(ticket) = self.startup_tickets.get(&receipt.ticket) else {
            return encode_error(
                id,
                "startup_ticket_unknown",
                "ticket is unknown to this daemon; no input sent",
            );
        };
        if ticket.receipt != receipt || !self.startup_location_matches(ticket) {
            return encode_error(
                id,
                "startup_ownership_mismatch",
                "startup terminal or shell lifetime changed",
            );
        }
        if ticket.submitted {
            return encode_error(
                id,
                "startup_already_submitted",
                "startup input was already submitted; never resend after timeout",
            );
        }
        let start = ticket.start.clone();
        if let Err(err) = crate::app::agents::validated_start_timeout(start.timeout_ms) {
            return encode_error_body(id, self.agent_start_error_body(err));
        }
        let Some(script) = ticket.script.as_ref() else {
            return encode_error(
                id,
                "startup_terminal_closed",
                "startup terminal has already closed",
            );
        };
        let source = script.source_command.clone();
        let mut submitted = false;
        let result = self.start_agent_with_source(start, Some(&source), &mut submitted);
        // The synchronous enqueue boundary reports acceptance independently of
        // response construction. Unknown/transport failures never authorize replay.
        if let Some(ticket) = self.startup_tickets.get_mut(&receipt.ticket) {
            ticket.submitted = submitted;
        }
        match result {
            Ok((agent, argv)) => encode_success(id, ResponseResult::AgentStarted { agent, argv }),
            Err(err) => encode_error_body(id, self.agent_start_error_body(err)),
        }
    }

    fn startup_shell_only(&self, ticket: &StartupTicket) -> bool {
        if !self.startup_location_matches(ticket) {
            return false;
        }
        let Some(terminal) = self.state.terminals.get(&ticket.terminal_id) else {
            return false;
        };
        if terminal.persisted_agent_session.is_some()
            || terminal.effective_known_agent().is_some_and(|agent| {
                Some(agent) != crate::detect::parse_agent_label(&ticket.start.kind)
            })
            || terminal
                .agent_name
                .as_deref()
                .is_some_and(|name| name != ticket.receipt.name)
        {
            return false;
        }
        let Some(runtime) = self.terminal_runtimes.get(&ticket.terminal_id) else {
            return false;
        };
        // Require both the foreground and the entire original session to contain
        // only the original shell. Background agents must not be killed either.
        crate::app::agents::available_shell_name(runtime).is_some()
            && crate::platform::session_processes(ticket.receipt.shell_pid)
                == vec![ticket.receipt.shell_pid]
    }

    fn cleanup_startup(&mut self, id: String, receipt: StartupReceipt) -> String {
        let Some(ticket) = self.startup_tickets.get(&receipt.ticket) else {
            return encode_error(
                id,
                "startup_ticket_unknown",
                "legacy, expired or unknown startup ticket; cleanup refused",
            );
        };
        if ticket.receipt != receipt {
            return encode_error(
                id,
                "startup_ownership_mismatch",
                "receipt does not match daemon ticket",
            );
        }
        if self.collect_agent_infos().iter().any(|agent| {
            agent.name.as_deref() == Some(&receipt.name) && agent.terminal_id != receipt.terminal_id
        }) {
            return encode_error(id, "startup_ownership_mismatch", "startup name has rebound");
        }
        // Completed cleanup proves a previous operation, not current PID absence.
        // It must never close again, even before the original child is reaped.
        if ticket.cleanup_completed {
            return encode_success(
                id,
                ResponseResult::AgentStartup {
                    receipt,
                    state: "cleaned".into(),
                },
            );
        }
        let pane_absent = self
            .parse_current_public_pane_id(&receipt.pane_id)
            .is_none();
        if pane_absent {
            let terminal_still_attached = self.state.workspaces.iter().any(|workspace| {
                workspace.tabs.iter().any(|tab| {
                    tab.panes
                        .values()
                        .any(|pane| pane.attached_terminal_id == ticket.terminal_id)
                })
            });
            if terminal_still_attached
                || self.terminal_runtimes.get(&ticket.terminal_id).is_some()
                || observed_process_exists(receipt.shell_pid)
            {
                return encode_error(
                    id,
                    "startup_ownership_mismatch",
                    "terminal moved or shell PID is still present; cleanup refused",
                );
            }
        } else {
            // An acknowledgement proves this submitted script returned to the
            // shell. Unconsumed queued input cannot be classified as a failure.
            if ticket.submitted && !ticket.script.as_ref().is_some_and(StartupScript::finished) {
                return encode_error(
                    id,
                    "startup_execution_uncertain",
                    "startup has not acknowledged returning; cleanup refused",
                );
            }
            if !self.startup_shell_only(ticket) || !self.startup_shell_only(ticket) {
                return encode_error(
                    id,
                    "startup_ownership_mismatch",
                    "fresh shell-only ownership evidence unavailable; cleanup refused",
                );
            }
            // No await or subsequent API dispatch between identity checks and
            // close. The original receipt, never a name/cwd lookup, selects it.
            if let Err(error) = self.close_pane(
                id.clone(),
                &PaneTarget {
                    pane_id: receipt.pane_id.clone(),
                },
            ) {
                return error;
            }
        }
        if let Some(ticket) = self.startup_tickets.get_mut(&receipt.ticket) {
            ticket.script.take();
            ticket.closed_at = Some(std::time::Instant::now());
            ticket.cleanup_completed = true;
        }
        encode_success(
            id,
            ResponseResult::AgentStartup {
                receipt,
                state: "cleaned".into(),
            },
        )
    }
}

fn observed_shell_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(value) = TEST_CWD.with(|value| value.borrow().clone()) {
        return value;
    }
    crate::platform::process_cwd(pid)
}

fn observed_process_exists(pid: u32) -> bool {
    #[cfg(test)]
    if let Some(value) = TEST_PROCESS_EXISTS.with(|value| *value.borrow()) {
        return value;
    }
    crate::platform::process_exists(pid)
}

// Tests can simulate kernel PID reuse without editing the client's receipt or
// the daemon's stored ticket. The override is thread-local and test-only.
fn observed_shell_lifetime(pid: u32) -> Option<String> {
    #[cfg(test)]
    if let Some(value) = TEST_LIFETIME.with(|value| value.borrow().clone()) {
        return Some(value);
    }
    crate::platform::startup_process_lifetime(pid)
}

#[cfg(test)]
thread_local! {
    static TEST_CWD: std::cell::RefCell<Option<Option<std::path::PathBuf>>> = const { std::cell::RefCell::new(None) };
    static TEST_PROCESS_EXISTS: std::cell::RefCell<Option<bool>> = const { std::cell::RefCell::new(None) };
    static TEST_LIFETIME: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request, WorkspaceCreateParams};

    struct Fixture(App);
    impl Drop for Fixture {
        fn drop(&mut self) {
            TEST_CWD.with(|value| *value.borrow_mut() = None);
            TEST_LIFETIME.with(|value| *value.borrow_mut() = None);
            TEST_PROCESS_EXISTS.with(|value| *value.borrow_mut() = None);
            for (_, runtime) in self.0.terminal_runtimes.drain() {
                runtime.shutdown();
            }
        }
    }

    #[tokio::test]
    async fn startup_cleanup_observes_changed_identity_session_and_binding() {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fixture = Fixture(App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        ));
        let app = &mut fixture.0;
        app.state.default_shell = "/bin/sh".into();
        app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
        for label in ["owned", "sentinel"] {
            let response = app.handle_api_request(Request {
                id: label.into(),
                method: Method::WorkspaceCreate(WorkspaceCreateParams {
                    cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
                    focus: false,
                    label: Some(label.into()),
                    env: Default::default(),
                }),
            });
            let value: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert!(value.get("error").is_none(), "{value}");
        }
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = app.pane_info(0, pane).unwrap().pane_id;
        let start = AgentStartParams {
            name: "owned".into(),
            kind: "cursor-agent".into(),
            pane_id,
            args: vec![],
            timeout_ms: Some(6000),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let receipt: StartupReceipt = loop {
            let value: serde_json::Value =
                serde_json::from_str(&app.prepare_startup("prepare".into(), start.clone(), vec![]))
                    .unwrap();
            if value.get("error").is_none() {
                assert_eq!(value["result"]["receipt"]["kind"], "cursor");
                assert_eq!(value["result"]["receipt"]["timeout_ms"], 6000);
                break serde_json::from_value(value["result"]["receipt"].clone()).unwrap();
            }
            assert_eq!(value["error"]["code"], "agent_pane_busy", "{value}");
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        // Handoff must retain issuing authority for every unfinished lifecycle,
        // including a submitted launch and a closed pane awaiting reconciliation.
        assert!(app.has_handoff_startup_hold());
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .submitted = true;
        assert!(app.has_handoff_startup_hold());
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .closed_at = Some(std::time::Instant::now());
        assert!(app.has_handoff_startup_hold());
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .cleanup_completed = true;
        assert!(!app.has_handoff_startup_hold());
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .cleanup_completed = false;
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .closed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(86_401));
        assert!(!app.has_handoff_startup_hold());
        let ticket = app.startup_tickets.get_mut(&receipt.ticket).unwrap();
        ticket.closed_at = None;
        ticket.submitted = false;
        // Inspect is observation only: a prepared ticket must never submit input.
        let inspect: AgentStartupParams = serde_json::from_value(serde_json::json!({
            "operation": "inspect", "receipt": receipt
        }))
        .expect("read-only startup inspection must be supported");
        let observed: serde_json::Value =
            serde_json::from_str(&app.handle_agent_startup("inspect".into(), inspect)).unwrap();
        assert_eq!(observed["result"]["state"], "prepared", "{observed}");
        assert!(!app.startup_tickets[&receipt.ticket].submitted);
        assert_eq!(app.state.workspaces.len(), 2);
        // An unchanged cached presentation directory cannot substitute for live proof.
        for live_cwd in [
            None,
            Some(std::path::PathBuf::from(&receipt.cwd).join("different")),
        ] {
            TEST_CWD.with(|value| *value.borrow_mut() = Some(live_cwd));
            assert_eq!(
                app.pane_info(0, pane).unwrap().cwd.as_deref(),
                Some(receipt.cwd.as_str())
            );
            let denied: serde_json::Value =
                serde_json::from_str(&app.inspect_startup("live-cwd".into(), receipt.clone()))
                    .unwrap();
            assert_eq!(denied["error"]["code"], "startup_ownership_mismatch");
            assert_eq!(app.startup_tickets[&receipt.ticket].receipt, receipt);
            assert!(!app.startup_tickets[&receipt.ticket].submitted);
        }
        TEST_CWD.with(|value| *value.borrow_mut() = None);
        let mut unknown = receipt.clone();
        unknown.ticket.push_str("-unknown");
        let unknown: serde_json::Value =
            serde_json::from_str(&app.inspect_startup("unknown".into(), unknown)).unwrap();
        assert_eq!(unknown["error"]["code"], "startup_ticket_unknown");
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .submitted = true;
        let submitted: serde_json::Value =
            serde_json::from_str(&app.inspect_startup("submitted".into(), receipt.clone()))
                .unwrap();
        assert_eq!(submitted["result"]["state"], "submitted");
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .submitted = false;
        // Invalid pre-input timeouts must never create/reserve a ticket.
        for timeout in [0, 3000, 300001] {
            let mut invalid = start.clone();
            invalid.timeout_ms = Some(timeout);
            let value: serde_json::Value =
                serde_json::from_str(&app.prepare_startup("invalid".into(), invalid, vec![]))
                    .unwrap();
            assert_eq!(value["error"]["code"], "invalid_agent_timeout", "{value}");
        }
        // The launch contract is part of the original receipt proof.
        for change_kind in [true, false] {
            let mut forged = receipt.clone();
            if change_kind {
                forged.kind = "pi".into();
            } else {
                forged.timeout_ms += 1;
            }
            for response in [
                app.inspect_startup("forged".into(), forged.clone()),
                app.launch_startup("forged".into(), forged.clone()),
                app.cleanup_startup("forged".into(), forged),
            ] {
                let value: serde_json::Value = serde_json::from_str(&response).unwrap();
                assert_eq!(value["error"]["code"], "startup_ownership_mismatch");
            }
            assert_eq!(app.startup_tickets[&receipt.ticket].receipt, receipt);
            assert!(!app.startup_tickets[&receipt.ticket].submitted);
        }
        let terminal_id = app.startup_tickets[&receipt.ticket].terminal_id.clone();
        // A different terminal acquiring the reserved name cannot strand this
        // unsubmitted receipt or authorize cleanup of either terminal.
        let sentinel_id = app.state.workspaces[1].tabs[0]
            .panes
            .values()
            .next()
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&sentinel_id)
            .unwrap()
            .agent_name = Some(receipt.name.clone());
        let rejected: serde_json::Value =
            serde_json::from_str(&app.launch_startup("conflict".into(), receipt.clone())).unwrap();
        assert_eq!(rejected["error"]["code"], "agent_name_taken", "{rejected}");
        assert!(!app.startup_tickets[&receipt.ticket].submitted);
        let rejected: serde_json::Value =
            serde_json::from_str(&app.cleanup_startup("conflict".into(), receipt.clone())).unwrap();
        assert_eq!(
            rejected["error"]["code"], "startup_ownership_mismatch",
            "{rejected}"
        );
        assert_eq!(app.state.workspaces.len(), 2);
        app.state
            .terminals
            .get_mut(&sentinel_id)
            .unwrap()
            .agent_name = None;
        // Positive control makes every refusal below load-bearing.
        assert!(app.startup_shell_only(&app.startup_tickets[&receipt.ticket]));
        for case in ["lifetime", "session", "agent", "name", "binding"] {
            let sentinel_terminal = app.state.workspaces[1].tabs[0]
                .panes
                .values()
                .next()
                .unwrap()
                .attached_terminal_id
                .clone();
            match case {
                "lifetime" => TEST_LIFETIME
                    .with(|value| *value.borrow_mut() = Some("simulated-reused-pid-birth".into())),
                "session" => {
                    app.state
                        .terminals
                        .get_mut(&terminal_id)
                        .unwrap()
                        .persisted_agent_session =
                        Some(crate::agent_resume::PersistedAgentSession {
                            source: "codex".into(),
                            agent: "codex".into(),
                            session_ref: crate::agent_resume::AgentSessionRef::id(
                                "different-session",
                            )
                            .unwrap(),
                        })
                }
                "agent" => {
                    app.state
                        .terminals
                        .get_mut(&terminal_id)
                        .unwrap()
                        .detected_agent = Some(crate::detect::Agent::Claude)
                }
                "name" => {
                    app.state
                        .terminals
                        .get_mut(&terminal_id)
                        .unwrap()
                        .agent_name = Some("different-owner".into())
                }
                "binding" => {
                    app.state.workspaces[0].tabs[0]
                        .panes
                        .get_mut(&pane)
                        .unwrap()
                        .attached_terminal_id = sentinel_terminal
                }
                _ => unreachable!(),
            }
            let observed: serde_json::Value =
                serde_json::from_str(&app.inspect_startup(case.into(), receipt.clone())).unwrap();
            assert_eq!(
                observed["error"]["code"], "startup_ownership_mismatch",
                "{case}: {observed}"
            );
            let response: serde_json::Value =
                serde_json::from_str(&app.cleanup_startup(case.into(), receipt.clone())).unwrap();
            assert_eq!(
                response["error"]["code"], "startup_ownership_mismatch",
                "{case}: {response}"
            );
            assert_eq!(app.state.workspaces.len(), 2, "{case}");
            assert!(app.terminal_runtimes.get(&terminal_id).is_some(), "{case}");
            assert!(std::path::Path::new(&receipt.ticket).exists(), "{case}");
            assert_eq!(app.startup_tickets[&receipt.ticket].receipt, receipt);
            TEST_LIFETIME.with(|value| *value.borrow_mut() = None);
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = None;
            terminal.persisted_agent_session = None;
            terminal.agent_name = None;
            app.state.workspaces[0].tabs[0]
                .panes
                .get_mut(&pane)
                .unwrap()
                .attached_terminal_id = terminal_id.clone();
            assert!(
                app.startup_shell_only(&app.startup_tickets[&receipt.ticket]),
                "{case}"
            );
        }
        let response: serde_json::Value =
            serde_json::from_str(&app.cleanup_startup("cleanup".into(), receipt.clone())).unwrap();
        assert_eq!(response["result"]["state"], "cleaned", "{response}");
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(!std::path::Path::new(&receipt.ticket).exists());
        TEST_PROCESS_EXISTS.with(|value| *value.borrow_mut() = Some(false));
        let closed: serde_json::Value =
            serde_json::from_str(&app.inspect_startup("closed".into(), receipt.clone())).unwrap();
        assert_eq!(closed["result"]["state"], "closed", "{closed}");
        // Simulate the kernel observing a reused/live PID without editing either
        // original receipt or daemon ticket. Completed proof ignores this fact.
        TEST_PROCESS_EXISTS.with(|value| *value.borrow_mut() = Some(true));
        let reused: serde_json::Value =
            serde_json::from_str(&app.inspect_startup("reused".into(), receipt.clone())).unwrap();
        assert_eq!(reused["error"]["code"], "startup_ownership_mismatch");
        // Immediate retry must not depend on the terminated shell being reaped.
        for _ in 0..3 {
            let value: serde_json::Value =
                serde_json::from_str(&app.cleanup_startup("retry".into(), receipt.clone()))
                    .unwrap();
            assert_eq!(value["result"]["state"], "cleaned", "{value}");
            assert_eq!(app.state.workspaces.len(), 1);
        }
        app.state
            .terminals
            .get_mut(&sentinel_id)
            .unwrap()
            .agent_name = Some(receipt.name.clone());
        let rebound: serde_json::Value =
            serde_json::from_str(&app.cleanup_startup("rebound".into(), receipt.clone())).unwrap();
        assert_eq!(
            rebound["error"]["code"], "startup_ownership_mismatch",
            "{rebound}"
        );
        assert_eq!(app.state.workspaces.len(), 1);
        app.state
            .terminals
            .get_mut(&sentinel_id)
            .unwrap()
            .agent_name = None;
        TEST_PROCESS_EXISTS.with(|value| *value.borrow_mut() = None);
        let mut forged = receipt.clone();
        forged.shell_lifetime.push('x');
        let value: serde_json::Value =
            serde_json::from_str(&app.cleanup_startup("forged".into(), forged)).unwrap();
        assert_eq!(value["error"]["code"], "startup_ownership_mismatch");
        app.startup_tickets
            .get_mut(&receipt.ticket)
            .unwrap()
            .closed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(86_401));
        let expired: serde_json::Value = serde_json::from_str(
            &app.handle_agent_startup("expired".into(), AgentStartupParams::Cleanup { receipt }),
        )
        .unwrap();
        assert_eq!(expired["error"]["code"], "startup_ticket_unknown");
        assert!(app.startup_tickets.is_empty());
    }
}

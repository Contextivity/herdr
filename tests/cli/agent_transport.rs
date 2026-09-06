use super::harness::*;

#[test]
fn agent_start_waits_through_unknown_then_rejects_blocked() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut pane_stream, pane_line) = accept_fake_cli_operation(&listener);
        let pane: serde_json::Value = serde_json::from_str(&pane_line).unwrap();
        assert_eq!(pane["method"], "pane.get");
        assert_eq!(pane["params"]["pane_id"], "w1:p1");
        writeln!(
            pane_stream,
            "{}",
            serde_json::json!({
                "id": pane["id"],
                "result": {
                    "type": "pane_info",
                    "pane": { "terminal_id": "term_1" }
                }
            })
        )
        .unwrap();
        pane_stream.flush().unwrap();

        let (mut start_stream, start_line) = accept_fake_cli_operation(&listener);
        let start: serde_json::Value = serde_json::from_str(&start_line).unwrap();
        assert_eq!(start["method"], "agent.start");
        writeln!(
            start_stream,
            "{}",
            serde_json::json!({
                "id": start["id"],
                "result": {
                    "type": "agent_started",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer"
                    },
                    "argv": ["opencode"]
                }
            })
        )
        .unwrap();
        start_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": null,
                        "agent_status": "unknown",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": "opencode",
                        "agent_status": "blocked",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent", "start", "reviewer", "--kind", "opencode", "--pane", "w1:p1",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    assert!(started.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_ready");

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn agent_start_does_not_retry_after_the_target_terminal_changes() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, line) = accept_fake_cli_operation(&listener);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "pane.get");
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "id": request["id"],
                    "result": {
                        "type": "pane_info",
                        "pane": { "terminal_id": "term_1" }
                    }
                })
            )
            .unwrap();
            stream.flush().unwrap();

            let (mut stream, line) = accept_fake_cli_operation(&listener);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if request["method"] == "agent.start" {
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "id": request["id"],
                        "error": {
                            "code": "agent_pane_busy",
                            "message": "agent target pane w1:p1 is not an available shell"
                        }
                    })
                )
                .unwrap();
                stream.flush().unwrap();
            } else {
                assert_eq!(request["method"], "pane.process_info");
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "id": request["id"],
                        "result": {
                            "type": "pane_process_info",
                            "process_info": {
                                "pane_id": "w1:p1",
                                "shell_pid": 10,
                                "foreground_process_group_id": 10,
                                "foreground_processes": [
                                    { "pid": 10, "name": "bash" },
                                    { "pid": 11, "name": "startup-helper" }
                                ]
                            }
                        }
                    })
                )
                .unwrap();
                stream.flush().unwrap();
            }
        }

        let (mut stream, line) = accept_fake_cli_operation(&listener);
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "pane.get");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": request["id"],
                "result": {
                    "type": "pane_info",
                    "pane": { "terminal_id": "term_2" }
                }
            })
        )
        .unwrap();
        stream.flush().unwrap();
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent", "start", "reviewer", "--kind", "pi", "--pane", "w1:p1",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_pane_busy");

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn prompt_wait_is_sent_as_one_agent_request() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut prompt_stream, prompt_line) = accept_fake_cli_operation(&listener);
        let prompt: serde_json::Value = serde_json::from_str(&prompt_line).unwrap();
        assert_eq!(prompt["method"], "agent.prompt");
        assert_eq!(prompt["params"]["target"], "w1:p1");
        assert_eq!(
            prompt["params"]["wait"]["until"],
            serde_json::json!(["idle"])
        );
        assert!(prompt["params"]["wait"].get("timeout_ms").is_none());
        writeln!(
            prompt_stream,
            "{}",
            serde_json::json!({
                "id": prompt["id"],
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer",
                        "agent": "pi",
                        "agent_status": "idle",
                        "workspace_id": "w1",
                        "tab_id": "w1:t1",
                        "focused": true,
                        "revision": 0
                    }
                }
            })
        )
        .unwrap();
        prompt_stream.flush().unwrap();
    });

    let prompted = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "w1:p1",
            "review this",
            "--wait",
            "--until",
            "idle",
        ],
    );
    assert!(
        prompted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prompted.stderr)
    );
    let prompted: serde_json::Value = serde_json::from_slice(&prompted.stdout).unwrap();
    assert_eq!(prompted["result"]["agent"]["name"], "reviewer");

    server.join().unwrap();
    cleanup_test_base(&base);
}

// Exercise the real stdin CLI with one accepted launch, without invoking an agent.
fn startup_cli_fixture(scenario: &'static str) -> std::process::Output {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("startup.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let receipt = serde_json::json!({
        "ticket": "ticket", "pane_id": "w1:p1", "workspace_id": "w1",
        "terminal_id": "term_1", "cwd": "/tmp", "name": "worker",
        "shell_pid": 42, "shell_lifetime": "birth", "kind": "pi", "timeout_ms": 3100
    });
    let expected_receipt = receipt.clone();
    let server = thread::spawn(move || {
        let (mut stream, line) = accept_fake_cli_operation(&listener);
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "agent.startup");
        assert_eq!(request["params"]["receipt"], expected_receipt);
        if scenario == "send_error" {
            writeln!(stream, "invalid json").unwrap();
            return;
        }
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": request["id"], "result": {"type": "agent_started",
                    "agent": {"terminal_id": "term_1", "name": "worker", "agent": null}}
            })
        )
        .unwrap();
        drop(stream);
        let start = Instant::now();
        loop {
            let (mut stream, line) = accept_fake_cli_operation(&listener);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            // Any second launch is a failure, including after transport errors.
            assert_eq!(request["method"], "agent.get");
            if scenario == "wait_error" {
                writeln!(stream, "invalid json").unwrap();
                break;
            }
            let expired = request["id"] == "cli:agent:start:timeout";
            let ready = scenario != "timeout" || start.elapsed() > Duration::from_secs(4);
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "id": request["id"], "result": {"type": "agent_info", "agent": {
                        "terminal_id": "term_1", "pane_id": "w1:p1", "name": "worker",
                        "agent": if scenario == "timeout" { None } else { Some("pi") },
                        "agent_status": if ready { "idle" } else { "unknown" },
                        "interactive_ready": ready, "launch_pending": !ready
                    }}
                })
            )
            .unwrap();
            if ready || expired {
                break;
            }
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["agent", "startup"])
        .env("HERDR_SOCKET_PATH", &socket_path)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        serde_json::json!({
            "operation": "launch", "receipt": receipt
        })
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    cleanup_test_base(&base);
    output
}

#[test]
fn startup_cli_preserves_prepared_non_codex_kind_without_detection() {
    let output = startup_cli_fixture("ready");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["result"]["agent"]["agent"], "pi");
}

#[test]
fn startup_cli_preserves_prepared_non_default_timeout_without_detection() {
    let output = startup_cli_fixture("timeout");
    assert_eq!(output.status.code(), Some(1));
    let response: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(response["error"]["code"], "timeout");
}

#[test]
fn startup_cli_structures_readiness_transport_error_without_resend() {
    let output = startup_cli_fixture("wait_error");
    assert_eq!(output.status.code(), Some(1));
    let response: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(response["error"]["code"], "agent_start_transport_failed");
}

#[test]
fn startup_cli_structures_initial_transport_error_without_resend() {
    let output = startup_cli_fixture("send_error");
    assert_eq!(output.status.code(), Some(1));
    let response: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(response["error"]["code"], "agent_start_transport_failed");
}

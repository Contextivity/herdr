#![cfg(target_os = "linux")]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Runtime {
    base: PathBuf,
    api: PathBuf,
    child: Child,
}

impl Runtime {
    fn start(replacement_race: bool) -> Self {
        let base = std::env::temp_dir().join(format!(
            "hals-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("runtime")).unwrap();
        fs::create_dir_all(base.join("home")).unwrap();
        for app in ["herdr", "herdr-dev"] {
            let config = base.join("config").join(app);
            fs::create_dir_all(&config).unwrap();
            fs::write(
                config.join("config.toml"),
                "onboarding = false\n[updates]\nauto_check = false\n",
            )
            .unwrap();
        }
        let shim = base.join("fault.so");
        assert!(Command::new("cc")
            .args(["-shared", "-fPIC", "-O2"])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/api-listener-fault.c"
            ))
            .arg("-ldl")
            .arg("-o")
            .arg(&shim)
            .status()
            .unwrap()
            .success());
        let binary = base.join("herdr");
        fs::copy(env!("CARGO_BIN_EXE_herdr"), &binary).unwrap();
        let api = base.join("runtime/herdr.sock");
        let child = Self::spawn(&base, &api, replacement_race);
        support::register_runtime_dir(&base.join("runtime"));
        let runtime = Self { base, api, child };
        wait_until(|| runtime.request("ping", json!({})).is_ok());
        runtime
    }

    fn spawn(base: &std::path::Path, api: &std::path::Path, replacement_race: bool) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("stderr.log"))
            .unwrap();
        let binary = base.join("herdr");
        let mut command = Command::new(&binary);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(key);
            }
        }
        command
            .arg("server")
            .env("HOME", base.join("home"))
            .env(
                "PATH",
                format!("{}:{}", base.display(), std::env::var("PATH").unwrap()),
            )
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_RUNTIME_DIR", base.join("runtime"))
            .env("TMPDIR", base)
            .env("SHELL", "/bin/sh")
            .env("TOKIO_WORKER_THREADS", "2")
            .env("HERDR_SOCKET_PATH", api)
            .env("LD_PRELOAD", base.join("fault.so"))
            .env("HERDR_REPRO_ACCEPT_TRIGGER", base.join("fail-accept"))
            .env("HERDR_REPRO_WORKER_TRIGGER", base.join("fail-worker"))
            .env("HERDR_REPRO_BIND_TRIGGER", base.join("fail-bind"))
            .env("HERDR_REPRO_BIND_REACHED", base.join("bind-reached"))
            .env("HERDR_REPRO_BIND_RELEASE", base.join("bind-release"))
            .env(
                "HERDR_REPRO_HANDOFF_ADMISSION_TRIGGER",
                base.join("handoff-admission"),
            )
            .env(
                "HERDR_REPRO_HANDOFF_ADMISSION_REACHED",
                base.join("handoff-admission-reached"),
            )
            .env(
                "HERDR_REPRO_HANDOFF_ADMISSION_RELEASE",
                base.join("handoff-admission-release"),
            )
            .env(
                "HERDR_REPRO_HANDOFF_CLIENT_FAILURE",
                base.join("handoff-client-failure"),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        if replacement_race {
            command
                .env("HERDR_REPRO_ACCEPT_REACHED", base.join("accept-reached"))
                .env("HERDR_REPRO_ACCEPT_RELEASE", base.join("accept-release"));
        }
        let child = command.spawn().unwrap();
        support::register_spawned_herdr_pid(Some(child.id()));
        child
    }

    fn request(&self, method: &str, params: Value) -> std::io::Result<Value> {
        let mut stream = UnixStream::connect(&self.api)?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        writeln!(
            stream,
            "{}",
            json!({"id":"survival", "method":method, "params":params})
        )?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        serde_json::from_str(&line).map_err(std::io::Error::other)
    }

    fn trigger_accept_failure(&self) {
        fs::write(self.base.join("fail-accept"), "").unwrap();
        let _ = self.request("ping", json!({}));
        wait_until(|| !self.base.join("fail-accept").exists());
    }

    fn logs(&self) -> String {
        ["herdr", "herdr-dev"]
            .iter()
            .filter_map(|app| {
                fs::read_to_string(self.base.join("config").join(app).join("herdr-server.log")).ok()
            })
            .collect()
    }

    fn create_pane(&self) -> Value {
        self.request("workspace.create", json!({"cwd":self.base,"focus":false}))
            .unwrap()["result"]["root_pane"]["pane_id"]
            .clone()
    }

    fn prepare(&self, pane_id: &Value) -> Value {
        let mut receipt = Value::Null;
        wait_until(|| {
            let prepared = self.request("agent.startup", json!({"operation":"prepare", "start":{
                "name":"survival", "kind":"codex", "pane_id":pane_id, "args":[], "timeout_ms":60000}, "preparation":[]})).unwrap();
            receipt = prepared["result"]["receipt"].clone();
            !receipt.is_null()
        });
        receipt
    }

    fn fixture_agent(&self) -> Value {
        let script = self.base.join("pi");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nexport HERDR_AGENT=pi\nprintf started > '{}'\nexec /bin/sleep 300\n",
                self.base.join("agent-started").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let pane_id = self.create_pane();
        assert!(self
            .request(
                "pane.send_input",
                json!({"pane_id":pane_id,"text":script,"keys":["Enter"]})
            )
            .unwrap()
            .get("error")
            .is_none());
        wait_until(|| self.base.join("agent-started").exists());
        let native = self.base.join("native-session.jsonl");
        fs::write(&native, "fixture native session\n").unwrap();
        assert!(self.request("pane.report_agent_session", json!({"pane_id":pane_id,"source":"herdr:pi","agent":"pi","seq":1,"agent_session_path":native,"session_start_source":"startup"})).unwrap().get("error").is_none());
        wait_until(|| {
            self.request("pane.get", json!({"pane_id":pane_id}))
                .unwrap()["result"]["pane"]["agent_session"]["value"]
                == json!(native)
        });
        pane_id
    }

    fn restart(&mut self) {
        support::unregister_spawned_herdr_pid(Some(self.child.id()));
        self.child = Self::spawn(&self.base, &self.api, false);
    }

    fn stop(&mut self) {
        assert_eq!(
            self.request("server.stop", json!({})).unwrap()["result"]["type"],
            "ok"
        );
        wait_until(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
    }

    fn session_path(&self) -> PathBuf {
        ["herdr-dev", "herdr"]
            .iter()
            .map(|app| self.base.join("config").join(app).join("session.json"))
            .find(|path| path.exists())
            .expect("shutdown must save a session")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        support::unregister_spawned_herdr_pid(Some(self.child.id()));
        support::cleanup_test_base(&self.base);
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_json(stream: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn subscribe(runtime: &Runtime) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(&runtime.api).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream, "{}", json!({"id":"subscription","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}})).unwrap();
    let mut stream = BufReader::new(stream);
    assert_eq!(
        read_json(&mut stream)["result"]["type"],
        "subscription_started"
    );
    stream
}

#[test]
fn api_listener_failure_preserves_runtime_and_receipt_authority() {
    let mut runtime = Runtime::start(false);
    let pane_id = runtime.create_pane();
    let pane_before = runtime
        .request("pane.get", json!({"pane_id":pane_id}))
        .unwrap()["result"]["pane"]
        .clone();
    let receipt = runtime.prepare(&pane_id);
    let artifact = PathBuf::from(receipt["ticket"].as_str().unwrap());
    let script_bytes = fs::read(artifact.join("launch")).unwrap();
    let process_before = runtime
        .request("pane.process_info", json!({"pane_id":pane_id}))
        .unwrap()["result"]["process_info"]
        .clone();
    assert_eq!(process_before["shell_pid"], receipt["shell_pid"]);
    let shell_pid = receipt["shell_pid"].as_u64().unwrap();
    let tty = fs::read_link(format!("/proc/{shell_pid}/fd/0")).unwrap();
    let agent_pane = runtime.fixture_agent();
    let agent_before = runtime
        .request("pane.get", json!({"pane_id":agent_pane}))
        .unwrap()["result"]["pane"]
        .clone();
    let agent_process = runtime
        .request("pane.process_info", json!({"pane_id":agent_pane}))
        .unwrap()["result"]["process_info"]
        .clone();
    let mut subscription = subscribe(&runtime);
    // Replacing the executable alone must leave the old runtime controllable.
    fs::copy(
        env!("CARGO_BIN_EXE_herdr"),
        runtime.base.join("replacement"),
    )
    .unwrap();
    fs::rename(runtime.base.join("replacement"), runtime.base.join("herdr")).unwrap();
    assert_eq!(
        runtime.request("ping", json!({})).unwrap()["result"]["type"],
        "pong"
    );

    runtime.trigger_accept_failure();
    wait_until(|| runtime.logs().contains("api.listener.recovered"));
    assert!(
        runtime.child.try_wait().unwrap().is_none(),
        "same daemon must survive"
    );
    let pane_after = runtime
        .request("pane.get", json!({"pane_id":pane_id}))
        .unwrap()["result"]["pane"]
        .clone();
    for field in ["pane_id", "terminal_id", "workspace_id", "tab_id"] {
        assert_eq!(pane_before[field], pane_after[field]);
    }
    assert_eq!(
        runtime
            .request("pane.process_info", json!({"pane_id":pane_id}))
            .unwrap()["result"]["process_info"],
        process_before
    );
    assert_eq!(
        fs::read_link(format!("/proc/{shell_pid}/fd/0")).unwrap(),
        tty
    );
    assert_eq!(fs::read(artifact.join("launch")).unwrap(), script_bytes);
    let agent_after = runtime
        .request("pane.get", json!({"pane_id":agent_pane}))
        .unwrap()["result"]["pane"]
        .clone();
    for field in [
        "pane_id",
        "terminal_id",
        "workspace_id",
        "tab_id",
        "agent_session",
    ] {
        assert_eq!(agent_before[field], agent_after[field]);
    }
    assert_eq!(
        runtime
            .request("pane.process_info", json!({"pane_id":agent_pane}))
            .unwrap()["result"]["process_info"],
        agent_process
    );
    let created = runtime.create_pane();
    let event = read_json(&mut subscription);
    assert_eq!(event["event"], "workspace_created", "{event}");
    assert_eq!(
        event["data"]["workspace"]["workspace_id"],
        created.as_str().unwrap().split(':').next().unwrap()
    );
    let inspected = runtime
        .request(
            "agent.startup",
            json!({"operation":"inspect","receipt":receipt}),
        )
        .unwrap();
    assert_eq!(inspected["result"]["state"], "prepared", "{inspected}");
    assert_eq!(inspected["result"]["receipt"], receipt);
    for (field, value) in [
        ("terminal_id", json!("term_ffffffff")),
        ("shell_pid", json!(1)),
        ("shell_lifetime", json!("0")),
        ("name", json!("forged")),
        ("cwd", json!("/")),
    ] {
        let mut forged = receipt.clone();
        forged[field] = value;
        for operation in ["inspect", "launch", "cleanup"] {
            let rejected = runtime
                .request(
                    "agent.startup",
                    json!({"operation":operation,"receipt":forged}),
                )
                .unwrap();
            assert_eq!(
                rejected["error"]["code"], "startup_ownership_mismatch",
                "{rejected}"
            );
        }
    }
    runtime.stop();
}

#[test]
fn api_listener_worker_admission_failure_is_fatal() {
    let mut runtime = Runtime::start(false);
    fs::write(runtime.base.join("fail-worker"), "").unwrap();
    let _ = runtime.request("ping", json!({}));
    wait_until(|| runtime.child.try_wait().unwrap().is_some());
    assert!(
        !runtime.child.wait().unwrap().success(),
        "worker admission failure must reach supervision"
    );
    assert!(runtime.logs().contains("api.connection.spawn_failed"));
}

#[test]
fn api_listener_recovery_refuses_replaced_socket_and_exits_failure() {
    for replacement_kind in ["socket", "symlink", "file"] {
        let mut runtime = Runtime::start(true);
        runtime.trigger_accept_failure();
        wait_until(|| runtime.base.join("accept-reached").exists());
        let displaced = runtime.base.join("runtime/displaced.sock");
        let replacement = if replacement_kind == "symlink" {
            // A symlink to the old owned inode is still a foreign pathname
            // generation. Identity lookup must not follow it and adopt it.
            fs::rename(&runtime.api, &displaced).unwrap();
            std::os::unix::fs::symlink(&displaced, &runtime.api).unwrap();
            None
        } else {
            fs::remove_file(&runtime.api).unwrap();
            if replacement_kind == "socket" {
                Some(UnixListener::bind(&runtime.api).unwrap())
            } else {
                fs::write(&runtime.api, "foreign file").unwrap();
                None
            }
        };
        let before = fs::symlink_metadata(&runtime.api).unwrap();
        fs::write(runtime.base.join("accept-release"), "").unwrap();
        wait_until(|| {
            runtime.child.try_wait().unwrap().is_some()
                || fs::symlink_metadata(&runtime.api)
                    .map(|metadata| {
                        metadata.ino() != before.ino() || metadata.file_type() != before.file_type()
                    })
                    .unwrap_or(true)
        });
        assert_eq!(
            fs::symlink_metadata(&runtime.api).unwrap().ino(),
            before.ino(),
            "foreign pathname must survive recovery and cleanup: {replacement_kind}"
        );
        assert!(
            runtime.child.try_wait().unwrap().is_some(),
            "foreign replacement must cause fatal shutdown"
        );
        assert!(
            !runtime.child.wait().unwrap().success(),
            "fatal recovery must be visible to supervision"
        );
        if replacement_kind == "symlink" {
            assert_eq!(fs::read_link(&runtime.api).unwrap(), displaced);
            assert!(displaced.exists());
        } else if replacement_kind == "socket" {
            assert!(UnixStream::connect(&runtime.api).is_ok());
        } else {
            assert_eq!(fs::read_to_string(&runtime.api).unwrap(), "foreign file");
        }
        assert!(runtime.logs().contains("api.listener.recovery_failed"));
        drop(replacement);
    }
}

#[test]
fn api_listener_competitor_wins_after_unlink_before_bind() {
    let mut runtime = Runtime::start(false);
    fs::write(runtime.base.join("fail-bind"), "").unwrap();
    runtime.trigger_accept_failure();
    wait_until(|| runtime.base.join("bind-reached").exists());
    assert!(!runtime.api.exists(), "barrier must be after unlink");
    let competitor = UnixListener::bind(&runtime.api).unwrap();
    let inode = fs::metadata(&runtime.api).unwrap().ino();
    fs::write(runtime.base.join("bind-release"), "").unwrap();
    wait_until(|| runtime.child.try_wait().unwrap().is_some());
    assert!(!runtime.child.wait().unwrap().success());
    assert_eq!(fs::metadata(&runtime.api).unwrap().ino(), inode);
    assert!(UnixStream::connect(&runtime.api).is_ok());
    assert!(runtime.logs().contains("api.listener.recovery_failed"));
    drop(competitor);
}

#[test]
fn api_listener_capacity_bounds_workers_and_releases_admission() {
    let mut runtime = Runtime::start(false);
    // Each acknowledged stream remains active. Opening the next stream only
    // after its ACK makes occupancy deterministic without a production seam.
    let mut streams = Vec::new();
    let baseline_threads = fs::read_dir(format!("/proc/{}/task", runtime.child.id()))
        .unwrap()
        .count();
    for _ in 0..128 {
        streams.push(subscribe(&runtime));
    }
    let occupied_threads = fs::read_dir(format!("/proc/{}/task", runtime.child.id()))
        .unwrap()
        .count();
    assert!(
        occupied_threads <= baseline_threads + 128,
        "threads grew beyond admitted connections"
    );
    let mut excess = UnixStream::connect(&runtime.api).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut line = String::new();
    assert_eq!(
        BufReader::new(&mut excess).read_line(&mut line).unwrap(),
        0,
        "excess connection must close promptly"
    );
    assert!(runtime.logs().contains("api.connection.capacity"));
    assert!(runtime.child.try_wait().unwrap().is_none());
    drop(streams);
    wait_until(|| runtime.request("ping", json!({})).is_ok());
    runtime.stop();
}

#[test]
fn api_listener_fatal_exit_retains_artifact_and_native_reference_without_ticket_adoption() {
    let mut runtime = Runtime::start(false);
    let pane = runtime.create_pane();
    let receipt = runtime.prepare(&pane);
    let artifact = PathBuf::from(receipt["ticket"].as_str().unwrap());
    let bytes = fs::read(artifact.join("launch")).unwrap();
    let agent = runtime.fixture_agent();
    let native_before = runtime
        .request("pane.get", json!({"pane_id":agent}))
        .unwrap()["result"]["pane"]["agent_session"]
        .clone();
    assert!(!native_before.is_null());
    fs::write(runtime.base.join("fail-worker"), "").unwrap();
    let _ = runtime.request("ping", json!({}));
    wait_until(|| runtime.child.try_wait().unwrap().is_some());
    assert!(!runtime.child.wait().unwrap().success());
    assert_eq!(fs::read(artifact.join("launch")).unwrap(), bytes);
    assert!(artifact.join("finished").exists());
    let session = fs::read_to_string(runtime.session_path()).unwrap();
    let saved: Value = serde_json::from_str(&session).unwrap();
    assert!(saved["workspaces"].as_array().unwrap().len() >= 2);
    let saved_native = saved["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|workspace| workspace["tabs"].as_array().unwrap())
        .flat_map(|tab| tab["panes"].as_object().unwrap().values())
        .filter_map(|pane| pane.get("agent_session"))
        .find(|native| **native == native_before);
    assert!(
        saved_native.is_some(),
        "saved native reference absent: {session}"
    );
    runtime.restart();
    wait_until(|| runtime.request("ping", json!({})).is_ok());
    let snapshot = runtime.request("session.snapshot", json!({})).unwrap();
    assert!(
        snapshot["result"]["snapshot"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pane| pane["agent_session"] == native_before),
        "restored native reference absent: {snapshot}"
    );
    let restored = runtime
        .request("pane.get", json!({"pane_id":pane}))
        .unwrap()["result"]["pane"]
        .clone();
    assert_eq!(restored["pane_id"], receipt["pane_id"]);
    assert_eq!(restored["workspace_id"], receipt["workspace_id"]);
    assert_ne!(restored["terminal_id"], receipt["terminal_id"]);
    let new_process = runtime
        .request("pane.process_info", json!({"pane_id":pane}))
        .unwrap();
    assert_ne!(
        new_process["result"]["process_info"]["shell_pid"],
        receipt["shell_pid"]
    );
    // A restored native session is useful continuity data, never old authority.
    for operation in ["inspect", "launch", "cleanup"] {
        let refused = runtime
            .request(
                "agent.startup",
                json!({"operation":operation,"receipt":receipt}),
            )
            .unwrap();
        assert_eq!(
            refused["error"]["code"], "startup_ticket_unknown",
            "{refused}"
        );
        assert_eq!(fs::read(artifact.join("launch")).unwrap(), bytes);
    }
    runtime.stop();
}

#[test]
fn api_listener_duplicate_start_preserves_foreign_public_paths_before_restore() {
    let mut runtime = Runtime::start(false);
    runtime.fixture_agent();
    runtime.stop();
    let session_path = runtime.session_path();
    let session_bytes = fs::read(&session_path).unwrap();
    fs::remove_file(runtime.base.join("agent-started")).unwrap();
    for path in [
        runtime.base.join("runtime/herdr-client.sock"),
        runtime.api.clone(),
    ] {
        for accepting in [true, false] {
            let listener = UnixListener::bind(&path).unwrap();
            let listener = accepting.then_some(listener);
            let inode = fs::metadata(&path).unwrap().ino();
            runtime.restart();
            wait_until(|| {
                runtime.child.try_wait().unwrap().is_some()
                    || runtime.base.join("agent-started").exists()
                    || fs::metadata(&path).map(|metadata| metadata.ino()).ok() != Some(inode)
            });
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
            assert_eq!(fs::read(&session_path).unwrap(), session_bytes);
            assert!(
                !runtime.base.join("agent-started").exists(),
                "duplicate restored native agent before rejecting socket"
            );
            if path != runtime.api {
                assert!(
                    !runtime.api.exists(),
                    "duplicate published API before rejecting client path"
                );
            }
            wait_until(|| runtime.child.try_wait().unwrap().is_some());
            assert!(!runtime.child.wait().unwrap().success());
            drop(listener);
            fs::remove_file(&path).unwrap();
        }
    }
}

// The existing cross_area kill/restart/reconnect case owns successful cold
// restart. This case owns its security boundary before any native restoration.
#[test]
fn api_listener_cold_restart_requires_exact_private_endpoint_evidence() {
    let mut runtime = Runtime::start(false);
    runtime.fixture_agent();
    runtime.stop();
    let session = runtime.session_path();
    let saved = fs::read(&session).unwrap();
    runtime.restart();
    wait_until(|| runtime.request("ping", json!({})).is_ok());
    // Ping is available before the client bind/publication completes.
    wait_until(|| {
        runtime
            .base
            .join("runtime/herdr-client.sock.owner.json")
            .exists()
    });
    runtime.child.kill().unwrap();
    runtime.child.wait().unwrap();
    fs::remove_file(runtime.base.join("agent-started")).unwrap();
    let paths = [
        runtime.api.clone(),
        runtime.base.join("runtime/herdr-client.sock"),
    ];
    let inodes: Vec<_> = paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().ino())
        .collect();
    for path in &paths {
        let record = PathBuf::from(format!("{}.owner.json", path.display()));
        let original = fs::read(&record).unwrap();
        let owner: Value = serde_json::from_slice(&original).unwrap();
        for invalid in [
            "missing",
            "corrupt",
            "identity",
            "live_pid_reused",
            "lifetime",
            "boot",
            "permissions",
        ] {
            let mut changed = owner.clone();
            match invalid {
                "missing" => {
                    fs::remove_file(&record).unwrap();
                }
                "corrupt" => {
                    fs::write(&record, "{").unwrap();
                }
                "identity" => {
                    changed["identity"]["ino"] = json!(0);
                }
                "live_pid_reused" => {
                    changed["pid"] = json!(std::process::id());
                    changed["lifetime"] = json!("foreign-lifetime");
                }
                "lifetime" => {
                    changed["lifetime"] = Value::Null;
                }
                "boot" => {
                    changed["boot"] = json!("unavailable-or-other-boot");
                }
                "permissions" => {
                    fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
                }
                _ => unreachable!(),
            }
            if !matches!(invalid, "missing" | "corrupt" | "permissions") {
                fs::write(&record, serde_json::to_vec(&changed).unwrap()).unwrap();
            }
            runtime.restart();
            wait_until(|| runtime.child.try_wait().unwrap().is_some());
            assert!(
                !runtime.child.wait().unwrap().success(),
                "accepted {invalid} evidence for {path:?}"
            );
            for (endpoint, inode) in paths.iter().zip(&inodes) {
                assert_eq!(
                    fs::metadata(endpoint).unwrap().ino(),
                    *inode,
                    "partial reclamation with {invalid} evidence for {path:?}"
                );
            }
            assert_eq!(fs::read(&session).unwrap(), saved);
            assert!(
                !runtime.base.join("agent-started").exists(),
                "restored before rejecting {invalid}"
            );
            fs::write(&record, &original).unwrap();
            fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

#[test]
fn api_listener_startup_admission_serializes_pair_binding() {
    let mut runtime = Runtime::start(false);
    runtime.stop();
    // Pause the real first start at bind, after pair preflight. An admitted
    // competitor must wait rather than restore/publish half of another pair.
    fs::write(runtime.base.join("fail-bind"), "").unwrap();
    runtime.restart();
    wait_until(|| runtime.base.join("bind-reached").exists());
    let mut competitor = Runtime::spawn(&runtime.base, &runtime.api, false);
    thread::sleep(Duration::from_millis(150));
    assert!(
        competitor.try_wait().unwrap().is_none(),
        "competitor bypassed admission"
    );
    assert!(
        !runtime.api.exists(),
        "competitor published while first start holds admission"
    );
    fs::write(runtime.base.join("bind-release"), "").unwrap();
    wait_until(|| runtime.request("ping", json!({})).is_ok());
    wait_until(|| competitor.try_wait().unwrap().is_some());
    assert!(!competitor.wait().unwrap().success());
    support::unregister_spawned_herdr_pid(Some(competitor.id()));
    let inode = fs::metadata(&runtime.api).unwrap().ino();
    assert_eq!(
        runtime.request("ping", json!({})).unwrap()["result"]["type"],
        "pong"
    );
    assert_eq!(fs::metadata(&runtime.api).unwrap().ino(), inode);
    runtime.stop();
}

#[test]
fn api_listener_handoff_vacancy_is_reserved_against_ordinary_start() {
    let mut runtime = Runtime::start(false);
    let old_pid = runtime.child.id();
    fs::write(runtime.base.join("handoff-admission"), "").unwrap();
    let path = runtime.api.clone();
    let handoff = thread::spawn(move || {
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({"id":"handoff-vacancy","method":"server.live_handoff","params":{}})
        )
        .unwrap();
        read_json(&mut BufReader::new(stream))
    });
    wait_until(|| runtime.base.join("handoff-admission-reached").exists());
    assert!(
        !runtime.api.exists(),
        "replacement must be paused in the handoff vacancy"
    );
    let mut competitor = Runtime::spawn(&runtime.base, &runtime.api, false);
    wait_until(|| competitor.try_wait().unwrap().is_some() || runtime.api.exists());
    assert!(
        !runtime.api.exists(),
        "ordinary start published in a live handoff vacancy"
    );
    assert!(
        !competitor.wait().unwrap().success(),
        "ordinary start took a live handoff vacancy"
    );
    assert!(!runtime.api.exists());
    fs::write(runtime.base.join("handoff-admission-release"), "").unwrap();
    assert!(handoff.join().unwrap().get("error").is_none());
    support::unregister_spawned_herdr_pid(Some(competitor.id()));
    wait_until(|| runtime.child.try_wait().unwrap().is_some());
    assert!(runtime.child.wait().unwrap().success());
    let record: Value =
        serde_json::from_slice(&fs::read(format!("{}.owner.json", runtime.api.display())).unwrap())
            .unwrap();
    let replacement_pid = record["pid"].as_u64().unwrap() as u32;
    assert_ne!(replacement_pid, old_pid);
    support::register_spawned_herdr_pid(Some(replacement_pid));
    assert_eq!(
        runtime.request("ping", json!({})).unwrap()["result"]["type"],
        "pong"
    );
    runtime.request("server.stop", json!({})).unwrap();
    wait_until(|| !runtime.api.exists());
    support::unregister_spawned_herdr_pid(Some(replacement_pid));
}

#[test]
fn api_listener_handoff_publication_failure_restores_original_owner() {
    let mut runtime = Runtime::start(false);
    let pane = runtime.create_pane();
    let before = runtime
        .request("pane.process_info", json!({"pane_id":pane}))
        .unwrap()["result"]["process_info"]
        .clone();
    fs::write(runtime.base.join("handoff-client-failure"), "").unwrap();
    let failed = runtime.request("server.live_handoff", json!({}));
    // The API response may take longer than ordinary requests during rollback.
    assert!(failed.is_err() || failed.unwrap().get("error").is_some());
    wait_until(|| {
        !runtime.base.join("handoff-client-failure").exists()
            && runtime.request("ping", json!({})).is_ok()
            && fs::read(format!("{}.owner.json", runtime.api.display()))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|owner| owner["pid"] == runtime.child.id())
    });
    wait_until(|| runtime.request("ping", json!({})).is_ok());
    assert!(runtime.child.try_wait().unwrap().is_none());
    let after = runtime
        .request("pane.process_info", json!({"pane_id":pane}))
        .unwrap()["result"]["process_info"]
        .clone();
    assert_eq!(after["shell_pid"], before["shell_pid"]);
    for path in [
        &runtime.api,
        &runtime.base.join("runtime/herdr-client.sock"),
    ] {
        let record: Value =
            serde_json::from_slice(&fs::read(format!("{}.owner.json", path.display())).unwrap())
                .unwrap();
        assert_eq!(record["pid"], runtime.child.id());
    }
    runtime.stop();
}

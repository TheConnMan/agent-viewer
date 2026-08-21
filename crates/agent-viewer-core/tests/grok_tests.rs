use agent_viewer_core::{
    Backend, BackendKind, GrokBackend, GrokLifecycle, Session, SessionOrigin, Status, TailEvent,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok")
        .join(path)
}

fn session(id: &str, rollout_path: Option<PathBuf>) -> Session {
    Session {
        backend: BackendKind::Grok,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: id.to_string(),
        cwd: PathBuf::from("/home/user/project"),
        git_branch: None,
        status: Status::Unknown,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path,
        pr_refs: Vec::new(),
        daemon_hosted: true,
    }
}

#[test]
fn durable_listing_skips_malformed_and_partial_records_without_losing_siblings() {
    let lifecycle = GrokLifecycle::new("missing-grok", fixture("home"));
    let rows = lifecycle.list().expect("readable durable Grok sessions");

    let ids = rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>();
    assert!(
        ids.contains(&"session-durable"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        ids.contains(&"session-dead"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        ids.contains(&"session-dormant"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        !ids.contains(&"session-malformed"),
        "malformed JSON must be skipped"
    );
    assert!(
        !ids.contains(&"session-partial"),
        "partial summary must be skipped"
    );

    let durable = rows
        .iter()
        .find(|row| row.id == "session-durable")
        .expect("durable session");
    assert_eq!(durable.title, "Generated fixture title");
    assert_eq!(durable.summary, "Implemented durable discovery");
    assert_eq!(durable.cwd, Path::new("/home/user/project"));
    assert_eq!(durable.status, Status::Unknown);
    assert_eq!(durable.pid, None);
    assert!(
        !durable.daemon_hosted,
        "disk only rows do not claim a live leader"
    );
    assert_eq!(
        durable.rollout_path.as_deref(),
        Some(
            fixture(
                "home/sessions/project-fixture-0123456789abcdef/session-durable/chat_history.jsonl"
            )
            .as_path()
        )
    );
}

#[test]
fn durable_turn_end_does_not_fabricate_a_live_or_finished_status() {
    let lifecycle = GrokLifecycle::new("missing-grok", fixture("home"));
    let rows = lifecycle.list().expect("durable listing");
    let row = rows
        .iter()
        .find(|row| row.id == "session-durable")
        .expect("durable session");

    assert_eq!(row.status, Status::Unknown);
}

#[test]
fn official_chat_history_tail_ignores_a_torn_last_record() {
    let history = fixture(
        "home/sessions/project-fixture-0123456789abcdef/session-durable/chat_history.jsonl",
    );
    let backend = GrokBackend::new();
    let events = backend
        .tail(&session("session-durable", Some(history)), 3)
        .expect("Grok transcript tail");

    assert_eq!(
        events,
        vec![
            TailEvent::Tool {
                name: "read_file".to_string(),
                detail: "/home/user/project/src/main.rs".to_string(),
            },
            TailEvent::User("latest request with context".to_string()),
            TailEvent::Agent("latest answer".to_string()),
        ]
    );
}

#[test]
fn grok_advertises_only_implemented_official_actions_and_exact_attach_argv() {
    let backend = GrokBackend::new();
    let capabilities = backend.capabilities();
    assert!(capabilities.spawn);
    assert!(capabilities.attach);
    assert!(capabilities.rename);
    assert!(capabilities.delete);
    assert!(capabilities.stop);
    assert!(!capabilities.archive);

    let command = backend
        .attach_command(&session("session-selected", None))
        .expect("official Grok resume command");
    assert_eq!(command.get_program(), OsStr::new("grok"));
    assert_eq!(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec!["--resume".to_string(), "session-selected".to_string()]
    );
}

#[cfg(unix)]
mod protocol {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

    #[derive(Clone, Debug)]
    struct CapturedFrame {
        prefix: [u8; 4],
        body: Value,
    }

    struct ScriptedLeader {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        frames: Arc<Mutex<Vec<CapturedFrame>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl ScriptedLeader {
        fn start(home: &Path, oversized: bool) -> ScriptedLeader {
            fs::create_dir_all(home).expect("leader home");
            let socket = home.join("leader.sock");
            let _ = fs::remove_file(&socket);
            let lock = home.join("leader.lock");
            fs::write(&lock, std::process::id().to_string()).expect("leader lock");
            let listener = UnixListener::bind(&socket).expect("scripted leader socket");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let stop = Arc::new(AtomicBool::new(false));
            let frames = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_frames = Arc::clone(&frames);
            let thread_socket = socket.clone();
            let thread_lock = lock.clone();
            let roster: Value = serde_json::from_str(
                &fs::read_to_string(fixture("roster.json")).expect("roster fixture"),
            )
            .expect("roster JSON");
            let models: Value = serde_json::from_str(
                &fs::read_to_string(fixture("models.json")).expect("models fixture"),
            )
            .expect("models JSON");
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => handle_connection(
                            stream,
                            &thread_frames,
                            &roster,
                            &models,
                            oversized,
                            &thread_socket,
                            &thread_lock,
                        ),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            ScriptedLeader {
                socket,
                stop,
                frames,
                thread: Some(thread),
            }
        }

        fn captured(&self) -> Vec<CapturedFrame> {
            self.frames.lock().expect("captured frames").clone()
        }
    }

    impl Drop for ScriptedLeader {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.socket);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn read_frame(stream: &mut UnixStream) -> io::Result<Option<([u8; 4], Value)>> {
        let mut prefix = [0_u8; 4];
        match stream.read_exact(&mut prefix) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let len = u32::from_be_bytes(prefix) as usize;
        let mut body = vec![0; len];
        stream.read_exact(&mut body)?;
        let value = serde_json::from_slice(&body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Some((prefix, value)))
    }

    fn framed(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("frame JSON");
        let mut bytes = Vec::with_capacity(4 + body.len());
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn send_fragmented(stream: &mut UnixStream, value: &Value) -> io::Result<()> {
        let bytes = framed(value);
        stream.write_all(&bytes[..2])?;
        thread::sleep(Duration::from_millis(1));
        stream.write_all(&bytes[2..])
    }

    fn send_acp_response(stream: &mut UnixStream, id: Value, result: Value) -> io::Result<()> {
        let notification = json!({
            "type": "acp",
            "payload": json!({
                "jsonrpc": "2.0",
                "method": "_x.ai/sessions/changed",
                "params": {}
            }).to_string()
        });
        let response = json!({
            "type": "acp",
            "payload": json!({"jsonrpc":"2.0", "id":id, "result":result}).to_string()
        });
        let unrelated = json!({
            "type": "acp",
            "payload": json!({
                "jsonrpc":"2.0",
                "id":"unrelated-request",
                "result":{"ignored":true}
            }).to_string()
        });
        let mut bytes = framed(&notification);
        bytes.extend_from_slice(&framed(&unrelated));
        bytes.extend_from_slice(&framed(&response));
        stream.write_all(&bytes)
    }

    fn handle_connection(
        mut stream: UnixStream,
        captured: &Arc<Mutex<Vec<CapturedFrame>>>,
        roster: &Value,
        models: &Value,
        oversized: bool,
        socket: &Path,
        lock: &Path,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("leader read timeout");
        loop {
            let Ok(Some((prefix, outer))) = read_frame(&mut stream) else {
                return;
            };
            captured.lock().expect("capture frame").push(CapturedFrame {
                prefix,
                body: outer.clone(),
            });
            match outer.get("type").and_then(Value::as_str) {
                Some("register") => {
                    if oversized {
                        let _ = stream.write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes());
                        return;
                    }
                    let registered = json!({
                        "type":"registered",
                        "client_id":7,
                        "ready":true,
                        "leader_protocol_version":1,
                        "leader_binary_version":"0.1.150",
                        "leader_capabilities":{
                            "control_v1":true,
                            "runtime_cpu_profile":false,
                            "profile_formats":[],
                            "workspace_exposure":false,
                            "relaunch_v1":false
                        }
                    });
                    if send_fragmented(&mut stream, &registered).is_err() {
                        return;
                    }
                }
                Some("control") => {
                    let request_id = outer["request_id"].clone();
                    let response = json!({
                        "type":"control_result",
                        "request_id":request_id,
                        "result":{"Ok":{
                            "type":"leader_info",
                            "pid":std::process::id(),
                            "socket_path":socket.display().to_string(),
                            "lock_path":lock.display().to_string(),
                            "ws_url_suffix":"",
                            "leader_protocol_version":1,
                            "leader_binary_version":"0.1.150",
                            "profiling_supported":false,
                            "profiling_compiled_in":false,
                            "cpu_profile_active":false,
                            "cpu_profile_stopping":false,
                            "profile_started_at":null,
                            "profile_formats":[]
                        }}
                    });
                    if send_fragmented(&mut stream, &response).is_err() {
                        return;
                    }
                }
                Some("acp") => {
                    let Some(payload) = outer.get("payload").and_then(Value::as_str) else {
                        return;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(payload) else {
                        return;
                    };
                    let Some(id) = request.get("id").cloned() else {
                        continue;
                    };
                    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion":1,
                            "agentCapabilities":{},
                            "authMethods":[]
                        }),
                        "session/new" => json!({"sessionId":"session-alpha"}),
                        "session/prompt" => json!({"stopReason":"end_turn"}),
                        "_x.ai/sessions/list" => json!({"result":roster}),
                        "_x.ai/models/list" => json!({"result":models}),
                        "_x.ai/session/rename" | "_x.ai/session/delete" => {
                            json!({"result":{}})
                        }
                        _ => json!({}),
                    };
                    if send_acp_response(&mut stream, id, result).is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn leader_home() -> (tempfile::TempDir, ScriptedLeader) {
        let temp = tempfile::tempdir().expect("temporary Grok home");
        std::os::unix::fs::symlink(fixture("home/sessions"), temp.path().join("sessions"))
            .expect("durable session fixture link");
        let leader = ScriptedLeader::start(temp.path(), false);
        (temp, leader)
    }

    fn acp_requests(frames: &[CapturedFrame]) -> Vec<Value> {
        frames
            .iter()
            .filter(|frame| frame.body["type"] == "acp")
            .filter_map(|frame| frame.body["payload"].as_str())
            .filter_map(|payload| serde_json::from_str(payload).ok())
            .collect()
    }

    #[test]
    fn roster_precedence_maps_dead_to_error_and_dormant_to_unknown() {
        let (home, _leader) = leader_home();
        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");

        let durable = rows.iter().find(|row| row.id == "session-durable").unwrap();
        assert_eq!(durable.title, "Resident title");
        assert_eq!(durable.cwd, Path::new("/home/user/project"));
        assert_eq!(durable.status, Status::Working);
        assert_eq!(durable.pid, None);
        assert!(durable.daemon_hosted);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "session-dead")
                .unwrap()
                .status,
            Status::Error
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "session-dormant")
                .unwrap()
                .status,
            Status::Unknown
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "leader-only")
                .unwrap()
                .status,
            Status::NeedsInput { reason: None }
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.id == "session-durable")
                .count(),
            1,
            "resident roster row must replace its durable sibling"
        );
    }

    #[test]
    fn spawn_registers_before_acp_and_preserves_identity_model_and_prompt_target() {
        let (home, leader) = leader_home();
        let result = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "implement framing",
                Some("grok-4-fast"),
            )
            .expect("official Grok spawn");
        assert_eq!(result.session_id.as_deref(), Some("session-alpha"));
        assert_eq!(result.pid, None);

        let frames = leader.captured();
        assert_eq!(frames.first().unwrap().body["type"], "register");
        assert_eq!(frames.first().unwrap().body["client_type"], "agent-viewer");
        assert_eq!(frames.first().unwrap().body["mode"], "stdio");
        assert_eq!(
            frames.first().unwrap().body["capabilities"]["default_model"],
            "grok-4-fast"
        );
        for frame in &frames {
            let encoded = serde_json::to_vec(&frame.body).unwrap();
            assert_eq!(u32::from_be_bytes(frame.prefix) as usize, encoded.len());
        }
        let requests = acp_requests(&frames);
        assert_eq!(requests.first().unwrap()["method"], "initialize");
        let new = requests
            .iter()
            .find(|request| request["method"] == "session/new")
            .unwrap();
        assert_eq!(new["params"]["cwd"], "/home/user/project");
        let prompt = requests
            .iter()
            .find(|request| request["method"] == "session/prompt")
            .unwrap();
        assert_eq!(prompt["params"]["sessionId"], "session-alpha");
        assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "implement framing");
    }

    #[test]
    fn cancel_is_scoped_to_the_selected_session() {
        let (home, leader) = leader_home();
        GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect("session cancel");

        let requests = acp_requests(&leader.captured());
        let cancels = requests
            .iter()
            .filter(|request| request["method"] == "session/cancel")
            .collect::<Vec<_>>();
        assert_eq!(cancels.len(), 1);
        assert_eq!(cancels[0]["params"]["sessionId"], "session-selected");
        let wire = serde_json::to_string(&cancels).unwrap();
        assert!(!wire.contains("session-sibling"));
    }

    #[test]
    fn spawn_without_a_model_does_not_invent_registration_metadata() {
        let (home, leader) = leader_home();
        let result = GrokLifecycle::new("/bin/true", home.path())
            .spawn(Path::new("/home/user/project"), "use runtime default", None)
            .expect("default model Grok spawn");
        assert_eq!(result.session_id.as_deref(), Some("session-alpha"));

        let registration = leader
            .captured()
            .into_iter()
            .find(|frame| frame.body["type"] == "register")
            .expect("leader registration");
        assert!(registration.body["capabilities"]["default_model"].is_null());
    }

    #[test]
    fn rename_delete_and_models_use_the_official_extension_envelopes() {
        let (home, leader) = leader_home();
        let lifecycle = GrokLifecycle::new("/bin/true", home.path());
        lifecycle
            .rename("session-selected", "A precise title")
            .expect("official rename");
        lifecycle
            .delete("session-selected")
            .expect("official delete");
        assert_eq!(
            lifecycle.models().expect("official model discovery"),
            vec![
                "default".to_string(),
                "grok-4".to_string(),
                "grok-4-fast".to_string(),
            ]
        );

        let requests = acp_requests(&leader.captured());
        for (wire_method, logical_method) in [
            ("_x.ai/session/rename", "x.ai/session/rename"),
            ("_x.ai/session/delete", "x.ai/session/delete"),
            ("_x.ai/models/list", "x.ai/models/list"),
        ] {
            let request = requests
                .iter()
                .find(|request| request["method"] == wire_method)
                .unwrap_or_else(|| panic!("missing {wire_method}"));
            assert_eq!(request["params"]["method"], logical_method);
        }
        let rename = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/rename")
            .unwrap();
        assert_eq!(rename["params"]["params"]["sessionId"], "session-selected");
        assert_eq!(rename["params"]["params"]["title"], "A precise title");
        let delete = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/delete")
            .unwrap();
        assert_eq!(delete["params"]["params"]["sessionId"], "session-selected");
    }

    #[test]
    fn oversized_leader_frame_is_rejected_at_the_official_cap() {
        let temp = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start(temp.path(), true);
        let error = GrokLifecycle::new("/bin/true", temp.path())
            .diagnostics()
            .expect_err("a frame over 64 MiB must be rejected");
        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains("64") || message.contains("frame") || message.contains("large"),
            "cap rejection must be actionable, got {message:?}"
        );
    }

    #[test]
    fn model_discovery_failure_degrades_to_only_the_backend_default() {
        let home = tempfile::tempdir().expect("empty Grok home");
        assert_eq!(
            GrokLifecycle::new("/bin/true", home.path())
                .models()
                .expect("best effort model fallback"),
            vec!["default".to_string()]
        );
    }
}

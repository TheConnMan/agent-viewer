mod common;

use agent_viewer_core::Status;
use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin};
use agent_viewer_core::opencode::{
    OpencodeBackend, OpencodeRuntime, is_run_mode_permission, opencode_status,
    parse_opencode_models, read_opencode_last_message,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Clone)]
struct ScriptedResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    delay: Duration,
}

impl ScriptedResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    fn raw(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn empty(status: u16) -> Self {
        Self::raw(status, "application/json", "")
    }
}

struct ScriptedServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedServer {
    fn spawn(responses: Vec<ScriptedResponse>) -> Self {
        Self::spawn_on("127.0.0.1:0".parse().unwrap(), responses).expect("bind scripted server")
    }

    fn spawn_on(addr: SocketAddr, responses: Vec<ScriptedResponse>) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let mut responses: VecDeque<_> = responses.into();
        let thread = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        if let Ok(request) = request {
                            requests_for_thread.lock().unwrap().push(request);
                        }
                        let response = responses.pop_front().unwrap_or_else(|| {
                            ScriptedResponse::raw(500, "text/plain", "unexpected request")
                        });
                        if !response.delay.is_zero() {
                            std::thread::sleep(response.delay);
                        }
                        let _ = write_http_response(&mut stream, &response);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        })
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_requests(&self, count: usize) -> Vec<RecordedRequest> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.requests()
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<RecordedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let target = request_parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(RecordedRequest {
        method,
        target,
        headers,
        body: bytes[header_end..bytes.len().min(header_end + content_length)].to_vec(),
    })
}

fn write_http_response(stream: &mut TcpStream, response: &ScriptedResponse) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Response",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
        response.body
    )?;
    stream.flush()
}

fn healthy_response() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"healthy": true, "version": "1.17.20"}))
}

fn empty_server_list_responses() -> Vec<ScriptedResponse> {
    vec![healthy_response(), ScriptedResponse::json(200, json!([]))]
}

fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve free address");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

fn unused_addrs() -> [SocketAddr; 2] {
    let primary = TcpListener::bind("127.0.0.1:0").expect("reserve primary address");
    let backup = TcpListener::bind("127.0.0.1:0").expect("reserve backup address");
    let addresses = [primary.local_addr().unwrap(), backup.local_addr().unwrap()];
    drop(primary);
    drop(backup);
    addresses
}

fn runtime_for(
    candidates: [SocketAddr; 2],
    timeout: Duration,
    durable_cwd: PathBuf,
    launcher: impl Fn(Command) -> io::Result<()> + Send + Sync + 'static,
) -> OpencodeRuntime {
    OpencodeRuntime::for_test(candidates, timeout, durable_cwd, launcher)
}

fn no_launch_runtime(
    candidates: [SocketAddr; 2],
    timeout: Duration,
    launcher_calls: Arc<AtomicUsize>,
) -> OpencodeRuntime {
    runtime_for(candidates, timeout, PathBuf::from("/"), move |_| {
        launcher_calls.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("launcher was not expected"))
    })
}

fn backend_with_runtime(runtime: OpencodeRuntime) -> OpencodeBackend {
    OpencodeBackend::with_runtime(runtime)
}

fn backend_with_db_and_runtime(db_path: PathBuf, runtime: OpencodeRuntime) -> OpencodeBackend {
    OpencodeBackend::with_db_and_runtime(db_path, runtime)
}

fn command_args(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn encoded_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[test]
fn opencode_runtime_reuses_healthy_primary_before_backup() {
    let primary = ScriptedServer::spawn(empty_server_list_responses());
    let backup = ScriptedServer::spawn(empty_server_list_responses());
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [primary.addr, backup.addr],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_runtime(runtime);

    assert!(
        backend
            .list()
            .expect("list from healthy primary")
            .is_empty()
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        primary
            .wait_for_requests(2)
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        vec!["/global/health", "/session?limit=10000"]
    );
    assert!(
        backup.requests().is_empty(),
        "a healthy primary must stop endpoint selection"
    );
}

#[test]
fn opencode_runtime_reuses_healthy_backup_before_starting_free_primary() {
    let primary = unused_addr();
    let backup = ScriptedServer::spawn(empty_server_list_responses());
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [primary, backup.addr],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_runtime(runtime);

    assert!(backend.list().expect("list from healthy backup").is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backup.wait_for_requests(2)[0].target, "/global/health");
}

#[test]
fn opencode_runtime_skips_unrelated_primary_and_reuses_healthy_backup() {
    let primary = ScriptedServer::spawn(vec![ScriptedResponse::raw(
        200,
        "text/html",
        "<html>not opencode</html>",
    )]);
    let backup = ScriptedServer::spawn(empty_server_list_responses());
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [primary.addr, backup.addr],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_runtime(runtime);

    assert!(backend.list().expect("list from backup").is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(primary.wait_for_requests(1)[0].target, "/global/health");
    assert_eq!(backup.wait_for_requests(2)[0].target, "/global/health");
}

#[test]
fn opencode_spawn_starts_primary_with_exact_loopback_command_and_durable_cwd() {
    let [primary, backup] = unused_addrs();
    let durable_cwd = tempfile::TempDir::new().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let launched_server = Arc::new(Mutex::new(None));
    let captured_for_launcher = Arc::clone(&captured);
    let server_for_launcher = Arc::clone(&launched_server);
    let runtime = runtime_for(
        [primary, backup],
        Duration::from_secs(1),
        durable_cwd.path().to_path_buf(),
        move |command| {
            let args = command_args(&command);
            let port = args
                .windows(2)
                .find(|pair| pair[0] == "--port")
                .and_then(|pair| pair[1].parse::<u16>().ok())
                .expect("launch carries a port");
            captured_for_launcher.lock().unwrap().push((
                command.get_program().to_string_lossy().into_owned(),
                args,
                command.get_current_dir().map(PathBuf::from),
            ));
            let server = ScriptedServer::spawn_on(
                SocketAddr::from(([127, 0, 0, 1], port)),
                vec![
                    healthy_response(),
                    ScriptedResponse::json(200, json!({"id": "ses_started_primary"})),
                    ScriptedResponse::empty(204),
                ],
            )?;
            *server_for_launcher.lock().unwrap() = Some(server);
            Ok(())
        },
    );
    let backend = backend_with_runtime(runtime);

    let spawned = backend
        .spawn(PathBuf::from("/tmp/project").as_path(), "hello", None)
        .expect("spawn through started primary");
    assert_eq!(spawned.pid, None);
    assert_eq!(spawned.session_id.as_deref(), Some("ses_started_primary"));

    let launches = captured.lock().unwrap();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, "opencode");
    assert_eq!(
        launches[0].1,
        vec![
            "serve".to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            primary.port().to_string(),
        ]
    );
    assert_eq!(launches[0].2.as_deref(), Some(durable_cwd.path()));
    assert!(!launches[0].1.iter().any(|arg| arg == "--mdns"));
}

#[test]
fn opencode_spawn_starts_backup_when_primary_is_an_unrelated_listener() {
    let primary = ScriptedServer::spawn(vec![ScriptedResponse::raw(
        200,
        "text/plain",
        "another service",
    )]);
    let backup = unused_addr();
    let launched_ports = Arc::new(Mutex::new(Vec::new()));
    let launched_server = Arc::new(Mutex::new(None));
    let ports_for_launcher = Arc::clone(&launched_ports);
    let server_for_launcher = Arc::clone(&launched_server);
    let runtime = runtime_for(
        [primary.addr, backup],
        Duration::from_secs(1),
        PathBuf::from("/"),
        move |command| {
            let args = command_args(&command);
            let port = args
                .windows(2)
                .find(|pair| pair[0] == "--port")
                .and_then(|pair| pair[1].parse::<u16>().ok())
                .unwrap();
            ports_for_launcher.lock().unwrap().push(port);
            *server_for_launcher.lock().unwrap() = Some(ScriptedServer::spawn_on(
                SocketAddr::from(([127, 0, 0, 1], port)),
                vec![
                    healthy_response(),
                    ScriptedResponse::json(200, json!({"id": "ses_backup"})),
                    ScriptedResponse::empty(204),
                ],
            )?);
            Ok(())
        },
    );
    let backend = backend_with_runtime(runtime);

    let spawned = backend
        .spawn(PathBuf::from("/tmp").as_path(), "hello", None)
        .expect("spawn through backup");
    assert_eq!(spawned.session_id.as_deref(), Some("ses_backup"));
    assert_eq!(*launched_ports.lock().unwrap(), vec![backup.port()]);
}

#[test]
fn opencode_spawn_reports_both_occupied_candidates_without_launching() {
    let primary = ScriptedServer::spawn(vec![ScriptedResponse::raw(
        200,
        "text/plain",
        "primary collision",
    )]);
    let backup = ScriptedServer::spawn(vec![ScriptedResponse::raw(
        200,
        "text/plain",
        "backup collision",
    )]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [primary.addr, backup.addr],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);

    let error = backend
        .spawn(PathBuf::from("/tmp").as_path(), "hello", None)
        .expect_err("both collisions must fail");
    let message = error.to_string();
    assert!(
        message.contains(&primary.addr.port().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&backup.addr.port().to_string()),
        "{message}"
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_spawn_readiness_deadline_is_one_bounded_budget() {
    let [primary, backup] = unused_addrs();
    let launches = Arc::new(AtomicUsize::new(0));
    let launches_for_callback = Arc::clone(&launches);
    let runtime = runtime_for(
        [primary, backup],
        Duration::from_millis(120),
        PathBuf::from("/"),
        move |_| {
            launches_for_callback.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let backend = backend_with_runtime(runtime);

    let start = Instant::now();
    let error = backend
        .spawn(PathBuf::from("/tmp").as_path(), "hello", None)
        .expect_err("a launcher that never becomes healthy must fail");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "readiness exceeded its overall deadline: {elapsed:?}"
    );
    assert!(
        error.to_string().contains(&primary.port().to_string()),
        "{}",
        error
    );
    assert!(launches.load(Ordering::SeqCst) >= 1);
}

#[test]
fn opencode_health_requires_true_and_a_string_version() {
    for body in [
        ScriptedResponse::raw(200, "text/html", "<html>spa</html>"),
        ScriptedResponse::raw(200, "application/json", "{broken"),
        ScriptedResponse::json(200, json!({"healthy": false, "version": "1.17.20"})),
        ScriptedResponse::json(200, json!({"healthy": true, "version": 11720})),
        ScriptedResponse::raw(401, "application/json", "{\"error\":\"auth required\"}"),
    ] {
        let primary = ScriptedServer::spawn(vec![body]);
        let backup = unused_addr();
        let launcher_calls = Arc::new(AtomicUsize::new(0));
        let runtime = no_launch_runtime(
            [primary.addr, backup],
            Duration::from_millis(100),
            Arc::clone(&launcher_calls),
        );
        let db_dir = tempfile::TempDir::new().unwrap();
        let mut backend = backend_with_db_and_runtime(db_dir.path().join("missing.db"), runtime);
        assert!(
            backend
                .list()
                .expect("invalid health degrades quietly")
                .is_empty(),
            "invalid health must not become the server tier"
        );
        assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    }
}

// --- Preserved v1 listing shape (order / labels / hidden) ---

#[test]
fn opencode_lists_rows_hidden_and_order() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_parent',NULL,'/home/user/oc-proj','Parent',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_child','ses_parent','/home/user/oc-proj','Child',1100,2000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_arch',NULL,'/home/user/oc-proj','Archived',900,1000,5000)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    assert_eq!(sessions.len(), 3);

    // time_updated DESC: parent (3000), child (2000), arch (1000).
    assert_eq!(sessions[0].id, "ses_parent");
    assert_eq!(sessions[1].id, "ses_child");
    assert_eq!(sessions[2].id, "ses_arch");

    let parent = &sessions[0];
    assert_eq!(parent.backend, BackendKind::Opencode);
    assert_eq!(parent.cwd, PathBuf::from("/home/user/oc-proj"));
    assert_eq!(parent.title, "Parent");
    assert_eq!(parent.created_at_ms, 1000);
    assert_eq!(parent.updated_at_ms, 3000);
    assert_eq!(parent.origin, SessionOrigin::Interactive);
    assert!(!parent.hidden);
    assert_eq!(parent.short_id, None); // opencode sessions carry no claude short id

    assert!(sessions[1].companion); // parent_id non-NULL
    assert!(!sessions[1].hidden);
    assert!(sessions[2].hidden); // time_archived IS NOT NULL
}

#[test]
fn opencode_missing_db_lists_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut backend = OpencodeBackend::with_db(dir.path().join("nope.db"));
    let sessions = backend.list().expect("missing DB must be Ok(empty)");
    assert!(sessions.is_empty());
}

#[test]
fn opencode_server_list_maps_status_input_companions_archive_and_hosting() {
    let mut sessions = common::fixture_json("opencode/session_list_with_archived_row.json");
    let rows = sessions.as_array_mut().unwrap();
    rows[0]["permission"] = Value::Null;
    let parent_id = rows[0]["id"].clone();
    rows[1]["parentID"] = parent_id;
    rows[1]["permission"] = Value::Null;
    rows[3]["permission"] = Value::Null;
    rows[4]["permission"] = Value::Null;
    rows[5]["permission"] = Value::Null;
    for row in &mut rows[1..5] {
        row["metadata"] = json!({"agent-viewer": {"background": true}});
    }
    let directory = rows[1]["directory"].as_str().unwrap().to_string();
    let permission_id = rows[3]["id"].as_str().unwrap().to_string();
    let question_id = rows[4]["id"].as_str().unwrap().to_string();
    let external_id = rows[5]["id"].as_str().unwrap().to_string();
    let mut status = common::fixture_json("opencode/session_status_idle_busy_retry.json");
    status
        .as_object_mut()
        .unwrap()
        .insert(external_id.clone(), json!({"type": "busy"}));
    let primary = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, sessions),
        ScriptedResponse::json(200, status),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "per_1",
                "sessionID": permission_id,
                "permission": "bash",
                "patterns": ["git status", "git diff"],
                "metadata": {"ignored": "not a reason"}
            }]),
        ),
        ScriptedResponse::json(
            200,
            json!([{
                "id": "que_1",
                "sessionID": question_id,
                "questions": [
                    {"question": "Which deployment target?", "header": "Target"},
                    {"question": "Ignored second question?"}
                ]
            }]),
        ),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [primary.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_runtime(runtime);

    let listed = backend.list().expect("server list");
    assert_eq!(listed.len(), 8);
    assert_eq!(
        listed
            .iter()
            .map(|session| session.updated_at_ms)
            .collect::<Vec<_>>(),
        {
            let mut values = listed
                .iter()
                .map(|session| session.updated_at_ms)
                .collect::<Vec<_>>();
            values.sort_by(|left, right| right.cmp(left));
            values
        },
        "server rows remain newest first"
    );
    assert_eq!(listed[0].status, Status::Idle);
    assert!(listed[0].hidden);
    assert_eq!(listed[1].status, Status::Working);
    assert!(listed[1].companion, "parentID marks a companion");
    assert_eq!(listed[2].status, Status::Working, "retry is active work");
    assert!(
        listed[2].companion,
        "the captured denied question permission marks a run row"
    );

    let permission = listed
        .iter()
        .find(|session| session.id == permission_id)
        .unwrap();
    let Status::NeedsInput {
        reason: Some(reason),
    } = &permission.status
    else {
        panic!("pending permission did not become NeedsInput");
    };
    assert!(reason.contains("bash"), "{reason}");
    assert!(reason.contains("git status"), "{reason}");

    let question = listed
        .iter()
        .find(|session| session.id == question_id)
        .unwrap();
    assert_eq!(
        question.status,
        Status::NeedsInput {
            reason: Some("Which deployment target?".to_string())
        }
    );
    assert_eq!(
        listed
            .iter()
            .find(|session| session.id == external_id)
            .unwrap()
            .status,
        Status::Idle,
        "unmarked external history must ignore scoped busy status"
    );
    assert!(
        listed
            .iter()
            .all(|session| session.daemon_hosted && session.pid.is_none()),
        "every server row is shared runtime hosted and never signalable"
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        primary
            .wait_for_requests(5)
            .iter()
            .map(|request| request.target.clone())
            .collect::<Vec<_>>(),
        vec![
            "/global/health".to_string(),
            "/session?limit=10000".to_string(),
            format!(
                "/session/status?directory={}",
                encoded_component(&directory)
            ),
            format!("/permission?directory={}", encoded_component(&directory)),
            format!("/question?directory={}", encoded_component(&directory)),
        ],
        "listing scopes one metadata triplet to the marked directory"
    );
}

#[test]
fn opencode_server_metadata_requests_are_once_per_unique_marked_directory() {
    let first_directory = "/tmp/project one";
    let second_directory = "/tmp/雪?project/two";
    let sessions = json!([
        {
            "id": "ses_first_busy",
            "parentID": null,
            "directory": first_directory,
            "title": "first busy",
            "permission": null,
            "metadata": {"agent-viewer": {"background": true}},
            "time": {"created": 10, "updated": 50}
        },
        {
            "id": "ses_first_idle",
            "parentID": null,
            "directory": first_directory,
            "title": "first idle",
            "permission": null,
            "metadata": {"agent-viewer": {"background": true}},
            "time": {"created": 10, "updated": 40}
        },
        {
            "id": "ses_second_retry",
            "parentID": null,
            "directory": second_directory,
            "title": "second retry",
            "permission": null,
            "metadata": {"agent-viewer": {"background": true}},
            "time": {"created": 10, "updated": 30}
        },
        {
            "id": "ses_external_history",
            "parentID": null,
            "directory": "/historical/do not probe",
            "title": "external",
            "permission": null,
            "time": {"created": 10, "updated": 20}
        },
        {
            "id": "ses_archived_marked",
            "parentID": null,
            "directory": "/archived/do not probe",
            "title": "archived",
            "permission": null,
            "metadata": {"agent-viewer": {"background": true}},
            "time": {"created": 10, "updated": 10, "archived": 11}
        }
    ]);
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, sessions),
        ScriptedResponse::json(
            200,
            json!({
                "ses_first_busy": {"type": "busy"},
                "ses_first_idle": {"type": "idle"}
            }),
        ),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(
            200,
            json!({
                "ses_second_retry": {
                    "type": "retry",
                    "attempt": 2,
                    "message": "temporary provider failure",
                    "next": 100
                }
            }),
        ),
        ScriptedResponse::json(200, json!([])),
        ScriptedResponse::json(200, json!([])),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_runtime(runtime);

    let listed = backend.list().expect("directory scoped server list");
    let status = |id: &str| {
        listed
            .iter()
            .find(|session| session.id == id)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status("ses_first_busy"), Status::Working);
    assert_eq!(status("ses_first_idle"), Status::Idle);
    assert_eq!(status("ses_second_retry"), Status::Working);
    assert_eq!(status("ses_external_history"), Status::Idle);

    let first = encoded_component(first_directory);
    let second = encoded_component(second_directory);
    assert_eq!(
        server
            .wait_for_requests(8)
            .iter()
            .map(|request| request.target.clone())
            .collect::<Vec<_>>(),
        vec![
            "/global/health".to_string(),
            "/session?limit=10000".to_string(),
            format!("/session/status?directory={first}"),
            format!("/permission?directory={first}"),
            format!("/question?directory={first}"),
            format!("/session/status?directory={second}"),
            format!("/permission?directory={second}"),
            format!("/question?directory={second}"),
        ],
        "two marked rows in one directory share one metadata triplet"
    );
    assert_eq!(
        server.requests().len(),
        8,
        "metadata calls must scale with unique marked directories, not rows"
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_healthy_server_api_failure_is_not_masked_by_sqlite() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_sqlite',NULL,'/tmp','must not leak through',1000,3000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::raw(500, "text/plain", "session route failed"),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_db_and_runtime(path, runtime);

    let error = backend
        .list()
        .expect_err("healthy server route failure must remain visible");
    let message = error.to_string();
    assert!(message.contains("/session"), "{message}");
    assert!(message.contains("500"), "{message}");
    assert!(!message.contains("must not leak through"), "{message}");
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_unavailable_runtime_uses_read_only_sqlite_without_launching() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_external',NULL,'/tmp','External compatibility row',1000,3000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let candidates = unused_addrs();
    let runtime = no_launch_runtime(
        candidates,
        Duration::from_millis(80),
        Arc::clone(&launcher_calls),
    );
    let mut backend = backend_with_db_and_runtime(path, runtime);

    let listed = backend.list().expect("SQLite compatibility listing");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "ses_external");
    assert!(!listed[0].daemon_hosted);
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_capabilities_follow_the_last_probed_tier_without_network_on_read() {
    let server = ScriptedServer::spawn(empty_server_list_responses());
    let healthy_launcher_calls = Arc::new(AtomicUsize::new(0));
    let healthy_runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&healthy_launcher_calls),
    );
    let mut healthy = backend_with_runtime(healthy_runtime);
    healthy.list().expect("establish healthy tier");
    let request_count = server.requests().len();
    let healthy_caps = healthy.capabilities();
    assert_eq!(
        healthy_caps,
        Capabilities {
            spawn: true,
            attach: true,
            rename: true,
            archive: true,
            delete: true,
            stop: true,
            needs_input: true,
            pr_refs: false,
            live_status: true,
        }
    );
    let mut server_row = session_with_pid(None);
    server_row.daemon_hosted = true;
    assert_eq!(healthy.capabilities_for(&server_row), healthy_caps);
    assert_eq!(
        server.requests().len(),
        request_count,
        "capability reads must not perform HTTP"
    );

    let fallback_launcher_calls = Arc::new(AtomicUsize::new(0));
    let candidates = unused_addrs();
    let fallback_runtime = no_launch_runtime(
        candidates,
        Duration::from_millis(80),
        Arc::clone(&fallback_launcher_calls),
    );
    let mut fallback = backend_with_runtime(fallback_runtime);
    fallback.list().expect("establish compatibility tier");
    assert_eq!(
        fallback.capabilities(),
        Capabilities {
            spawn: true,
            attach: true,
            rename: false,
            archive: false,
            delete: true,
            stop: false,
            needs_input: false,
            pr_refs: false,
            live_status: false,
        }
    );
    assert_eq!(healthy_launcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fallback_launcher_calls.load(Ordering::SeqCst), 0);
}

// --- v2: three-tier heuristic (test 17) ---

#[test]
fn opencode_status_three_tiers() {
    let now = 1_000_000_000_000_i64;
    // live + fresh -> Working (age <= 60_000, boundary inclusive).
    assert_eq!(opencode_status(true, now - 59_000, now), Status::Working);
    assert_eq!(opencode_status(true, now - 60_000, now), Status::Working);
    // live + <= 30 min -> Idle (boundary inclusive at 1_800_000).
    assert_eq!(opencode_status(true, now - 61_000, now), Status::Idle);
    assert_eq!(opencode_status(true, now - 1_800_000, now), Status::Idle);
    // live but older than 30 min -> Done.
    assert_eq!(opencode_status(true, now - 1_800_001, now), Status::Done);
    // no live process -> Done regardless of recency.
    assert_eq!(opencode_status(false, now - 5_000, now), Status::Done);
}

// --- v2: companion flag from parent_id (test 18) ---

#[test]
fn opencode_lists_companion_flag() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_root',NULL,'/home/user/oc-proj','Root',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_kid','ses_root','/home/user/oc-proj','Kid',1100,2000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");

    let root = sessions.iter().find(|s| s.id == "ses_root").unwrap();
    let kid = sessions.iter().find(|s| s.id == "ses_kid").unwrap();
    assert!(!root.companion); // parent_id IS NULL
    assert!(kid.companion); // parent_id set -> companion
}

// --- v2: last-message reader (peek) ---

#[test]
fn opencode_last_message_returns_newest_text_concatenated() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with a single text part.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"older reply\"}')",
        // Newer assistant message: a tool part THEN a text part (tool must be skipped).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_tool','msg_new','ses_1',2001,2001,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_text','msg_new','ses_1',2002,2002,'{\"type\":\"text\",\"text\":\"newer reply text\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.role, "assistant");
    // The NEWER message's text only, tool part skipped.
    assert_eq!(item.text, "newer reply text");
}

#[test]
fn opencode_last_message_skips_whitespace_only_newest() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with real text.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"real prior message\"}')",
        // Newest message whose only text part is whitespace (newline-only around a tool
        // transition) -> it must be skipped so the real prior message surfaces.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new','msg_new','ses_1',2000,2000,'{\"type\":\"text\",\"text\":\"\\n\\n\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.text, "real prior message");
}

#[test]
fn opencode_last_message_none_when_no_text_message() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // A message whose only part is a tool part -> no text -> Ok(None).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_1','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_1','msg_1','ses_1',1000,1000,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    // Also nothing for an unknown session id.
    assert!(
        read_opencode_last_message(&path, "ses_1")
            .expect("read ok")
            .is_none()
    );
    assert!(
        read_opencode_last_message(&path, "nope")
            .expect("read ok")
            .is_none()
    );
}

#[test]
fn opencode_last_message_missing_db_is_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("nope.db");
    assert!(
        read_opencode_last_message(&missing, "ses_1")
            .expect("read ok")
            .is_none()
    );
}

// --- v2: `opencode models` stdout parse ---

#[test]
fn parse_opencode_models_trims_and_drops_blanks() {
    let stdout = "\
anthropic/claude-opus-4-8
  openai/gpt-5.6

github-copilot/gpt-5

";
    let got = parse_opencode_models(stdout);
    assert_eq!(
        got,
        vec![
            "anthropic/claude-opus-4-8",
            "openai/gpt-5.6",
            "github-copilot/gpt-5",
        ]
    );
}

// --- per-row stop capability ---

fn session_with_pid(pid: Option<u32>) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: "ses_cap".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "probe".to_string(),
        cwd: PathBuf::from("/tmp"),
        git_branch: None,
        status: Status::Idle,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn server_session(id: &str, cwd: PathBuf) -> Session {
    let mut session = session_with_pid(None);
    session.id = id.to_string();
    session.cwd = cwd;
    session.daemon_hosted = true;
    session
}

#[test]
fn opencode_spawn_create_then_prompt_returns_exact_id_and_model_shapes() {
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_exact_identity"})),
        ScriptedResponse::empty(204),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);
    let cwd = PathBuf::from("/tmp/space and 雪/?x=1&next=../");
    let task = "A title with Unicode 雪 and enough text to exceed the forty character title limit";

    let spawned = backend
        .spawn(&cwd, task, Some("openai/gpt-5.6-sol"))
        .expect("server spawn");
    assert_eq!(spawned.pid, None);
    assert_eq!(spawned.session_id.as_deref(), Some("ses_exact_identity"));
    let requests = server.wait_for_requests(3);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        vec!["GET", "POST", "POST"]
    );
    let directory = encoded_component(cwd.to_str().unwrap());
    assert_eq!(
        requests[1].target,
        format!("/session?directory={directory}")
    );
    assert_eq!(
        requests[2].target,
        format!("/session/ses_exact_identity/prompt_async?directory={directory}")
    );
    assert_eq!(
        requests[1].json_body(),
        json!({
            "title": task.chars().take(40).collect::<String>(),
            "metadata": {"agent-viewer": {"background": true}},
            "model": {"providerID": "openai", "id": "gpt-5.6-sol"}
        })
    );
    assert_eq!(
        requests[2].json_body(),
        json!({
            "parts": [{"type": "text", "text": task}],
            "model": {"providerID": "openai", "modelID": "gpt-5.6-sol"}
        })
    );
    assert!(
        requests[1]
            .headers
            .get("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_spawn_omits_default_model_and_rejects_malformed_model_before_mutation() {
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_default"})),
        ScriptedResponse::empty(204),
        healthy_response(),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);

    backend
        .spawn(PathBuf::from("/tmp").as_path(), "default prompt", None)
        .expect("default spawn");
    let requests = server.wait_for_requests(3);
    assert!(requests[1].json_body().get("model").is_none());
    assert!(requests[2].json_body().get("model").is_none());

    let error = backend
        .spawn(
            PathBuf::from("/tmp").as_path(),
            "must not submit",
            Some("missing-provider-separator"),
        )
        .expect_err("malformed model must fail");
    assert!(error.to_string().contains("provider"), "{error}");
    let after = server.wait_for_requests(4);
    assert_eq!(
        after
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2,
        "malformed model must not create or prompt a session"
    );
}

#[test]
fn opencode_prompt_failure_reports_created_id_and_never_submits_twice() {
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": "ses_orphan_visible"})),
        ScriptedResponse::raw(200, "text/plain", "provider rejected prompt"),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);

    let error = backend
        .spawn(PathBuf::from("/tmp").as_path(), "one submission", None)
        .expect_err("prompt failure must be visible");
    let message = error.to_string();
    assert!(message.contains("ses_orphan_visible"), "{message}");
    assert!(message.contains("provider rejected prompt"), "{message}");
    let requests = server.wait_for_requests(3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.contains("prompt_async"))
            .count(),
        1
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_shared_runtime_connects_spawn_and_fresh_listing_instances() {
    let [primary, backup] = unused_addrs();
    let launched_server = Arc::new(Mutex::new(None));
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let server_for_launcher = Arc::clone(&launched_server);
    let calls_for_launcher = Arc::clone(&launcher_calls);
    let runtime = runtime_for(
        [primary, backup],
        Duration::from_secs(1),
        PathBuf::from("/"),
        move |command| {
            calls_for_launcher.fetch_add(1, Ordering::SeqCst);
            let args = command_args(&command);
            let port = args
                .windows(2)
                .find(|pair| pair[0] == "--port")
                .and_then(|pair| pair[1].parse::<u16>().ok())
                .unwrap();
            *server_for_launcher.lock().unwrap() = Some(ScriptedServer::spawn_on(
                SocketAddr::from(([127, 0, 0, 1], port)),
                vec![
                    healthy_response(),
                    ScriptedResponse::json(200, json!({"id": "ses_shared"})),
                    ScriptedResponse::empty(204),
                    healthy_response(),
                    ScriptedResponse::json(
                        200,
                        json!([{
                            "id": "ses_shared",
                            "parentID": null,
                            "directory": "/tmp",
                            "title": "shared",
                            "permission": null,
                            "metadata": {"agent-viewer": {"background": true}},
                            "time": {"created": 1, "updated": 2}
                        }]),
                    ),
                    ScriptedResponse::json(200, json!({"ses_shared": {"type": "busy"}})),
                    ScriptedResponse::json(200, json!([])),
                    ScriptedResponse::json(200, json!([])),
                ],
            )?);
            Ok(())
        },
    );

    let first = backend_with_runtime(runtime.clone());
    let result = first
        .spawn(PathBuf::from("/tmp").as_path(), "shared runtime", None)
        .expect("spawn");
    drop(first);
    let mut second = backend_with_runtime(runtime);
    let row = second
        .list()
        .expect("fresh backend lists shared server")
        .into_iter()
        .find(|session| session.id == result.session_id.clone().unwrap())
        .expect("exact spawned row");
    assert_eq!(row.status, Status::Working);
    assert!(row.daemon_hosted);
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn opencode_server_attach_uses_exact_url_id_and_never_forks() {
    let server = ScriptedServer::spawn(vec![healthy_response(), healthy_response()]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);
    let existing_dir = tempfile::TempDir::new().unwrap();
    let session = server_session("ses_attach_exact", existing_dir.path().to_path_buf());

    let command = backend.attach_command(&session).expect("server attach");
    assert_eq!(command.get_program(), "opencode");
    assert_eq!(
        command_args(&command),
        vec![
            "attach".to_string(),
            format!("http://{}", server.addr),
            "-s".to_string(),
            "ses_attach_exact".to_string(),
        ]
    );
    assert_eq!(command.get_current_dir(), Some(existing_dir.path()));
    assert!(!command_args(&command).iter().any(|arg| arg == "--fork"));

    let missing = server_session(
        "ses_missing_cwd",
        existing_dir.path().join("deleted-directory"),
    );
    let missing_command = backend
        .attach_command(&missing)
        .expect("deleted cwd does not block attach");
    assert_eq!(missing_command.get_current_dir(), None);
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_stop_aborts_only_the_percent_encoded_session() {
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!(false)),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);
    let id = "ses/雪?../../target#fragment";
    let mut session = server_session(id, PathBuf::from("/tmp"));
    session.pid = Some(u32::MAX);

    backend
        .stop(&session)
        .expect("idle abort false is still an accepted response");
    let requests = server.wait_for_requests(2);
    assert_eq!(requests[1].method, "POST");
    assert_eq!(
        requests[1].target,
        format!("/session/{}/abort", encoded_component(id))
    );
    assert!(requests[1].body.is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_server_metadata_mutations_use_exact_paths_and_json() {
    let id = "ses/\"雪?../../target#fragment";
    let encoded = encoded_component(id);
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!({"id": id})),
        healthy_response(),
        ScriptedResponse::json(200, json!(true)),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let backend = backend_with_runtime(runtime);
    let session = server_session(id, PathBuf::from("/tmp"));
    let name = "quoted \"name\" with 雪 / ? and ../";

    backend.rename(&session, name).expect("rename");
    let before_archive = agent_viewer_core::spawn::now_ms();
    backend.hide(id).expect("archive");
    let after_archive = agent_viewer_core::spawn::now_ms();
    backend.unhide(id).expect("unarchive");
    backend.remove(&session).expect("delete");

    let requests = server.wait_for_requests(8);
    let mutations = requests
        .iter()
        .filter(|request| request.target != "/global/health")
        .collect::<Vec<_>>();
    assert_eq!(mutations.len(), 4);
    assert_eq!(mutations[0].method, "PATCH");
    assert_eq!(mutations[0].target, format!("/session/{encoded}"));
    assert_eq!(mutations[0].json_body(), json!({"title": name}));
    assert_eq!(mutations[1].method, "PATCH");
    assert_eq!(mutations[1].target, format!("/session/{encoded}"));
    let archive_body = mutations[1].json_body();
    let archived = archive_body
        .pointer("/time/archived")
        .and_then(Value::as_i64)
        .expect("archive timestamp");
    assert!(
        (before_archive..=after_archive).contains(&archived),
        "{archive_body}"
    );
    assert_eq!(
        archive_body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["time"]
    );
    assert_eq!(mutations[2].json_body(), json!({"time": {"archived": 0}}));
    assert_eq!(mutations[3].method, "DELETE");
    assert_eq!(mutations[3].target, format!("/session/{encoded}"));
    assert!(mutations[3].body.is_empty());
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_server_message_peek_selects_newest_nonblank_ordered_text() {
    let id = "ses/message?雪/../";
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([
                {
                    "info": {"id": "msg_old", "role": "assistant", "time": {"created": 100}},
                    "parts": [{"type": "text", "text": "older"}]
                },
                {
                    "info": {"id": "msg_whitespace", "role": "user", "time": {"created": 400}},
                    "parts": [{"type": "text", "text": "\n \n"}]
                },
                {
                    "info": {"id": "msg_tool", "role": "assistant", "time": {"created": 300}},
                    "parts": [{"type": "tool", "tool": "bash", "state": {"secret": "ignored"}}]
                },
                {
                    "info": {
                        "id": "msg_newest_text",
                        "role": "assistant",
                        "time": {"created": 200},
                        "unrelated": "ignored"
                    },
                    "parts": [
                        {"type": "text", "text": "line one\n"},
                        {"type": "tool", "tool": "read"},
                        {"type": "text", "text": "line two"}
                    ]
                }
            ]),
        ),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let missing_db = tempfile::TempDir::new().unwrap().path().join("missing.db");

    let item = runtime
        .read_last_message(&missing_db, id)
        .expect("server message read")
        .expect("newest text message");
    assert_eq!(item.role, "assistant");
    assert_eq!(item.text, "line one\nline two");
    let requests = server.wait_for_requests(2);
    assert_eq!(
        requests[1].target,
        format!("/session/{}/message?limit=200", encoded_component(id))
    );
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn opencode_server_message_peek_prefers_newer_abort_over_older_user_text() {
    let server = ScriptedServer::spawn(vec![
        healthy_response(),
        ScriptedResponse::json(
            200,
            json!([
                {
                    "info": {
                        "id": "msg_user",
                        "role": "user",
                        "time": {"created": 400}
                    },
                    "parts": [
                        {"type": "text", "text": "OLDER USER PROMPT MARKER"}
                    ]
                },
                {
                    "info": {
                        "id": "msg_error",
                        "role": "assistant",
                        "time": {"created": 500},
                        "error": {
                            "name": "MessageAbortedError",
                            "data": {
                                "message": "Aborted",
                                "responseBody": "unrelated provider payload"
                            }
                        }
                    },
                    "parts": []
                }
            ]),
        ),
    ]);
    let launcher_calls = Arc::new(AtomicUsize::new(0));
    let runtime = no_launch_runtime(
        [server.addr, unused_addr()],
        Duration::from_millis(250),
        Arc::clone(&launcher_calls),
    );
    let missing_db = tempfile::TempDir::new().unwrap().path().join("missing.db");

    let item = runtime
        .read_last_message(&missing_db, "ses_error")
        .expect("assistant error is parseable")
        .expect("assistant error is visible");
    assert_eq!(item.role, "assistant");
    assert_eq!(item.text, "Aborted");
}

// --- run-mode companions: `opencode run` sessions are one-shots, not fleet members ---

#[test]
fn opencode_run_mode_permission_marks_companion() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    // The exact blob `opencode run` writes (verified live on this box, opencode 1.17.20:
    // a `run` session stores this triple, a TUI session stores NULL).
    let run_perm = "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_enter\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_exit\",\"pattern\":\"*\",\"action\":\"deny\"}]";
    let inserts = [
        // TUI session: no parent, no permission override -> a real fleet row.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_tui',NULL,'/home/user/oc-proj','Interactive',1000,5000,NULL,NULL)"
            .to_string(),
        // `opencode run` one-shot (an /implement review pass): no parent, so parent_id
        // alone would have shown it.
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
             VALUES ('ses_run',NULL,'/home/user/oc-proj','CUR-1667 billing bug fix review',1000,4000,NULL,'{run_perm}')"
        ),
        // A permission override that is NOT the run marker must stay visible.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_other',NULL,'/home/user/oc-proj','Custom perms',1000,3000,NULL,'[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"}]')"
            .to_string(),
        // Empty string (not NULL) is the same as no override.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_empty',NULL,'/home/user/oc-proj','Empty perms',1000,2000,NULL,'')"
            .to_string(),
    ];
    let refs: Vec<&str> = inserts.iter().map(String::as_str).collect();
    let (_dir, path) = common::temp_db(&schema, &refs);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    let by = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .companion
    };

    assert!(!by("ses_tui"), "interactive TUI session must stay visible");
    assert!(by("ses_run"), "`opencode run` one-shot must be a companion");
    assert!(
        !by("ses_other"),
        "unrelated permission override is not a run marker"
    );
    assert!(!by("ses_empty"), "empty permission is not a run marker");
}

#[test]
fn opencode_run_mode_permission_marker_shapes() {
    // Absent / empty -> interactive.
    assert!(!is_run_mode_permission(None));
    assert!(!is_run_mode_permission(Some("")));
    assert!(!is_run_mode_permission(Some("   ")));
    // Not JSON, or JSON of the wrong shape -> interactive (never panics).
    assert!(!is_run_mode_permission(Some("not json at all")));
    assert!(!is_run_mode_permission(Some("{}")));
    assert!(!is_run_mode_permission(Some("[]")));
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\"}]"
    )));
    // A `question` entry that is allowed, not denied -> interactive.
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"allow\"}]"
    )));
    // The stored key order is not the source order, so order must not matter.
    assert!(is_run_mode_permission(Some(
        "[{\"action\":\"deny\",\"permission\":\"question\",\"pattern\":\"*\"}]"
    )));
    // The github-action variant writes the `question` deny alone, without the plan pair.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
    // Extra unrelated entries around the marker must not hide it.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"},\
          {\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
}

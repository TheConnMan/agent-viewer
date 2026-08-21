use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_viewer_core::{
    AttachRefusal, Backend, BackendKind, Capabilities, Error, Session, SessionOrigin, SpawnResult,
    Status, TailEvent,
};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header::CONTENT_TYPE};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt as _;

const LOGIN: &str = "bcconn2112@gmail.com";
const ANDROID_ORIGIN: &str = "https://appassets.androidplatform.net";

#[derive(Default)]
struct FakeCalls {
    list: AtomicUsize,
    spawn: AtomicUsize,
    stop: AtomicUsize,
    hide: AtomicUsize,
    unhide: AtomicUsize,
    rename: AtomicUsize,
    attach: AtomicUsize,
}

struct FakeBackend {
    kind: BackendKind,
    sessions: Vec<Session>,
    calls: Arc<FakeCalls>,
    attach_script: Option<String>,
}

impl FakeBackend {
    fn new(session: Session, calls: Arc<FakeCalls>) -> Self {
        Self {
            kind: BackendKind::Codex,
            sessions: vec![session],
            calls,
            attach_script: None,
        }
    }

    fn attachable(session: Session, calls: Arc<FakeCalls>, script: impl Into<String>) -> Self {
        Self::attachable_kind(BackendKind::Codex, session, calls, script)
    }

    fn attachable_kind(
        kind: BackendKind,
        session: Session,
        calls: Arc<FakeCalls>,
        script: impl Into<String>,
    ) -> Self {
        Self::attachable_sessions(kind, vec![session], calls, script)
    }

    fn attachable_sessions(
        kind: BackendKind,
        sessions: Vec<Session>,
        calls: Arc<FakeCalls>,
        script: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            sessions,
            calls,
            attach_script: Some(script.into()),
        }
    }
}

impl Backend for FakeBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            attach: self.attach_script.is_some(),
            rename: true,
            archive: true,
            delete: false,
            stop: true,
        }
    }

    fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
        self.calls.list.fetch_add(1, Ordering::SeqCst);
        Ok(self.sessions.clone())
    }

    fn turn_activity(
        &self,
        _session: &Session,
        _window: Duration,
    ) -> agent_viewer_core::Result<Vec<i64>> {
        Ok(vec![1_000, 1_500, 2_000])
    }

    fn tail(
        &self,
        _session: &Session,
        max_events: usize,
    ) -> agent_viewer_core::Result<Vec<TailEvent>> {
        let mut events = vec![
            TailEvent::User("Please inspect the failure".to_string()),
            TailEvent::Tool {
                name: "cargo test".to_string(),
                detail: "agent_viewer_server".to_string(),
            },
            TailEvent::Agent("The server contract is ready".to_string()),
        ];
        let keep_from = events.len().saturating_sub(max_events);
        Ok(events.split_off(keep_from))
    }

    fn spawn(
        &self,
        _dir: &Path,
        _task: &str,
        _model: Option<&str>,
        _effort: Option<&str>,
    ) -> agent_viewer_core::Result<SpawnResult> {
        self.calls.spawn.fetch_add(1, Ordering::SeqCst);
        Ok(SpawnResult {
            pid: None,
            session_id: Some("spawned-session".to_string()),
        })
    }

    fn available_models(&self) -> Vec<String> {
        vec!["default".to_string(), "gpt-5.6-sol".to_string()]
    }

    fn hide(&self, _id: &str) -> agent_viewer_core::Result<()> {
        self.calls.hide.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn unhide(&self, _id: &str) -> agent_viewer_core::Result<()> {
        self.calls.unhide.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self, _session: &Session) -> agent_viewer_core::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        Err(Error::Unsupported("fake"))
    }

    fn rename(&self, _session: &Session, _name: &str) -> agent_viewer_core::Result<()> {
        self.calls.rename.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn attach_command(&self, _session: &Session) -> Result<Command, AttachRefusal> {
        let Some(script) = &self.attach_script else {
            return Err(AttachRefusal::new("fake backend does not attach"));
        };
        self.calls.attach.fetch_add(1, Ordering::SeqCst);
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        Ok(command)
    }
}

fn session(cwd: PathBuf) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "session-1".to_string(),
        short_id: None,
        origin: SessionOrigin::Background,
        title: "Mobile companion".to_string(),
        cwd,
        git_branch: Some("task/mobile-companion".to_string()),
        status: Status::Working,
        created_at_ms: 1_000,
        updated_at_ms: 2_000,
        hidden: false,
        companion: false,
        subagent: false,
        summary: "Building the server".to_string(),
        pid: Some(42),
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn request(method: Method, uri: &str, authorized: bool) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if authorized {
        builder = builder.header("Tailscale-User-Login", LOGIN);
    }
    builder.body(Body::empty()).expect("valid request")
}

fn body_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Tailscale-User-Login", LOGIN)
        .body(Body::from(body.to_string()))
        .expect("valid request")
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    let mut request = body_request(method, uri, body);
    request.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("valid media type"),
    );
    request
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    serde_json::from_slice(&bytes).expect("JSON response")
}

struct ProjectFixture {
    root: PathBuf,
    cwd: PathBuf,
}

impl Drop for ProjectFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn project_fixture() -> ProjectFixture {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "agent-viewer-server-http-contract-{}-{nonce}",
        std::process::id()
    ));
    let cwd = root.join("android/app");
    std::fs::create_dir_all(root.join(".git")).expect("git marker");
    std::fs::create_dir_all(&cwd).expect("session cwd");
    ProjectFixture { root, cwd }
}

type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestServer {
    address: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_server(app: axum::Router) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve terminal contract");
    });
    TestServer { address, task }
}

async fn connect_terminal(
    server: &TestServer,
    login: Option<&str>,
    origin: Option<&str>,
) -> Result<ClientSocket, WebSocketError> {
    connect_terminal_session(server, "session-1", BackendKind::Codex, login, origin).await
}

async fn connect_terminal_for_backend(
    server: &TestServer,
    backend: BackendKind,
    login: Option<&str>,
    origin: Option<&str>,
) -> Result<ClientSocket, WebSocketError> {
    connect_terminal_session(server, "session-1", backend, login, origin).await
}

async fn connect_terminal_session(
    server: &TestServer,
    id: &str,
    backend: BackendKind,
    login: Option<&str>,
    origin: Option<&str>,
) -> Result<ClientSocket, WebSocketError> {
    let mut request = format!(
        "ws://{}/sessions/{id}/terminal?backend={backend:?}",
        server.address
    )
    .into_client_request()
    .expect("terminal WebSocket request");
    if let Some(login) = login {
        request.headers_mut().insert(
            "Tailscale-User-Login",
            login.parse().expect("valid login header"),
        );
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().expect("valid origin header"));
    }
    connect_async(request).await.map(|(socket, _)| socket)
}

fn websocket_status(error: WebSocketError) -> StatusCode {
    match error {
        WebSocketError::Http(response) => response.status(),
        other => panic!("expected HTTP handshake refusal, got {other}"),
    }
}

async fn next_terminal_json(socket: &mut ClientSocket) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.expect("terminal message") {
                Ok(Message::Text(text)) => {
                    return serde_json::from_str(text.as_str()).expect("terminal JSON frame");
                }
                Ok(Message::Binary(bytes)) => {
                    return serde_json::from_slice(&bytes).expect("terminal JSON frame");
                }
                Ok(Message::Ping(bytes)) => {
                    socket
                        .send(Message::Pong(bytes))
                        .await
                        .expect("terminal pong");
                }
                Ok(Message::Close(frame)) => panic!("terminal closed early: {frame:?}"),
                Ok(_) => {}
                Err(error) => panic!("terminal receive failed: {error}"),
            }
        }
    })
    .await
    .expect("terminal message timeout")
}

async fn terminal_json_matching(
    socket: &mut ClientSocket,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            let value = next_terminal_json(socket).await;
            if predicate(&value) {
                return value;
            }
        }
    })
    .await
    .expect("matching terminal frame timeout")
}

async fn wait_for_pid_exit(pid: u32) {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while path.exists() && tokio::time::Instant::now() < deadline {
        sleep(Duration::from_millis(25)).await;
    }
    assert!(!path.exists(), "terminal child {pid} must be reaped");
}

async fn wait_for_pids(path: &Path, expected: usize) -> Vec<u32> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let parsed = contents
                .lines()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>();
            if let Ok(pids) = parsed
                && pids.len() == expected
            {
                return pids;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected} terminal pid lines at {}",
            path.display()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_file_bytes(path: &Path) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(path)
            && !bytes.is_empty()
        {
            return bytes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected terminal evidence at {}",
            path.display()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

fn attachable_app(script: impl Into<String>, calls: Arc<FakeCalls>) -> axum::Router {
    let fixture = project_fixture();
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable(
        attachable_session,
        calls,
        script,
    ))])
}

#[test]
fn tail_events_and_capabilities_have_json_contracts() {
    let events = vec![
        TailEvent::Agent("ready".to_string()),
        TailEvent::User("continue".to_string()),
        TailEvent::Tool {
            name: "cargo".to_string(),
            detail: "test".to_string(),
        },
    ];

    assert_eq!(
        serde_json::to_value(events).expect("TailEvent implements Serialize"),
        json!([
            { "Agent": "ready" },
            { "User": "continue" },
            { "Tool": { "name": "cargo", "detail": "test" } }
        ])
    );
    assert_eq!(
        serde_json::to_value(Capabilities::none()).expect("Capabilities implements Serialize"),
        json!({
            "spawn": false,
            "attach": false,
            "rename": false,
            "archive": false,
            "delete": false,
            "stop": false
        })
    );
}

#[tokio::test]
async fn router_exposes_authenticated_backend_contracts() {
    let fixture = project_fixture();
    let project_root = &fixture.root;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::new(
        session(fixture.cwd.clone()),
        Arc::clone(&calls),
    ))]);
    let list_calls_after_construction = calls.list.load(Ordering::SeqCst);
    assert_eq!(list_calls_after_construction, 1);

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/health", false))
        .await
        .expect("health response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/health", true))
        .await
        .expect("health response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "status": "ok" }));

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/sessions", true))
        .await
        .expect("sessions response");
    assert_eq!(response.status(), StatusCode::OK);
    let sessions = json_body(response).await;
    assert_eq!(sessions.as_array().map(Vec::len), Some(1));
    assert_eq!(sessions[0]["id"], "session-1");
    assert_eq!(sessions[0]["backend"], "Codex");
    assert_eq!(
        sessions[0]["project_root"],
        project_root.to_string_lossy().as_ref()
    );

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/sessions/session-1/tail?n=2", true))
        .await
        .expect("tail response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!([
            { "Tool": { "name": "cargo test", "detail": "agent_viewer_server" } },
            { "Agent": "The server contract is ready" }
        ])
    );

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/sessions/session-1/caps", true))
        .await
        .expect("capabilities response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({
            "spawn": true,
            "attach": false,
            "rename": true,
            "archive": true,
            "delete": false,
            "stop": true
        })
    );

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/sessions/session-1/pulse", true))
        .await
        .expect("pulse response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!([1_000, 1_500, 2_000]));

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/models", true))
        .await
        .expect("models response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({
            "Claude": [],
            "Codex": ["default", "gpt-5.6-sol"]
        })
    );

    let spawn_body = json!({
        "provider": "codex",
        "dir": project_root.join("android").to_string_lossy(),
        "task": "Build the phone companion",
        "model": "gpt-5.6-sol"
    });
    let rename_body = json!({ "name": "Phone companion server" });

    let response = app
        .clone()
        .oneshot(body_request(Method::POST, "/sessions", spawn_body.clone()))
        .await
        .expect("spawn response without JSON media type");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(calls.spawn.load(Ordering::SeqCst), 0);

    let response = app
        .clone()
        .oneshot(request(Method::POST, "/sessions/session-1/stop", true))
        .await
        .expect("stop response without JSON media type");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(calls.stop.load(Ordering::SeqCst), 0);

    let response = app
        .clone()
        .oneshot(request(Method::POST, "/sessions/session-1/hide", true))
        .await
        .expect("hide response without JSON media type");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(calls.hide.load(Ordering::SeqCst), 0);

    let response = app
        .clone()
        .oneshot(request(Method::POST, "/sessions/session-1/unhide", true))
        .await
        .expect("unhide response without JSON media type");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(calls.unhide.load(Ordering::SeqCst), 0);

    let response = app
        .clone()
        .oneshot(body_request(
            Method::PATCH,
            "/sessions/session-1",
            rename_body.clone(),
        ))
        .await
        .expect("rename response without JSON media type");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(calls.rename.load(Ordering::SeqCst), 0);

    let response = app
        .clone()
        .oneshot(json_request(Method::POST, "/sessions", spawn_body))
        .await
        .expect("spawn response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({
            "backend": "Codex",
            "session_id": "spawned-session",
            "pid": null
        })
    );
    assert_eq!(calls.spawn.load(Ordering::SeqCst), 1);

    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/sessions/session-1/stop",
            json!({}),
        ))
        .await
        .expect("stop response");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(calls.stop.load(Ordering::SeqCst), 1);

    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/sessions/session-1/hide",
            json!({}),
        ))
        .await
        .expect("hide response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "status": "ok" }));
    assert_eq!(calls.hide.load(Ordering::SeqCst), 1);

    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/sessions/session-1/unhide",
            json!({}),
        ))
        .await
        .expect("unhide response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "status": "ok" }));
    assert_eq!(calls.unhide.load(Ordering::SeqCst), 1);

    let response = app
        .oneshot(json_request(
            Method::PATCH,
            "/sessions/session-1",
            rename_body,
        ))
        .await
        .expect("rename response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "status": "ok" }));
    assert_eq!(calls.rename.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.list.load(Ordering::SeqCst),
        list_calls_after_construction
    );
}

#[tokio::test]
async fn terminal_requires_tailscale_auth_and_exact_android_origin() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app("sleep 30", Arc::clone(&calls))).await;

    let missing_auth = connect_terminal(&server, None, Some(ANDROID_ORIGIN))
        .await
        .expect_err("missing login must be refused");
    assert_eq!(websocket_status(missing_auth), StatusCode::UNAUTHORIZED);

    let missing_origin = connect_terminal(&server, Some(LOGIN), None)
        .await
        .expect_err("missing origin must be refused");
    assert_eq!(websocket_status(missing_origin), StatusCode::FORBIDDEN);

    let wrong_origin = connect_terminal(&server, Some(LOGIN), Some("https://example.com"))
        .await
        .expect_err("wrong origin must be refused");
    assert_eq!(websocket_status(wrong_origin), StatusCode::FORBIDDEN);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_refuses_sessions_without_attach_capability_before_spawning() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::new(
        session(fixture.cwd.clone()),
        Arc::clone(&calls),
    ))]);
    let server = start_server(app).await;

    let refusal = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect_err("unsupported attach must be refused");
    assert_eq!(websocket_status(refusal), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_initial_screen_includes_geometry_cursor_and_visible_rows() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(
        "printf 'READY\\r\\n'; sleep 30",
        Arc::clone(&calls),
    ))
    .await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let screen = terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"].as_array().is_some_and(|lines| {
                lines
                    .iter()
                    .any(|line| line.as_str().is_some_and(|line| line.contains("READY")))
            })
    })
    .await;
    assert!(screen["seq"].as_u64().is_some());
    let rows = screen["rows"].as_u64().expect("screen rows");
    let cols = screen["cols"].as_u64().expect("screen columns");
    assert_eq!(
        screen["lines"].as_array().map(Vec::len),
        Some(rows as usize)
    );
    assert!(rows > 0);
    assert!(cols > 0);
    assert!(screen["cursor_row"].as_u64().is_some_and(|row| row < rows));
    assert!(screen["cursor_col"].as_u64().is_some_and(|col| col < cols));
    assert_eq!(screen["running"], true);

    socket.close(None).await.expect("close terminal");
    assert_eq!(calls.attach.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn terminal_input_preserves_utf8_and_control_bytes() {
    let calls = Arc::new(FakeCalls::default());
    let script = "stty raw -echo; printf 'INPUT READY\\r\\n'; python3 -c 'import sys; data=sys.stdin.buffer.read(4); print(data.hex(), flush=True)'";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "INPUT READY"))
    })
    .await;

    socket
        .send(Message::Text(
            json!({ "type": "input", "text": "hé\u{0003}" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send terminal input");

    let screen = terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "68c3a903"))
    })
    .await;
    assert!(screen["seq"].as_u64().is_some());
}

#[tokio::test]
async fn terminal_submit_uses_bracketed_paste_and_acknowledges_the_delivery_id() {
    let fixture = project_fixture();
    let evidence = fixture.root.join("bracketed-submit.bin");
    let calls = Arc::new(FakeCalls::default());
    let script = format!(
        "stty raw -echo; printf 'Resuming session\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H\\033[?2004h› ready\\r\\n'; python3 -c 'import sys; data=sys.stdin.buffer.read(25); open(\"{}\", \"wb\").write(data)'; sleep 30",
        evidence.display()
    );
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable(
        attachable_session,
        calls,
        script,
    ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| value["ready"] == true).await;

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 17, "text": "café 漢字" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit terminal line");

    let delivered = wait_for_file_bytes(&evidence).await;
    assert_eq!(
        delivered,
        b"\x1b[200~caf\xc3\xa9 \xe6\xbc\xa2\xe5\xad\x97\x1b[201~\r"
    );
    let ack = terminal_json_matching(&mut socket, |value| {
        value["type"] == "ack" || value["type"] == "error"
    })
    .await;
    assert_eq!(
        ack["type"], "ack",
        "successful PTY write must be acknowledged without screen echo"
    );
    assert_eq!(ack["id"], 17);
}

#[tokio::test]
async fn terminal_submit_without_bracketed_paste_delays_enter_past_burst_detection() {
    let fixture = project_fixture();
    let evidence = fixture.root.join("plain-submit.txt");
    let calls = Arc::new(FakeCalls::default());
    let script = format!(
        "stty raw -echo; printf 'Resuming session\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H› ready\\r\\n'; python3 -c 'import sys,time; data=sys.stdin.buffer.read(2); started=time.monotonic(); tail=sys.stdin.buffer.read(1); open(\"{}\", \"w\").write(data.hex()+\":\"+tail.hex()+\":\"+str(time.monotonic()-started))'; sleep 30",
        evidence.display()
    );
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable(
        attachable_session,
        calls,
        script,
    ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| value["ready"] == true).await;

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 23, "text": "go" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit terminal line");

    let result =
        String::from_utf8(wait_for_file_bytes(&evidence).await).expect("captured raw submission");
    let elapsed = result
        .rsplit_once(':')
        .and_then(|(_, elapsed)| elapsed.parse::<f64>().ok())
        .expect("submission delay");
    assert!(
        elapsed >= 0.12,
        "enter arrived inside the native paste burst window: {elapsed}"
    );

    let ack = terminal_json_matching(&mut socket, |value| {
        value["type"] == "ack" || value["type"] == "error"
    })
    .await;
    assert_eq!(
        ack["type"], "ack",
        "successful PTY write must be acknowledged without screen echo"
    );
    assert_eq!(ack["id"], 23);
}

#[tokio::test]
async fn terminal_submit_write_failure_returns_an_error_without_acknowledging() {
    let calls = Arc::new(FakeCalls::default());
    let script = "printf 'Resuming session\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H› ready\\r\\n'; sleep 0.75; exec </dev/null >/dev/null 2>&1; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| value["ready"] == true).await;
    sleep(Duration::from_millis(100)).await;

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 29, "text": "do not acknowledge" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit after PTY closed");

    let failure = next_terminal_json(&mut socket).await;
    assert_eq!(
        failure["type"], "error",
        "failed PTY write must report an error before any other response"
    );
    let ack = timeout(Duration::from_millis(250), async {
        loop {
            let value = next_terminal_json(&mut socket).await;
            if value["type"] == "ack" && value["id"] == 29 {
                return value;
            }
        }
    })
    .await;
    assert!(ack.is_err(), "failed PTY write must not be acknowledged");
}

#[tokio::test]
async fn terminal_submit_rejects_control_characters_without_writing_or_acknowledging() {
    for (case, text) in [
        ("carriage-return", "first\rsecond"),
        ("line-feed", "first\nsecond"),
        ("paste-escape", "first\u{1b}[201~second"),
        ("null", "first\0second"),
    ] {
        let fixture = project_fixture();
        let evidence = fixture.root.join(format!("control-{case}.bin"));
        let calls = Arc::new(FakeCalls::default());
        let script = format!(
            "stty raw -echo; printf 'Resuming session\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H\\033[?2004h› ready\\r\\n'; python3 -c 'import sys; data=sys.stdin.buffer.read(1); open(\"{}\", \"wb\").write(data)'; sleep 30",
            evidence.display()
        );
        let mut attachable_session = session(fixture.cwd.clone());
        attachable_session.daemon_hosted = true;
        let app = agent_viewer_server::router_with_backends(vec![Box::new(
            FakeBackend::attachable(attachable_session, calls, script),
        )]);
        let server = start_server(app).await;
        let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
            .await
            .expect("attach terminal");
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await;

        let id = match case {
            "carriage-return" => 43,
            "line-feed" => 44,
            "paste-escape" => 45,
            "null" => 46,
            _ => unreachable!(),
        };
        socket
            .send(Message::Text(
                json!({ "type": "submit", "id": id, "text": text })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("submit control text");

        let response = terminal_json_matching(&mut socket, |value| {
            value["type"] == "error" || value["type"] == "ack"
        })
        .await;
        assert_eq!(response["type"], "error", "control case {case}");
        assert!(
            response["message"]
                .as_str()
                .is_some_and(|message| message.contains("control")),
            "control case {case} must be rejected before PTY delivery"
        );
        let ack = timeout(Duration::from_millis(250), async {
            terminal_json_matching(&mut socket, |value| {
                value["type"] == "ack" && value["id"] == id
            })
            .await
        })
        .await;
        assert!(ack.is_err(), "control case {case} must not be acknowledged");
        sleep(Duration::from_millis(100)).await;
        assert!(
            !evidence.exists(),
            "control case {case} reached the terminal child"
        );
    }
}

#[tokio::test]
async fn codex_terminal_becomes_ready_only_after_a_stable_native_prompt() {
    let calls = Arc::new(FakeCalls::default());
    let script = "sleep 0.15; printf 'Resuming session\\r\\n'; sleep 0.15; printf '\\033[2J\\033[H› ready\\r\\n'; sleep 0.3; printf '\\033[2J\\033[Hdirect input is disabled\\r\\n'; sleep 0.15; printf '\\033[2J\\033[H› ready\\r\\n'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let initial = next_terminal_json(&mut socket).await;
    assert_eq!(initial["type"], "screen");
    assert_eq!(initial["ready"], false, "blank terminal must not be ready");
    let resuming = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.contains("Resuming session"))
            })
        })
    })
    .await;
    assert_eq!(resuming["ready"], false);
    let first_prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.trim_start().starts_with("› "))
            })
        })
    })
    .await;
    assert_eq!(first_prompt["ready"], false);
    let blocked = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.contains("direct input is disabled"))
            })
        })
    })
    .await;
    assert_eq!(blocked["ready"], false);
    let stable_prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.trim_start().starts_with("› "))
            })
        }) && value["seq"].as_u64() > first_prompt["seq"].as_u64()
    })
    .await;
    assert_eq!(stable_prompt["ready"], false);

    let premature = timeout(Duration::from_millis(550), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(premature.is_err(), "Codex prompt became ready before 600ms");
    timeout(Duration::from_secs(1), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await
    .expect("stable Codex prompt becomes ready");
}

#[tokio::test]
async fn claude_terminal_becomes_ready_only_after_startup_markers_clear() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.15; printf 'Waking session\\r\\n'; sleep 0.15; printf '\\033[2J\\033[HAttaching\\r\\n'; sleep 0.15; printf '\\033[2J\\033[H❯ '; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");

    let initial = next_terminal_json(&mut socket).await;
    assert_eq!(initial["type"], "screen");
    assert_eq!(initial["ready"], false, "blank terminal must not be ready");
    for marker in ["Waking session", "Attaching"] {
        let startup = terminal_json_matching(&mut socket, |value| {
            value["lines"].as_array().is_some_and(|lines| {
                lines
                    .iter()
                    .any(|line| line.as_str().is_some_and(|line| line.contains(marker)))
            })
        })
        .await;
        assert_eq!(startup["ready"], false);
    }
    let prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines
                .iter()
                .any(|line| line.as_str().is_some_and(|line| line.trim() == "❯"))
        })
    })
    .await;
    assert_eq!(prompt["ready"], false);
    assert_eq!(prompt["cursor_row"], 0);
    assert_eq!(prompt["cursor_col"], 2);

    let premature = timeout(Duration::from_millis(550), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(
        premature.is_err(),
        "Claude prompt became ready before 600ms"
    );
    timeout(Duration::from_secs(1), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await
    .expect("stable Claude prompt becomes ready");
}

#[tokio::test]
async fn claude_terminal_becomes_ready_when_startup_and_prompt_share_one_pty_read() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.15; printf 'Attaching\\r\\n\\033[2J\\033[H❯ '; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");

    let prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines
                .iter()
                .any(|line| line.as_str().is_some_and(|line| line.trim() == "❯"))
        })
    })
    .await;
    assert_eq!(prompt["ready"], false);
    assert_eq!(prompt["cursor_row"], 0);
    assert_eq!(prompt["cursor_col"], 2);

    timeout(Duration::from_millis(1_500), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await
    .expect("stable Claude composer becomes ready without an observed startup snapshot");
}

#[tokio::test]
async fn claude_transcript_prompt_off_cursor_row_stays_unready_and_rejects_submit() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.15; printf '\\033[2J\\033[H❯\\r\\nassistant transcript'; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");

    let transcript = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| line.as_str() == Some("❯"))
                && lines
                    .iter()
                    .any(|line| line.as_str() == Some("assistant transcript"))
        })
    })
    .await;
    assert_eq!(transcript["cursor_row"], 1);
    assert_eq!(transcript["ready"], false);
    let unexpected_ready = timeout(Duration::from_millis(750), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(
        unexpected_ready.is_err(),
        "Claude transcript prompt must remain unready after the settle window"
    );

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 47, "text": "must not reach transcript prompt" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit with Claude transcript prompt");
    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(response["type"], "error");
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 47
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "unready Claude transcript prompt must not be acknowledged"
    );
}

#[tokio::test]
async fn claude_prompt_with_hidden_cursor_stays_unready_and_rejects_submit() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.15; printf '\\033[2J\\033[H❯ \\033[?25l'; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");

    let prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines
                .first()
                .and_then(Value::as_str)
                .is_some_and(|line| line.trim() == "❯")
        })
    })
    .await;
    assert_eq!(prompt["cursor_row"], 0);
    assert_eq!(prompt["cursor_col"], 2);
    assert_eq!(prompt["ready"], false);
    let unexpected_ready = timeout(Duration::from_millis(750), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(
        unexpected_ready.is_err(),
        "Claude prompt with hidden cursor must remain unready after the settle window"
    );

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 59, "text": "must wait for visible cursor" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit with hidden Claude cursor");
    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(response["type"], "error");
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 59
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "unready hidden cursor prompt must not be acknowledged"
    );
}

#[tokio::test]
async fn codex_prompt_without_startup_marker_stays_unready_and_rejects_submit() {
    let calls = Arc::new(FakeCalls::default());
    let script = "sleep 0.15; printf '\\033[2J\\033[H› ready'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let prompt = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.trim_start().starts_with("› "))
            })
        })
    })
    .await;
    assert_eq!(prompt["cursor_row"], 0);
    assert_eq!(prompt["ready"], false);
    let unexpected_ready = timeout(Duration::from_millis(750), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(
        unexpected_ready.is_err(),
        "Codex prompt without startup marker must remain unready"
    );

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 53, "text": "must wait for Codex startup" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit before Codex startup marker");
    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(response["type"], "error");
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 53
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "unready Codex prompt must not be acknowledged"
    );
}

#[tokio::test]
async fn terminal_submit_is_rejected_until_the_native_terminal_is_ready() {
    let calls = Arc::new(FakeCalls::default());
    let script = "sleep 2; printf 'Resuming session\\r\\n'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let initial = next_terminal_json(&mut socket).await;
    assert_eq!(initial["type"], "screen");
    assert_ne!(initial["ready"], true, "blank terminal must not be ready");
    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 31, "text": "must not be lost" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit during startup");

    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(
        response["type"], "error",
        "submit before native readiness must be rejected"
    );
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 31
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "rejected startup submit must not be acknowledged"
    );
}

#[tokio::test]
async fn codex_nonblank_precomposer_banner_stays_unready_and_rejects_submit() {
    let calls = Arc::new(FakeCalls::default());
    let script = "sleep 0.15; printf 'Codex startup banner\\r\\n'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let banner = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.contains("Codex startup banner"))
            })
        })
    })
    .await;
    assert_eq!(
        banner["ready"], false,
        "arbitrary Codex banner must fail closed"
    );

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 37, "text": "must wait for the composer" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit before Codex composer");
    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(response["type"], "error");
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 37
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "unready Codex submit must not be acknowledged"
    );
}

#[tokio::test]
async fn claude_nonblank_precomposer_banner_stays_unready_and_rejects_submit() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.15; printf 'Claude startup banner\\r\\n'; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");
    let banner = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.contains("Claude startup banner"))
            })
        })
    })
    .await;
    assert_eq!(
        banner["ready"], false,
        "arbitrary Claude banner must fail closed"
    );
    let unexpected_ready = timeout(Duration::from_millis(750), async {
        terminal_json_matching(&mut socket, |value| value["ready"] == true).await
    })
    .await;
    assert!(
        unexpected_ready.is_err(),
        "arbitrary Claude banner must remain unready after the settle window"
    );

    socket
        .send(Message::Text(
            json!({ "type": "submit", "id": 41, "text": "must wait for the composer" })
                .to_string()
                .into(),
        ))
        .await
        .expect("submit before Claude composer");
    let response = terminal_json_matching(&mut socket, |value| {
        value["type"] == "error" || value["type"] == "ack"
    })
    .await;
    assert_eq!(response["type"], "error");
    let ack = timeout(Duration::from_millis(250), async {
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "ack" && value["id"] == 41
        })
        .await
    })
    .await;
    assert!(
        ack.is_err(),
        "unready Claude submit must not be acknowledged"
    );
}

#[tokio::test]
async fn codex_startup_words_in_transcript_do_not_revoke_latched_readiness() {
    let calls = Arc::new(FakeCalls::default());
    let script = "sleep 0.1; printf 'Resuming session\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H› ready\\r\\n'; sleep 0.8; printf '\\033[2J\\033[HTranscript says Resuming session completed\\r\\n'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| value["ready"] == true).await;

    let transcript = terminal_json_matching(&mut socket, |value| {
        value["lines"].as_array().is_some_and(|lines| {
            lines.iter().any(|line| {
                line.as_str()
                    .is_some_and(|line| line.contains("Transcript says Resuming session"))
            })
        })
    })
    .await;
    assert_eq!(
        transcript["ready"], true,
        "startup words after readiness must be ordinary transcript text"
    );
}

#[tokio::test]
async fn claude_startup_words_in_transcript_do_not_revoke_latched_readiness() {
    let fixture = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.backend = BackendKind::Claude;
    let script = "sleep 0.1; printf 'Attaching\\r\\n'; sleep 0.1; printf '\\033[2J\\033[H❯ '; sleep 0.8; printf '\\r\\nTranscript says Attaching completed\\r\\n❯ '; sleep 30";
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            attachable_session,
            Arc::clone(&calls),
            script,
        ))]);
    let server = start_server(app).await;
    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| value["ready"] == true).await;

    let transcript = terminal_json_matching(&mut socket, |value| {
        let Some(lines) = value["lines"].as_array() else {
            return false;
        };
        let Some(cursor_row) = value["cursor_row"].as_u64() else {
            return false;
        };
        lines.iter().any(|line| {
            line.as_str()
                .is_some_and(|line| line.contains("Transcript says Attaching"))
        }) && lines
            .get(cursor_row as usize)
            .and_then(Value::as_str)
            .is_some_and(|line| line.trim() == "❯")
    })
    .await;
    assert_eq!(
        transcript["ready"], true,
        "startup words after readiness must be ordinary transcript text"
    );
}

#[tokio::test]
async fn terminal_resize_applies_valid_geometry_and_rejects_unbounded_sizes() {
    let calls = Arc::new(FakeCalls::default());
    let script = "trap 'stty size' WINCH; printf 'START\\r\\n'; while :; do sleep 1; done";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "START"))
    })
    .await;

    socket
        .send(Message::Text(
            json!({ "type": "resize", "rows": 0, "cols": 10_000 })
                .to_string()
                .into(),
        ))
        .await
        .expect("send invalid resize");
    let error = terminal_json_matching(&mut socket, |value| value["type"] == "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );

    socket
        .send(Message::Text(
            json!({ "type": "resize", "rows": 14, "cols": 37 })
                .to_string()
                .into(),
        ))
        .await
        .expect("send terminal resize");
    let resized = terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen" && value["rows"] == 14 && value["cols"] == 37
    })
    .await;
    assert!(
        resized["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line == "14 37"))
    );

    socket.close(None).await.expect("close terminal");
}

#[tokio::test]
async fn terminal_exit_sends_final_screen_with_running_false() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app("printf 'DONE\\r\\n'", calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let exited = terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen" && value["running"] == false
    })
    .await;
    assert!(
        exited["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line == "DONE"))
    );
}

#[tokio::test]
async fn terminal_allows_one_connection_per_session() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(
        "printf 'FIRST\\r\\n'; sleep 30",
        Arc::clone(&calls),
    ))
    .await;
    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    terminal_json_matching(&mut first, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "FIRST"))
    })
    .await;

    let duplicate = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect_err("duplicate attach must be refused");
    assert_eq!(websocket_status(duplicate), StatusCode::CONFLICT);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 1);

    first.close(None).await.expect("close first terminal");
}

#[tokio::test]
async fn terminal_capacity_is_released_when_an_ordinary_socket_closes() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("capacity.pids");
    let sessions = (1..=5)
        .map(|index| {
            let mut value = session(fixture.cwd.clone());
            value.id = format!("session-{index}");
            value.daemon_hosted = true;
            value
        })
        .collect();
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![Box::new(
        FakeBackend::attachable_sessions(
            BackendKind::Codex,
            sessions,
            Arc::clone(&calls),
            format!(
                "printf '%s\\n' $$ >> '{}'; printf 'CAPACITY\\r\\n'; sleep 30",
                pid_file.display()
            ),
        ),
    )]);
    let server = start_server(app).await;

    let mut sockets = Vec::new();
    for index in 1..=4 {
        let mut socket = connect_terminal_session(
            &server,
            &format!("session-{index}"),
            BackendKind::Codex,
            Some(LOGIN),
            Some(ANDROID_ORIGIN),
        )
        .await
        .expect("attach retained terminal owner");
        terminal_json_matching(&mut socket, |value| {
            value["type"] == "screen"
                && value["lines"]
                    .as_array()
                    .is_some_and(|lines| lines.iter().any(|line| line == "CAPACITY"))
        })
        .await;
        sockets.push(socket);
    }
    assert_eq!(calls.attach.load(Ordering::SeqCst), 4);
    let first_four_pids = wait_for_pids(&pid_file, 4).await;
    assert_eq!(first_four_pids.len(), 4);

    let full = connect_terminal_session(
        &server,
        "session-5",
        BackendKind::Codex,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect_err("four open terminal views must fill capacity");
    assert_eq!(websocket_status(full), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 4);

    let mut closed = sockets.remove(0);
    closed
        .close(None)
        .await
        .expect("ordinary socket disconnect");
    drop(closed);
    wait_for_pid_exit(first_four_pids[0]).await;

    let mut fifth = connect_terminal_session(
        &server,
        "session-5",
        BackendKind::Codex,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("ordinary disconnect must release capacity");
    terminal_json_matching(&mut fifth, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "CAPACITY"))
    })
    .await;
    assert_eq!(calls.attach.load(Ordering::SeqCst), 5);
    let all_pids = wait_for_pids(&pid_file, 5).await;
    assert_eq!(all_pids.len(), 5);

    fifth.close(None).await.expect("close fifth socket");
    wait_for_pid_exit(all_pids[4]).await;
    for (mut socket, pid) in sockets.into_iter().zip(first_four_pids.into_iter().skip(1)) {
        socket.close(None).await.expect("close terminal socket");
        wait_for_pid_exit(pid).await;
    }
}

#[tokio::test]
async fn terminal_explicit_close_kills_and_reaps_attach_child() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("explicit-close.pid");
    let script = format!(
        "printf '%s' $$ > '{}'; printf 'EXPLICIT CLOSE\\r\\n'; sleep 30",
        pid_file.display()
    );
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "EXPLICIT CLOSE"))
    })
    .await;
    let pid = std::fs::read_to_string(&pid_file)
        .expect("terminal pid file")
        .parse::<u32>()
        .expect("terminal pid");

    socket
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("explicit terminal close failed: {error}"),
            }
        }
    })
    .await
    .expect("explicit terminal close timeout");
    wait_for_pid_exit(pid).await;
}

#[tokio::test]
async fn terminal_transport_rejects_oversized_websocket_messages() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app("printf 'SIZE LIMIT\\r\\n'; sleep 30", calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "SIZE LIMIT"))
    })
    .await;

    socket
        .send(Message::Text("x".repeat(32 * 1_024).into()))
        .await
        .expect("send oversized message");
    let close = timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => return frame,
                Some(Ok(Message::Text(text))) => {
                    panic!("oversized message reached application handler: {text}")
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("transport did not send a close frame: {error}"),
                None => panic!("transport closed without a size close frame"),
            }
        }
    })
    .await
    .expect("oversized transport close timeout");
    assert_eq!(close.code, CloseCode::Size);
}

#[tokio::test]
async fn terminal_identity_includes_backend_when_session_ids_collide() {
    let codex_fixture = project_fixture();
    let claude_fixture = project_fixture();
    let mut codex_session = session(codex_fixture.cwd.clone());
    codex_session.daemon_hosted = true;
    let mut claude_session = session(claude_fixture.cwd.clone());
    claude_session.backend = BackendKind::Claude;
    claude_session.daemon_hosted = true;
    let codex_calls = Arc::new(FakeCalls::default());
    let claude_calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![
        Box::new(FakeBackend::attachable_kind(
            BackendKind::Codex,
            codex_session,
            Arc::clone(&codex_calls),
            "printf 'CODEX TERMINAL\\r\\n'; sleep 30",
        )),
        Box::new(FakeBackend::attachable_kind(
            BackendKind::Claude,
            claude_session,
            Arc::clone(&claude_calls),
            "printf 'CLAUDE TERMINAL\\r\\n'; sleep 30",
        )),
    ]);
    let server = start_server(app).await;

    let mut socket = connect_terminal_for_backend(
        &server,
        BackendKind::Claude,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("attach backend qualified terminal");
    let screen = terminal_json_matching(&mut socket, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "CLAUDE TERMINAL"))
    })
    .await;
    assert!(
        !screen["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line == "CODEX TERMINAL"))
    );
    assert_eq!(claude_calls.attach.load(Ordering::SeqCst), 1);
    assert_eq!(codex_calls.attach.load(Ordering::SeqCst), 0);
    socket.close(None).await.expect("close terminal");
}

#[tokio::test]
async fn terminal_disconnect_reaps_child_and_reconnect_creates_fresh_pty() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("terminal.pid");
    let script = format!(
        "printf '%s' $$ > '{}'; printf 'CONNECTED\\r\\n'; sleep 30",
        pid_file.display()
    );
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(script, Arc::clone(&calls))).await;
    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    terminal_json_matching(&mut first, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "CONNECTED"))
    })
    .await;
    let first_pid = std::fs::read_to_string(&pid_file)
        .expect("terminal pid file")
        .parse::<u32>()
        .expect("terminal pid");

    first.close(None).await.expect("close first terminal");
    drop(first);
    wait_for_pid_exit(first_pid).await;

    let mut second = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("reconnect fresh terminal");
    terminal_json_matching(&mut second, |value| {
        value["type"] == "screen"
            && value["lines"]
                .as_array()
                .is_some_and(|lines| lines.iter().any(|line| line == "CONNECTED"))
    })
    .await;
    let second_pid = std::fs::read_to_string(&pid_file)
        .expect("fresh terminal pid file")
        .parse::<u32>()
        .expect("fresh terminal pid");
    assert_ne!(second_pid, first_pid);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 2);
    second.close(None).await.expect("close second terminal");
    wait_for_pid_exit(second_pid).await;
}

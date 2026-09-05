use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_viewer_core::{
    ArchiveMode, AttachRefusal, Backend, BackendKind, Capabilities, Error, Session, SessionOrigin,
    SpawnResult, Status, TailEvent,
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
            archive: Some(ArchiveMode::Reversible),
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

    fn archive(&mut self, _session: &Session) -> agent_viewer_core::Result<()> {
        self.calls.hide.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn unarchive(&mut self, _session: &Session) -> agent_viewer_core::Result<()> {
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

/// The next JSON control frame. Binary frames are PTY bytes and are skipped here.
async fn next_terminal_text(socket: &mut ClientSocket) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.expect("terminal message") {
                Ok(Message::Text(text)) => {
                    return serde_json::from_str(text.as_str()).expect("terminal JSON frame");
                }
                Ok(Message::Binary(_)) => {}
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
    .expect("terminal control frame timeout")
}

/// The next raw PTY frame.
async fn next_terminal_binary(socket: &mut ClientSocket) -> Vec<u8> {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.expect("terminal message") {
                Ok(Message::Binary(bytes)) => return bytes.to_vec(),
                Ok(Message::Text(text)) => panic!("expected PTY bytes, got control frame {text}"),
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
    .expect("terminal PTY frame timeout")
}

/// Accumulate raw PTY frames onto `seen` until `needle` shows up in the stream. Seeding with
/// the redraw matters: whether a marker lands in the repaint or in the live stream depends on
/// how far the child got before the socket attached, and either side is correct. What is NOT
/// correct is landing on both, so this helper only ever answers "did it arrive"; exactly-once
/// is asserted by `terminal_prints_each_chunk_exactly_once_across_redraw_and_live` and
/// `terminal_never_duplicates_output_when_attaching_mid_flood`.
async fn terminal_bytes_containing(
    socket: &mut ClientSocket,
    seed: Vec<u8>,
    needle: &[u8],
) -> Vec<u8> {
    let mut seen = seed;
    timeout(Duration::from_secs(10), async {
        while !contains(&seen, needle) {
            match socket.next().await.expect("terminal message") {
                Ok(Message::Binary(bytes)) => seen.extend_from_slice(&bytes),
                Ok(Message::Text(_)) => {}
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
    .unwrap_or_else(|_| {
        panic!(
            "expected {:?} in the PTY stream, got {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&seen)
        )
    });
    seen
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The hello frame plus the single redraw frame that opens every (re)connection.
async fn terminal_handshake(socket: &mut ClientSocket) -> (Value, Vec<u8>) {
    let hello = next_terminal_text(socket).await;
    assert_eq!(hello["type"], "hello", "the first frame must be hello");
    let redraw = next_terminal_binary(socket).await;
    (hello, redraw)
}

async fn expect_terminal_close(socket: &mut ClientSocket) {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("terminal close failed: {error}"),
            }
        }
    })
    .await
    .expect("terminal close timeout");
}

/// The close frame itself, so a test can assert the code and reason the client acts on. PTY
/// frames already queued ahead of the close are drained on the way.
async fn expect_terminal_close_frame(
    socket: &mut ClientSocket,
) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => return frame,
                Some(Ok(Message::Close(None))) => panic!("expected a close frame with a code"),
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("terminal close failed: {error}"),
                None => panic!("the socket ended without a close frame"),
            }
        }
    })
    .await
    .expect("terminal close frame timeout")
}

/// Every `M<six digits>` marker in a byte stream, in order.
fn markers(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (index, window) in bytes.windows(7).enumerate() {
        if window[0] == b'M' && window[1..].iter().all(u8::is_ascii_digit) {
            // Skip a match that is really the tail of a longer digit run.
            if index + 7 < bytes.len() && bytes[index + 7].is_ascii_digit() {
                continue;
            }
            found.push(String::from_utf8_lossy(window).into_owned());
        }
    }
    found
}

fn duplicated(markers: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut twice = Vec::new();
    for marker in markers {
        if !seen.insert(marker.clone()) {
            twice.push(marker.clone());
        }
    }
    twice
}

async fn wait_for_pid_exit(pid: u32) {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while path.exists() && tokio::time::Instant::now() < deadline {
        sleep(Duration::from_millis(25)).await;
    }
    assert!(!path.exists(), "terminal child {pid} must be reaped");
}

fn pid_lines(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

async fn wait_for_pids(path: &Path, expected: usize) -> Vec<u32> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let pids = pid_lines(path);
        if pids.len() == expected {
            return pids;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected} terminal pid lines at {}, saw {}",
            path.display(),
            pids.len()
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
            "archive": null,
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
            "archive": "Reversible",
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

/// The whole wire protocol in order: one hello, one redraw carrying the current screen as
/// terminal escapes, then the child's own bytes as they are produced.
#[tokio::test]
async fn terminal_sends_hello_then_one_redraw_then_live_pty_bytes() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(
        "printf 'ATTACHED\\r\\n'; sleep 0.6; printf 'LIVE-MARKER\\r\\n'; sleep 30",
        Arc::clone(&calls),
    ))
    .await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");

    let (hello, redraw) = terminal_handshake(&mut socket).await;
    assert_eq!(hello["cols"], 80);
    assert_eq!(hello["rows"], 24);
    assert_eq!(hello["running"], true);
    assert!(
        !redraw.is_empty() && redraw.contains(&0x1b),
        "the redraw must be a real vt100 repaint, got {redraw:?}"
    );
    assert!(
        !contains(&redraw, b"LIVE-MARKER"),
        "the redraw is the screen at connect time, not the future"
    );

    let live = terminal_bytes_containing(&mut socket, Vec::new(), b"LIVE-MARKER").await;
    assert!(
        contains(&live, b"\r\n"),
        "PTY output must arrive verbatim, control bytes included: {live:?}"
    );
    assert_eq!(calls.attach.load(Ordering::SeqCst), 1);
    socket.close(None).await.expect("close terminal");
}

/// The redraw and the live stream must partition the child's output, not overlap it: every
/// chunk lands in exactly one of them. The PTY only exists once somebody has attached, so
/// "printed before this client connected" is a reconnect — which is the case the phone lives
/// in anyway, every time it comes back from the background.
#[tokio::test]
async fn terminal_prints_each_chunk_exactly_once_across_redraw_and_live() {
    let fixture = project_fixture();
    let gate = fixture.root.join("print-marker-two");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends_and_grace(
        vec![Box::new(FakeBackend::attachable(
            attachable_session,
            Arc::clone(&calls),
            format!(
                "printf 'MARKER-ONE\\r\\n'; while [ ! -f '{}' ]; do sleep 0.05; done; printf 'MARKER-TWO\\r\\n'; sleep 30",
                gate.display()
            ),
        ))],
        Duration::from_secs(30),
    );
    let server = start_server(app).await;

    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    let (_, redraw) = terminal_handshake(&mut first).await;
    terminal_bytes_containing(&mut first, redraw, b"MARKER-ONE").await;
    first.close(None).await.expect("close first terminal");
    drop(first);
    sleep(Duration::from_millis(200)).await;

    let mut second = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("reconnect within the grace window");
    let (_, redraw) = terminal_handshake(&mut second).await;
    // Only now does the child print the second marker, so it cannot be in the snapshot.
    std::fs::write(&gate, b"go").expect("open the marker gate");
    let stream = terminal_bytes_containing(&mut second, redraw, b"MARKER-TWO").await;

    assert_eq!(
        count(&stream, b"MARKER-ONE"),
        1,
        "the marker printed before this client connected must appear exactly once across \
         redraw + live"
    );
    assert_eq!(
        count(&stream, b"MARKER-TWO"),
        1,
        "the marker printed after this client connected must appear exactly once"
    );
    assert_eq!(calls.attach.load(Ordering::SeqCst), 1);
    second.close(None).await.expect("close terminal");
}

/// The same rule under the timing that actually breaks it: a socket attaching while the child
/// is mid-flood. Anything a forwarding thread were holding at that instant would be inside the
/// redraw AND forwarded again as live bytes, so every numbered line must be unique.
#[tokio::test]
async fn terminal_never_duplicates_output_when_attaching_mid_flood() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(
        "i=0; while [ $i -lt 40000 ]; do i=$((i+1)); printf 'M%06d\\r\\n' \"$i\"; done; printf 'FLOOD-DONE\\r\\n'; sleep 30",
        Arc::clone(&calls),
    ))
    .await;

    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    let (_, redraw) = terminal_handshake(&mut first).await;
    terminal_bytes_containing(&mut first, redraw, b"M000005").await;

    // Take over mid-flood: the second socket's snapshot is taken while chunks are in motion.
    let mut second = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("mid-flood takeover");
    let (_, redraw) = terminal_handshake(&mut second).await;
    let stream = terminal_bytes_containing(&mut second, redraw, b"FLOOD-DONE").await;

    let seen = markers(&stream);
    assert!(
        seen.len() > 24,
        "the takeover must see a real slice of the flood, saw {}",
        seen.len()
    );
    let twice = duplicated(&seen);
    assert!(
        twice.is_empty(),
        "no chunk may be printed twice across redraw + live, duplicated {twice:?}"
    );
    drop(first);
    second.close(None).await.expect("close terminal");
}

/// Client binary frames are keystrokes for the PTY and must reach the child byte for byte,
/// UTF-8 and control bytes alike.
#[tokio::test]
async fn terminal_forwards_client_binary_frames_to_the_attach_child() {
    let calls = Arc::new(FakeCalls::default());
    let script = "stty raw -echo; printf 'INPUT READY\\r\\n'; python3 -c 'import sys; data=sys.stdin.buffer.read(4); sys.stdout.write(data.hex()+\"\\r\\n\"); sys.stdout.flush()'; sleep 30";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"INPUT READY").await;

    socket
        .send(Message::Binary(b"h\xc3\xa9\x03".to_vec().into()))
        .await
        .expect("send terminal keystrokes");

    terminal_bytes_containing(&mut socket, Vec::new(), b"68c3a903").await;
    socket.close(None).await.expect("close terminal");
}

/// A resize must reach the child as a real SIGWINCH on a real winsize, not just change a
/// server-side number.
#[tokio::test]
async fn terminal_resize_reaches_the_attach_child() {
    let calls = Arc::new(FakeCalls::default());
    let script = "trap 'stty size' WINCH; printf 'RESIZE READY\\r\\n'; while :; do sleep 1; done";
    let server = start_server(attachable_app(script, calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"RESIZE READY").await;

    socket
        .send(Message::Text(
            json!({ "type": "resize", "rows": 14, "cols": 37 })
                .to_string()
                .into(),
        ))
        .await
        .expect("send terminal resize");
    terminal_bytes_containing(&mut socket, Vec::new(), b"14 37").await;

    socket
        .send(Message::Text(
            json!({ "type": "resize", "rows": 0, "cols": 10_000 })
                .to_string()
                .into(),
        ))
        .await
        .expect("send invalid resize");
    let error = next_terminal_text(&mut socket).await;
    assert_eq!(error["type"], "error");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    expect_terminal_close(&mut socket).await;
}

/// A phone's `WebSocket.close` is asynchronous and a phone that drops off the tailnet never
/// closes at all, so a reconnect routinely arrives while the previous socket still counts as
/// connected. That is a takeover, not a 409: the new socket gets the same child and the same
/// screen, the old one is closed with 4000 "replaced", and nothing the loser sends afterwards
/// can reach the child.
#[tokio::test]
async fn terminal_second_connection_takes_over_from_the_first() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("takeover.pids");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::attachable(
        attachable_session,
        Arc::clone(&calls),
        format!(
            "printf '%s\\n' $$ >> '{}'; stty raw -echo; printf 'FIRST-SCREEN\\r\\n'; cat",
            pid_file.display()
        ),
    ))]);
    let server = start_server(app).await;

    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    let (_, redraw) = terminal_handshake(&mut first).await;
    terminal_bytes_containing(&mut first, redraw, b"FIRST-SCREEN").await;
    let child = wait_for_pids(&pid_file, 1).await[0];

    let mut second = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("a second connection must take over, not be refused");
    let (hello, redraw) = terminal_handshake(&mut second).await;
    assert_eq!(hello["running"], true);
    assert!(
        contains(&redraw, b"FIRST-SCREEN"),
        "the taking-over socket must get the redraw of the same child: {:?}",
        String::from_utf8_lossy(&redraw)
    );
    assert_eq!(
        pid_lines(&pid_file),
        vec![child],
        "a takeover must not start a second child"
    );
    assert_eq!(
        calls.attach.load(Ordering::SeqCst),
        1,
        "a takeover must not ask the backend to attach again"
    );

    // The replaced socket still holds a lease, so prove its input is ignored rather than
    // merely late: whatever it sends must never come back out of the child.
    let _ = first
        .send(Message::Binary(b"GHOST-INPUT\r\n".to_vec().into()))
        .await;
    second
        .send(Message::Binary(b"TAKEOVER-INPUT\r\n".to_vec().into()))
        .await
        .expect("send input from the taking-over socket");
    let live = terminal_bytes_containing(&mut second, Vec::new(), b"TAKEOVER-INPUT").await;
    assert!(
        !contains(&live, b"GHOST-INPUT"),
        "a replaced socket must not be able to type into the child: {:?}",
        String::from_utf8_lossy(&live)
    );

    let close = expect_terminal_close_frame(&mut first).await;
    assert_eq!(
        u16::from(close.code),
        4000,
        "the replaced socket must be closed with the final code the phone never retries"
    );
    assert_eq!(close.reason.as_str(), "replaced");

    second
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    expect_terminal_close(&mut second).await;
    wait_for_pid_exit(child).await;
}

/// A client that stops reading must not be allowed to grow the owner's queue without bound:
/// past the cap it is disconnected with 1013 and reconnects into a fresh redraw.
#[tokio::test]
async fn terminal_disconnects_a_client_that_stops_reading() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app(
        "yes 'FLOOD-LINE' | head -c 16000000; printf 'FLOOD-END\\r\\n'; sleep 30",
        calls,
    ))
    .await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let hello = next_terminal_text(&mut socket).await;
    assert_eq!(hello["type"], "hello");

    // Stop reading entirely while the child floods.
    sleep(Duration::from_secs(2)).await;

    let mut received = 0_usize;
    let close = timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => return frame,
                Some(Ok(Message::Binary(bytes))) => {
                    assert!(
                        !contains(&bytes, b"FLOOD-END"),
                        "a slow consumer must be cut off before the flood finishes"
                    );
                    received += bytes.len();
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("expected a slow-consumer close, got {error}"),
                None => panic!("the socket ended without a slow-consumer close frame"),
            }
        }
    })
    .await
    .expect("slow consumer close timeout");

    assert_eq!(u16::from(close.code), 1013);
    assert_eq!(close.reason.as_str(), "slow consumer");
    assert!(
        received < 8_000_000,
        "the owner must bound what it buffers for a stalled client, queued {received} bytes"
    );
}

/// A socket that answers nothing at all never sends a FIN, so only a server-side ping can end
/// it. Once it is closed the ordinary grace window runs and the child is killed.
#[tokio::test]
async fn terminal_closes_a_socket_that_stops_answering() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("liveness.pids");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends_and_terminal_timing(
        vec![Box::new(FakeBackend::attachable(
            attachable_session,
            Arc::clone(&calls),
            format!(
                "printf '%s\\n' $$ >> '{}'; printf 'SILENT\\r\\n'; while :; do sleep 1; done",
                pid_file.display()
            ),
        ))],
        Duration::from_millis(300),
        Duration::from_millis(100),
        Duration::from_millis(400),
    );
    let server = start_server(app).await;

    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"SILENT").await;
    let child = wait_for_pids(&pid_file, 1).await[0];

    // Never poll the socket again: no pong, no close, nothing. tokio-tungstenite only answers
    // pings while it is being read, so this is exactly a phone that fell off the tailnet.
    wait_for_pid_exit(child).await;
    drop(socket);
}

#[tokio::test]
async fn terminal_sends_an_exit_frame_when_the_attach_child_ends() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app("printf 'DONE\\r\\n'", calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let hello = next_terminal_text(&mut socket).await;
    assert_eq!(hello["type"], "hello");

    let exit = timeout(Duration::from_secs(5), async {
        loop {
            let frame = next_terminal_text(&mut socket).await;
            if frame["type"] == "exit" {
                return frame;
            }
        }
    })
    .await
    .expect("exit frame timeout");
    assert!(exit.get("status").is_some(), "exit frame carries a status");
    expect_terminal_close(&mut socket).await;
}

/// The phone goes to sleep, comes back, and finds the SAME child: no second attach, no second
/// process, and the redraw carries what the screen already held.
#[tokio::test]
async fn terminal_reconnect_within_the_grace_window_adopts_the_same_pty() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("grace.pids");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends_and_grace(
        vec![Box::new(FakeBackend::attachable(
            attachable_session,
            Arc::clone(&calls),
            format!(
                "printf '%s\\n' $$ >> '{}'; printf 'STICKY-SESSION\\r\\n'; while :; do sleep 1; done",
                pid_file.display()
            ),
        ))],
        Duration::from_secs(30),
    );
    let server = start_server(app).await;

    let mut first = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("first terminal attach");
    let (_, redraw) = terminal_handshake(&mut first).await;
    terminal_bytes_containing(&mut first, redraw, b"STICKY-SESSION").await;
    let pids = wait_for_pids(&pid_file, 1).await;
    let child = pids[0];

    first.close(None).await.expect("close first terminal");
    drop(first);
    sleep(Duration::from_millis(300)).await;
    assert!(
        PathBuf::from(format!("/proc/{child}")).exists(),
        "the child must outlive its socket for the grace window"
    );

    let mut second = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("reconnect within the grace window");
    let (hello, redraw) = terminal_handshake(&mut second).await;
    assert_eq!(hello["running"], true);
    assert!(
        contains(&redraw, b"STICKY-SESSION"),
        "the redraw must repaint the screen the same child already had: {:?}",
        String::from_utf8_lossy(&redraw)
    );
    assert_eq!(
        pid_lines(&pid_file),
        vec![child],
        "a reconnect inside the grace window must not start a second child"
    );
    assert_eq!(
        calls.attach.load(Ordering::SeqCst),
        1,
        "a reconnect inside the grace window must not ask the backend to attach again"
    );

    second
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    expect_terminal_close(&mut second).await;
    wait_for_pid_exit(child).await;
}

/// The other half of the same rule: nobody comes back, so the child is killed.
#[tokio::test]
async fn terminal_grace_expiry_kills_the_pty() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("expiry.pids");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends_and_grace(
        vec![Box::new(FakeBackend::attachable(
            attachable_session,
            Arc::clone(&calls),
            format!(
                "printf '%s\\n' $$ >> '{}'; printf 'EXPIRING\\r\\n'; while :; do sleep 1; done",
                pid_file.display()
            ),
        ))],
        Duration::from_millis(700),
    );
    let server = start_server(app).await;

    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"EXPIRING").await;
    let child = wait_for_pids(&pid_file, 1).await[0];

    socket.close(None).await.expect("close terminal");
    drop(socket);
    sleep(Duration::from_millis(200)).await;
    assert!(
        PathBuf::from(format!("/proc/{child}")).exists(),
        "the kill must wait for the grace window, not fire on disconnect"
    );

    wait_for_pid_exit(child).await;

    let mut fresh = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach after the grace window closed");
    terminal_handshake(&mut fresh).await;
    let pids = wait_for_pids(&pid_file, 2).await;
    assert_ne!(
        pids[1], child,
        "after expiry a connect must start a new child"
    );
    assert_eq!(calls.attach.load(Ordering::SeqCst), 2);
    fresh
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    expect_terminal_close(&mut fresh).await;
    wait_for_pid_exit(pids[1]).await;
}

/// The same rule against a child that never goes quiet. The owner's wait is only ever a bound
/// on how long it sits idle, so the grace deadline has to be tested on every pass: a child that
/// keeps the event loop busy must still be killed on time, not outlive its window forever.
#[tokio::test]
async fn terminal_grace_expiry_kills_a_flooding_pty() {
    let fixture = project_fixture();
    let pid_file = fixture.root.join("flood-expiry.pids");
    let mut attachable_session = session(fixture.cwd.clone());
    attachable_session.daemon_hosted = true;
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends_and_grace(
        vec![Box::new(FakeBackend::attachable(
            attachable_session,
            Arc::clone(&calls),
            format!(
                "printf '%s\\n' $$ >> '{}'; exec yes 'FLOOD-EXPIRING'",
                pid_file.display()
            ),
        ))],
        Duration::from_millis(700),
    );
    let server = start_server(app).await;

    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"FLOOD-EXPIRING").await;
    let child = wait_for_pids(&pid_file, 1).await[0];

    socket.close(None).await.expect("close terminal");
    drop(socket);

    let closed_at = tokio::time::Instant::now();
    wait_for_pid_exit(child).await;
    let killed_after = closed_at.elapsed();
    assert!(
        killed_after < Duration::from_secs(3),
        "a flooding child must be killed on its own window, not whenever its output finally \
         goes quiet; it survived {killed_after:?} of a 700ms window"
    );
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
    let (_, redraw) = terminal_handshake(&mut socket).await;
    terminal_bytes_containing(&mut socket, redraw, b"EXPLICIT CLOSE").await;
    let pid = std::fs::read_to_string(&pid_file)
        .expect("terminal pid file")
        .parse::<u32>()
        .expect("terminal pid");

    socket
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    expect_terminal_close(&mut socket).await;
    wait_for_pid_exit(pid).await;
}

#[tokio::test]
async fn terminal_capacity_is_released_when_a_terminal_is_closed() {
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
        .expect("attach terminal owner");
        let (_, redraw) = terminal_handshake(&mut socket).await;
        terminal_bytes_containing(&mut socket, redraw, b"CAPACITY").await;
        sockets.push(socket);
    }
    assert_eq!(calls.attach.load(Ordering::SeqCst), 4);
    let first_four_pids = wait_for_pids(&pid_file, 4).await;

    let full = connect_terminal_session(
        &server,
        "session-5",
        BackendKind::Codex,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect_err("four attached terminals must fill capacity");
    assert_eq!(websocket_status(full), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(calls.attach.load(Ordering::SeqCst), 4);

    let mut closed = sockets.remove(0);
    closed
        .send(Message::Text(json!({ "type": "close" }).to_string().into()))
        .await
        .expect("explicit terminal close");
    expect_terminal_close(&mut closed).await;
    wait_for_pid_exit(first_four_pids[0]).await;

    let mut fifth = connect_terminal_session(
        &server,
        "session-5",
        BackendKind::Codex,
        Some(LOGIN),
        Some(ANDROID_ORIGIN),
    )
    .await
    .expect("closing a terminal must release capacity");
    let (_, redraw) = terminal_handshake(&mut fifth).await;
    terminal_bytes_containing(&mut fifth, redraw, b"CAPACITY").await;
    assert_eq!(calls.attach.load(Ordering::SeqCst), 5);
    let all_pids = wait_for_pids(&pid_file, 5).await;

    for (mut socket, pid) in sockets
        .into_iter()
        .chain(std::iter::once(fifth))
        .zip(all_pids.into_iter().skip(1))
    {
        socket
            .send(Message::Text(json!({ "type": "close" }).to_string().into()))
            .await
            .expect("explicit terminal close");
        expect_terminal_close(&mut socket).await;
        wait_for_pid_exit(pid).await;
    }
}

#[tokio::test]
async fn terminal_transport_rejects_oversized_websocket_messages() {
    let calls = Arc::new(FakeCalls::default());
    let server = start_server(attachable_app("printf 'SIZE LIMIT\\r\\n'; sleep 30", calls)).await;
    let mut socket = connect_terminal(&server, Some(LOGIN), Some(ANDROID_ORIGIN))
        .await
        .expect("attach terminal");
    terminal_handshake(&mut socket).await;

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
    let (_, redraw) = terminal_handshake(&mut socket).await;
    let stream = terminal_bytes_containing(&mut socket, redraw, b"CLAUDE TERMINAL").await;
    assert!(!contains(&stream, b"CODEX TERMINAL"));
    assert_eq!(claude_calls.attach.load(Ordering::SeqCst), 1);
    assert_eq!(codex_calls.attach.load(Ordering::SeqCst), 0);
    socket.close(None).await.expect("close terminal");
}

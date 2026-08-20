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
use serde_json::{Value, json};
use tower::ServiceExt as _;

const LOGIN: &str = "bcconn2112@gmail.com";

#[derive(Default)]
struct FakeCalls {
    list: AtomicUsize,
    spawn: AtomicUsize,
    stop: AtomicUsize,
    hide: AtomicUsize,
    unhide: AtomicUsize,
    rename: AtomicUsize,
}

struct FakeBackend {
    sessions: Vec<Session>,
    calls: Arc<FakeCalls>,
}

impl FakeBackend {
    fn new(session: Session, calls: Arc<FakeCalls>) -> Self {
        Self {
            sessions: vec![session],
            calls,
        }
    }
}

impl Backend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            attach: false,
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
        Err(AttachRefusal::new("fake backend does not attach"))
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

fn project_fixture() -> (PathBuf, PathBuf) {
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
    (root, cwd)
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
    let (project_root, cwd) = project_fixture();
    let calls = Arc::new(FakeCalls::default());
    let app = agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::new(
        session(cwd),
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

    std::fs::remove_dir_all(&project_root).expect("remove project fixture");
}

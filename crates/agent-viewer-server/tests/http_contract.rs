use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_viewer_core::{
    AttachRefusal, Backend, BackendKind, Capabilities, Error, Session, SessionOrigin, SpawnResult,
    Status, TailEvent,
};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const LOGIN: &str = "bcconn2112@gmail.com";

struct FakeBackend {
    sessions: Vec<Session>,
}

impl FakeBackend {
    fn new(session: Session) -> Self {
        Self {
            sessions: vec![session],
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
        Ok(self.sessions.clone())
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
        Err(Error::Unsupported("fake"))
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
    let app =
        agent_viewer_server::router_with_backends(vec![Box::new(FakeBackend::new(session(cwd)))]);

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
        .oneshot(request(Method::POST, "/sessions/session-1/stop", true))
        .await
        .expect("stop response");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);

    std::fs::remove_dir_all(&project_root).expect("remove project fixture");
}

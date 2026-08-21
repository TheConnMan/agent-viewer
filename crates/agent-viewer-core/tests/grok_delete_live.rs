use agent_viewer_core::{Backend, GrokBackend, GrokLifecycle, Status, TailEvent};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is required")).join(".grok")
        })
}

#[test]
#[ignore = "requires explicit authorization to stop and remove one exact live Grok session"]
fn live_stop_then_remove_exact_opt_in() {
    let session_id = std::env::var("GROK_LIVE_DELETE_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .expect("set GROK_LIVE_DELETE_SESSION_ID to the exact disposable session identity");
    let expected_cwd = std::env::var_os("GROK_LIVE_DELETE_EXPECTED_CWD")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .expect("set GROK_LIVE_DELETE_EXPECTED_CWD to the exact durable session cwd");
    let lifecycle = GrokLifecycle::new("grok", grok_home());

    let matching = lifecycle
        .list()
        .expect("list live Grok sessions before exact stop and removal")
        .into_iter()
        .filter(|session| session.id == session_id)
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "exactly one live Grok session must match the authorized identity"
    );
    assert_eq!(
        matching[0].cwd, expected_cwd,
        "the authorized identity must resolve to the exact expected cwd"
    );

    lifecycle
        .cancel(&session_id)
        .expect("stop the exact authorized live Grok session");
    lifecycle
        .delete(&session_id)
        .expect("remove the exact authorized live Grok session");
    let remaining = lifecycle
        .list()
        .expect("list live Grok sessions after exact stop and removal");
    assert!(
        !remaining.iter().any(|session| session.id == session_id),
        "the exact authorized live Grok session remained after removal"
    );
}

struct DisposableSessionCleanup<'a> {
    lifecycle: &'a GrokLifecycle,
    session_id: Option<String>,
}

impl<'a> DisposableSessionCleanup<'a> {
    fn new(lifecycle: &'a GrokLifecycle) -> DisposableSessionCleanup<'a> {
        DisposableSessionCleanup {
            lifecycle,
            session_id: None,
        }
    }

    fn record(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    fn disarm(&mut self) {
        self.session_id = None;
    }
}

impl Drop for DisposableSessionCleanup<'_> {
    fn drop(&mut self) {
        if let Some(session_id) = self.session_id.as_deref() {
            let _ = self.lifecycle.cancel(session_id);
            let _ = self.lifecycle.delete(session_id);
        }
    }
}

#[test]
#[ignore = "requires the official authenticated Grok runtime and consumes live usage"]
fn live_detached_spawn_reaches_done_and_persists_response() {
    let home = grok_home();
    let lifecycle = GrokLifecycle::new("grok", &home);
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository = manifest_dir
        .parent()
        .and_then(|crates| crates.parent())
        .expect("Agent Viewer workspace root");
    let project = tempfile::Builder::new()
        .prefix(".grok-detached-live-")
        .tempdir_in(repository)
        .expect("throwaway Grok project inside the Agent Viewer repository");
    let mut cleanup = DisposableSessionCleanup::new(&lifecycle);

    let spawned = lifecycle
        .spawn(
            project.path(),
            "Do not use tools. Reply with exactly DETACHED_GROK_LIVE_OK and nothing else.",
            None,
        )
        .expect("authenticated detached Grok spawn must be accepted");
    let session_id = spawned
        .session_id
        .expect("accepted detached Grok spawn must return its exact session identity");
    assert!(
        !session_id.trim().is_empty(),
        "accepted detached Grok spawn returned an empty identity"
    );
    assert_eq!(
        spawned.pid, None,
        "detached Grok spawn must not claim the shared leader process"
    );
    cleanup.record(&session_id);

    let accepted = lifecycle
        .list()
        .expect("list after detached Grok acceptance")
        .into_iter()
        .find(|session| session.id == session_id)
        .expect("accepted detached Grok session must be independently listable");
    assert!(
        accepted.daemon_hosted,
        "accepted detached Grok session must remain hosted by the shared leader"
    );

    let backend = GrokBackend::new();
    let started = Instant::now();
    let mut observed_statuses = Vec::new();
    let mut latest = None;
    let completed = loop {
        let matching = lifecycle
            .list()
            .expect("poll detached Grok session through its public lifecycle")
            .into_iter()
            .find(|session| session.id == session_id);
        if let Some(session) = matching {
            if !observed_statuses.contains(&session.status) {
                observed_statuses.push(session.status.clone());
            }
            latest = Some(session.clone());
            if session.status == Status::Done {
                break session;
            }
            assert_ne!(
                session.status,
                Status::Error,
                "detached Grok session reached an error instead of completing"
            );
        }
        let within_timeout = started.elapsed() < Duration::from_secs(120);
        let final_tail_count = if within_timeout {
            None
        } else {
            latest
                .as_ref()
                .and_then(|session| backend.tail(session, 20).ok())
                .map(|events| events.len())
        };
        assert!(
            within_timeout,
            "detached Grok session did not reach terminal completion; observed statuses: {observed_statuses:?}; final public tail event count: {final_tail_count:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    println!("grok detached live: observed statuses {observed_statuses:?}");

    let tail = backend
        .tail(&completed, 20)
        .expect("read completed detached Grok transcript through the public backend");
    assert!(
        tail.iter().any(
            |event| matches!(event, TailEvent::Agent(text) if text.trim() == "DETACHED_GROK_LIVE_OK")
        ),
        "completed detached Grok transcript did not persist the expected response"
    );

    lifecycle
        .delete(&session_id)
        .expect("delete exact disposable detached Grok session");
    let remaining = lifecycle
        .list()
        .expect("list after deleting disposable detached Grok session");
    assert!(
        !remaining.iter().any(|session| session.id == session_id),
        "disposable detached Grok session remained after exact deletion"
    );
    cleanup.disarm();
}

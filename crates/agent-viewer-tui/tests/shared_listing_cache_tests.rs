use std::path::PathBuf;

use agent_viewer_core::{
    AttachRefusal, Backend, BackendKind, Capabilities, Error, ListingCacheScope, Session,
    SessionOrigin, SpawnResult, Status, ViewerDb, spawn::now_ms, state::LISTING_CACHE_FRESHNESS_MS,
};
use agent_viewer_tui::shared_listing::{
    RefreshCursor, RefreshOutcome, TargetRequest, TargetResolution, authoritative_target,
    refresh_backend,
};
use tempfile::TempDir;

enum Listing {
    Sessions(Vec<Session>),
    Error(String),
}

struct CountedBackend {
    kind: BackendKind,
    scope: ListingCacheScope,
    listing: Listing,
    calls: usize,
    delay: std::time::Duration,
}

impl CountedBackend {
    fn sessions(kind: BackendKind, scope: &ListingCacheScope, sessions: Vec<Session>) -> Self {
        Self {
            kind,
            scope: scope.clone(),
            listing: Listing::Sessions(sessions),
            calls: 0,
            delay: std::time::Duration::ZERO,
        }
    }

    fn error(kind: BackendKind, scope: &ListingCacheScope, message: &str) -> Self {
        Self {
            kind,
            scope: scope.clone(),
            listing: Listing::Error(message.to_string()),
            calls: 0,
            delay: std::time::Duration::ZERO,
        }
    }
}

impl Backend for CountedBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    fn listing_scope(&self) -> Option<ListingCacheScope> {
        Some(self.scope.clone())
    }

    fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
        self.calls += 1;
        std::thread::sleep(self.delay);
        match &self.listing {
            Listing::Sessions(sessions) => Ok(sessions.clone()),
            Listing::Error(message) => Err(Error::Command(message.clone())),
        }
    }

    fn spawn(
        &self,
        _dir: &std::path::Path,
        _task: &str,
        _model: Option<&str>,
        _effort: Option<&str>,
    ) -> agent_viewer_core::Result<SpawnResult> {
        unreachable!("spawn is not exercised by listing tests")
    }

    fn attach_command(&self, _session: &Session) -> Result<std::process::Command, AttachRefusal> {
        unreachable!("attach is not exercised by listing tests")
    }
}

/// A backend whose listing races a mutation: another viewer invalidates the scope while
/// `list()` is running, so what it returns is a pre-mutation snapshot and its publish fences.
struct FencedBackend {
    kind: BackendKind,
    scope: ListingCacheScope,
    sessions: Vec<Session>,
    database: PathBuf,
    fence_next: bool,
    calls: usize,
}

impl Backend for FencedBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    fn listing_scope(&self) -> Option<ListingCacheScope> {
        Some(self.scope.clone())
    }

    fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
        self.calls += 1;
        if self.fence_next {
            let other = ViewerDb::open(&self.database).expect("mutating viewer database");
            other
                .invalidate_listing_scope(Some(&self.scope))
                .expect("invalidate the scope mid listing");
        }
        Ok(self.sessions.clone())
    }

    fn spawn(
        &self,
        _dir: &std::path::Path,
        _task: &str,
        _model: Option<&str>,
        _effort: Option<&str>,
    ) -> agent_viewer_core::Result<SpawnResult> {
        unreachable!("spawn is not exercised by listing tests")
    }

    fn attach_command(&self, _session: &Session) -> Result<std::process::Command, AttachRefusal> {
        unreachable!("attach is not exercised by listing tests")
    }
}

fn open_viewers() -> (TempDir, ViewerDb, ViewerDb) {
    let directory = tempfile::tempdir().expect("temporary viewer database directory");
    let path = directory.path().join("viewer.sqlite");
    let first = ViewerDb::open(&path).expect("first viewer database");
    let second = ViewerDb::open(&path).expect("second viewer database");
    (directory, first, second)
}

fn scope(backend: BackendKind) -> ListingCacheScope {
    ListingCacheScope::new(backend, "test instance").expect("valid listing scope")
}

fn session(backend: BackendKind, id: &str, pid: Option<u32>, daemon_hosted: bool) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Background,
        title: format!("session {id}"),
        cwd: PathBuf::from("/work/agent viewer"),
        git_branch: None,
        status: Status::Working,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted,
    }
}

#[test]
fn fresh_shared_snapshot_skips_the_authoritative_source_across_viewers() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let expected = session(BackendKind::Codex, "thread_123", Some(101), false);
    let mut publisher =
        CountedBackend::sessions(BackendKind::Codex, &scope, vec![expected.clone()]);
    let mut follower = CountedBackend::sessions(BackendKind::Codex, &scope, Vec::new());
    let mut publisher_cursor = RefreshCursor::default();
    let mut follower_cursor = RefreshCursor::default();

    let initial = refresh_backend(
        Some(&first),
        &mut publisher,
        &[],
        &mut publisher_cursor,
        1_000,
    );
    let shared = refresh_backend(
        Some(&second),
        &mut follower,
        &[],
        &mut follower_cursor,
        2_999,
    );

    assert!(matches!(initial, RefreshOutcome::Authoritative { .. }));
    assert!(matches!(shared, RefreshOutcome::Shared { sessions } if sessions == vec![expected]));
    assert_eq!(publisher.calls, 1);
    assert_eq!(follower.calls, 0);
}

#[test]
fn unchanged_shared_generation_skips_decode_and_authoritative_source() {
    let (_directory, publisher_db, follower_db) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let expected = session(BackendKind::Codex, "thread_123", Some(101), false);
    let mut publisher =
        CountedBackend::sessions(BackendKind::Codex, &scope, vec![expected.clone()]);
    let mut follower = CountedBackend::sessions(BackendKind::Codex, &scope, Vec::new());
    let mut publisher_cursor = RefreshCursor::default();
    let mut follower_cursor = RefreshCursor::default();

    refresh_backend(
        Some(&publisher_db),
        &mut publisher,
        &[],
        &mut publisher_cursor,
        1_000,
    );
    let loaded = refresh_backend(
        Some(&follower_db),
        &mut follower,
        &[],
        &mut follower_cursor,
        2_000,
    );
    let unchanged = refresh_backend(
        Some(&follower_db),
        &mut follower,
        std::slice::from_ref(&expected),
        &mut follower_cursor,
        2_001,
    );

    assert!(matches!(loaded, RefreshOutcome::Shared { sessions } if sessions == vec![expected]));
    assert!(matches!(unchanged, RefreshOutcome::Unchanged));
    assert_eq!(publisher.calls, 1);
    assert_eq!(follower.calls, 0);
}

#[test]
fn expired_snapshot_lists_once_then_the_other_viewer_uses_the_publication() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Claude);
    let before_expiry = session(BackendKind::Claude, "agent_123", Some(201), false);
    let after_expiry = session(BackendKind::Claude, "agent_123", Some(202), false);
    let mut first_backend =
        CountedBackend::sessions(BackendKind::Claude, &scope, vec![before_expiry]);
    let mut second_backend =
        CountedBackend::sessions(BackendKind::Claude, &scope, vec![after_expiry.clone()]);
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    refresh_backend(
        Some(&first),
        &mut first_backend,
        &[],
        &mut first_cursor,
        1_000,
    );
    let refreshed = refresh_backend(
        Some(&second),
        &mut second_backend,
        &[],
        &mut second_cursor,
        3_000,
    );
    let shared = refresh_backend(
        Some(&first),
        &mut first_backend,
        &[],
        &mut first_cursor,
        3_001,
    );

    assert!(
        matches!(refreshed, RefreshOutcome::Authoritative { sessions } if sessions == vec![after_expiry.clone()])
    );
    assert!(
        matches!(shared, RefreshOutcome::Shared { sessions } if sessions == vec![after_expiry])
    );
    assert_eq!(first_backend.calls, 1);
    assert_eq!(second_backend.calls, 1);
}

#[test]
fn source_completion_time_keeps_a_delayed_publication_fresh_for_another_viewer() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let expected = session(BackendKind::Codex, "thread_123", Some(101), false);
    let mut publisher =
        CountedBackend::sessions(BackendKind::Codex, &scope, vec![expected.clone()]);
    publisher.delay = std::time::Duration::from_millis(LISTING_CACHE_FRESHNESS_MS as u64);
    let mut follower = CountedBackend::sessions(BackendKind::Codex, &scope, Vec::new());
    let mut publisher_cursor = RefreshCursor::default();
    let mut follower_cursor = RefreshCursor::default();
    let refresh_started_at = now_ms();

    let published = refresh_backend(
        Some(&first),
        &mut publisher,
        &[],
        &mut publisher_cursor,
        refresh_started_at,
    );
    let shared = refresh_backend(
        Some(&second),
        &mut follower,
        &[],
        &mut follower_cursor,
        now_ms(),
    );

    assert!(
        matches!(published, RefreshOutcome::Authoritative { sessions } if sessions == vec![expected.clone()])
    );
    assert_eq!(publisher.calls, 1);
    assert_eq!(follower.calls, 0);
    assert!(matches!(shared, RefreshOutcome::Shared { sessions } if sessions == vec![expected]));
}

#[test]
fn missing_cache_calls_the_authoritative_source() {
    let (_directory, first, _second) = open_viewers();
    let scope = scope(BackendKind::Claude);
    let expected = session(BackendKind::Claude, "ses_123", None, true);
    let mut backend = CountedBackend::sessions(BackendKind::Claude, &scope, vec![expected.clone()]);
    let mut cursor = RefreshCursor::default();

    let outcome = refresh_backend(Some(&first), &mut backend, &[], &mut cursor, 1_000);

    assert!(
        matches!(outcome, RefreshOutcome::Authoritative { sessions } if sessions == vec![expected])
    );
    assert_eq!(backend.calls, 1);
}

#[test]
fn successful_empty_listing_replaces_rows_and_cached_error_keeps_notice() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let stale_row = session(BackendKind::Codex, "thread_123", Some(101), false);
    let mut initial = CountedBackend::sessions(BackendKind::Codex, &scope, vec![stale_row.clone()]);
    let mut empty = CountedBackend::sessions(BackendKind::Codex, &scope, Vec::new());
    let mut failing = CountedBackend::error(BackendKind::Codex, &scope, "source unavailable");
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    refresh_backend(Some(&first), &mut initial, &[], &mut first_cursor, 1_000);
    let empty_outcome = refresh_backend(Some(&second), &mut empty, &[], &mut second_cursor, 3_000);
    let failed = refresh_backend(
        Some(&first),
        &mut failing,
        &[stale_row],
        &mut first_cursor,
        5_000,
    );

    assert!(
        matches!(empty_outcome, RefreshOutcome::Authoritative { sessions } if sessions.is_empty())
    );
    assert!(
        matches!(failed, RefreshOutcome::CachedError { sessions, notice } if sessions.is_empty() && notice == "codex: command failed: source unavailable")
    );
}

#[test]
fn authoritative_target_returns_the_fresh_row() {
    let scope = scope(BackendKind::Codex);
    let displayed = session(BackendKind::Codex, "thread_123", Some(101), false);
    let fresh = session(BackendKind::Codex, "thread_123", Some(202), false);
    let request = TargetRequest::from(&displayed);
    let mut backend = CountedBackend::sessions(BackendKind::Codex, &scope, vec![fresh.clone()]);

    let resolved = authoritative_target(&mut backend, &request).expect("fresh target");

    assert_eq!(resolved, fresh);
    assert_eq!(backend.calls, 1);
}

#[test]
fn authoritative_target_reports_a_missing_row() {
    let scope = scope(BackendKind::Claude);
    let request = TargetRequest::new(BackendKind::Claude, "ses_123");
    let mut backend = CountedBackend::sessions(BackendKind::Claude, &scope, Vec::new());

    let result = authoritative_target(&mut backend, &request);

    assert!(
        matches!(result, Err(TargetResolution::Missing { notice }) if notice == "claude session is no longer available")
    );
    assert_eq!(backend.calls, 1);
}

#[test]
fn authoritative_target_reports_a_source_error() {
    let scope = scope(BackendKind::Claude);
    let request = TargetRequest::new(BackendKind::Claude, "agent_123");
    let mut backend = CountedBackend::error(BackendKind::Claude, &scope, "source unavailable");

    let result = authoritative_target(&mut backend, &request);

    assert!(
        matches!(result, Err(TargetResolution::SourceError { notice }) if notice == "claude: command failed: source unavailable")
    );
    assert_eq!(backend.calls, 1);
}

/// A listing that started before a mutation landed is a pre-mutation snapshot: the cache
/// already fences it, and the display must refuse it too. Applying it puts the row the user
/// just archived back on screen until the next tick republishes.
#[test]
fn a_fenced_publication_is_not_applied_to_the_display() {
    let directory = tempfile::tempdir().expect("temporary viewer database directory");
    let path = directory.path().join("viewer.sqlite");
    let db = ViewerDb::open(&path).expect("viewer database");
    let scope = scope(BackendKind::Codex);
    let stale = session(BackendKind::Codex, "archived_thread", Some(101), false);
    let last_good = vec![session(BackendKind::Codex, "kept_thread", Some(102), false)];
    let mut backend = FencedBackend {
        kind: BackendKind::Codex,
        scope: scope.clone(),
        sessions: vec![stale.clone()],
        database: path.clone(),
        fence_next: true,
        calls: 0,
    };
    let mut cursor = RefreshCursor::default();

    let fenced = refresh_backend(Some(&db), &mut backend, &last_good, &mut cursor, 1_000);

    assert_eq!(backend.calls, 1);
    assert!(
        matches!(fenced, RefreshOutcome::Unchanged),
        "a fenced listing must leave the display on what it already shows, got {fenced:?}"
    );

    // The very next refresh re-lists and publishes normally, so the fence costs one tick.
    backend.fence_next = false;
    backend.sessions = last_good.clone();
    let republished = refresh_backend(Some(&db), &mut backend, &last_good, &mut cursor, 2_000);

    assert_eq!(backend.calls, 2);
    assert!(
        matches!(republished, RefreshOutcome::Authoritative { sessions } if sessions == last_good)
    );
}

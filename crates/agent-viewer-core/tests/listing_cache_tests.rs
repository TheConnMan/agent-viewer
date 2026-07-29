use agent_viewer_core::{
    Backend, BackendKind, ClaudeBackend, CodexBackend, ListingCacheClaim, ListingCacheRead,
    ListingCacheScope, ListingCacheSnapshot, ListingCacheWrite, OpencodeBackend, OpencodeRuntime,
    OpencodeRuntimeTestConfig, PrRef, Session, SessionOrigin, Status, ViewerDb,
};
use rusqlite::{Connection, params};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

const FRESHNESS_MS: i64 = 1_000;
const LEASE_MS: i64 = 2_000;

fn open_viewers() -> (TempDir, PathBuf, ViewerDb, ViewerDb) {
    let directory = tempfile::tempdir().expect("temporary viewer database directory");
    let path = directory.path().join("viewer.sqlite");
    let first = ViewerDb::open_at(&path).expect("first viewer database");
    let second = ViewerDb::open_at(&path).expect("second viewer database");
    (directory, path, first, second)
}

fn advertised_scope(backend: &impl Backend) -> ListingCacheScope {
    backend
        .listing_scope()
        .expect("configured backend must advertise its listing scope")
}

fn complete_session(backend: BackendKind) -> Session {
    Session {
        backend,
        id: "0198f1e2-4c18-7bc3-9f42-c8c8d91e7001".to_string(),
        short_id: Some("cache-lease".to_string()),
        origin: SessionOrigin::Background,
        title: "Coordinate shared listings".to_string(),
        cwd: PathBuf::from("/work/agent-viewer"),
        git_branch: Some("task/shared-listing-cache".to_string()),
        status: Status::NeedsInput {
            reason: Some("confirm cache protocol".to_string()),
        },
        created_at_ms: 1_725_000_000_000,
        updated_at_ms: 1_725_000_000_123,
        hidden: false,
        companion: false,
        summary: "waiting for a second viewer".to_string(),
        pid: Some(42_424),
        rollout_path: Some(PathBuf::from("/var/tmp/cache-lease.jsonl")),
        pr_refs: vec![PrRef {
            id: "123".to_string(),
            href: Some("https://github.com/TheConnMan/agent-viewer/pull/123".to_string()),
        }],
        daemon_hosted: true,
    }
}

fn snapshot(rows: Vec<Session>) -> ListingCacheSnapshot {
    ListingCacheSnapshot::from_sessions(rows).expect("serialize complete Session rows")
}

fn claimed(
    db: &ViewerDb,
    scope: &ListingCacheScope,
    now_ms: i64,
) -> agent_viewer_core::ListingCacheLease {
    match db
        .claim_listing_refresh(Some(scope), None, now_ms, FRESHNESS_MS, LEASE_MS)
        .expect("claim listing refresh")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected a lease claim, got {other:?}"),
    }
}

fn secure_runtime(root: &Path, candidates: [SocketAddr; 2], password: &str) -> OpencodeRuntime {
    OpencodeRuntime::for_test_secure(OpencodeRuntimeTestConfig {
        candidates,
        startup_timeout: Duration::from_millis(20),
        durable_cwd: root.join("durable-cwd"),
        launcher: Arc::new(|_| Ok(42_424)),
        viewer_db_path: root.join("viewer.sqlite"),
        proc_root: root.join("proc"),
        password_override: Some(password.to_string()),
        before_authorized_write: None,
    })
}

fn socket(address: &str) -> SocketAddr {
    address.parse().expect("loopback test socket")
}

#[test]
fn complete_session_json_round_trip_preserves_every_field() {
    let original = complete_session(BackendKind::Codex);
    let json = serde_json::to_string(&original).expect("serialize Session");
    let restored: Session = serde_json::from_str(&json).expect("deserialize Session");

    assert_eq!(restored, original);
}

#[test]
fn backend_advertised_scopes_change_only_with_namespace_fields_and_exclude_passwords() {
    let root = tempfile::tempdir().expect("backend scope directory");
    let binary_a = root.path().join("claude-a");
    let binary_b = root.path().join("claude-b");
    std::fs::write(&binary_a, "").expect("first Claude executable");
    std::fs::write(&binary_b, "").expect("second Claude executable");
    let jobs_a = root.path().join("jobs-a");
    let jobs_b = root.path().join("jobs-b");
    let sessions_a = root.path().join("sessions-a");
    let sessions_b = root.path().join("sessions-b");
    std::fs::create_dir_all(&jobs_a).expect("first jobs root");
    std::fs::create_dir_all(&jobs_b).expect("second jobs root");
    std::fs::create_dir_all(&sessions_a).expect("first sessions root");
    std::fs::create_dir_all(&sessions_b).expect("second sessions root");

    let codex_a = CodexBackend::new(root.path().join("codex-a"));
    let codex_b = CodexBackend::new(root.path().join("codex-b"));
    assert_ne!(advertised_scope(&codex_a), advertised_scope(&codex_b));

    let claude = ClaudeBackend::with_roots(
        binary_a.to_str().expect("UTF8 path"),
        jobs_a.clone(),
        sessions_a.clone(),
    );
    let changed_binary = ClaudeBackend::with_roots(
        binary_b.to_str().expect("UTF8 path"),
        jobs_a.clone(),
        sessions_a.clone(),
    );
    let changed_jobs = ClaudeBackend::with_roots(
        binary_a.to_str().expect("UTF8 path"),
        jobs_b,
        sessions_a.clone(),
    );
    let changed_sessions =
        ClaudeBackend::with_roots(binary_a.to_str().expect("UTF8 path"), jobs_a, sessions_b);
    assert_ne!(advertised_scope(&claude), advertised_scope(&changed_binary));
    assert_ne!(advertised_scope(&claude), advertised_scope(&changed_jobs));
    assert_ne!(
        advertised_scope(&claude),
        advertised_scope(&changed_sessions)
    );

    let db_a = root.path().join("opencode-a.sqlite");
    let db_b = root.path().join("opencode-b.sqlite");
    let compatibility_a = OpencodeBackend::with_db(db_a.clone());
    let compatibility_b = OpencodeBackend::with_db(db_b.clone());
    let ordered = [socket("127.0.0.1:41001"), socket("127.0.0.1:41002")];
    let reversed = [ordered[1], ordered[0]];
    let secure_one = OpencodeBackend::with_db_and_runtime(
        db_a.clone(),
        secure_runtime(root.path(), ordered, "first generated password"),
    );
    let secure_other_password = OpencodeBackend::with_db_and_runtime(
        db_a.clone(),
        secure_runtime(root.path(), ordered, "second generated password"),
    );
    let secure_reordered = OpencodeBackend::with_db_and_runtime(
        db_a.clone(),
        secure_runtime(root.path(), reversed, "first generated password"),
    );

    assert_ne!(
        advertised_scope(&compatibility_a),
        advertised_scope(&compatibility_b)
    );
    assert_ne!(
        advertised_scope(&compatibility_a),
        advertised_scope(&secure_one)
    );
    assert_ne!(
        advertised_scope(&secure_one),
        advertised_scope(&secure_reordered)
    );
    assert_eq!(
        advertised_scope(&secure_one),
        advertised_scope(&secure_other_password)
    );

    let (_directory, path, viewer, _other) = open_viewers();
    let scope = advertised_scope(&secure_one);
    let lease = claimed(&viewer, &scope, 10);
    assert_eq!(
        viewer
            .publish_listing(
                &lease,
                snapshot(vec![complete_session(BackendKind::Opencode)]),
                11
            )
            .expect("publish OpenCode snapshot"),
        ListingCacheWrite::Published
    );
    let connection = Connection::open(path).expect("inspect viewer database");
    let stored: (String, String) = connection
        .query_row(
            "SELECT scope, snapshot_json FROM backend_listing_cache WHERE scope = ?1",
            params![scope.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored OpenCode cache row");
    assert!(!stored.0.contains("generated password"));
    assert!(!stored.1.contains("generated password"));
}

#[test]
fn missing_scope_bypasses_every_shared_cache_operation() {
    let (_directory, _path, first, _second) = open_viewers();

    assert_eq!(
        first
            .claim_listing_refresh(None, None, 10, FRESHNESS_MS, LEASE_MS)
            .expect("scope less refresh"),
        ListingCacheClaim::Bypass
    );
    assert_eq!(
        first
            .invalidate_listing_scope(None)
            .expect("scope less invalidation"),
        false
    );
}

#[test]
fn one_scoped_lease_wins_while_an_independent_scope_can_refresh() {
    let (_directory, _path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("backend scope directory");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let claude = advertised_scope(&ClaudeBackend::with_roots(
        root.path().join("claude").to_str().expect("UTF8 path"),
        root.path().join("jobs"),
        root.path().join("sessions"),
    ));

    let _codex_lease = claimed(&first, &codex, 10);
    assert_eq!(
        second
            .claim_listing_refresh(Some(&codex), None, 11, FRESHNESS_MS, LEASE_MS)
            .expect("second Codex claim"),
        ListingCacheClaim::LeaseHeld
    );
    assert!(matches!(
        second
            .claim_listing_refresh(Some(&claude), None, 11, FRESHNESS_MS, LEASE_MS)
            .expect("Claude claim"),
        ListingCacheClaim::Claimed(_)
    ));
}

#[test]
fn locked_claim_rereads_a_snapshot_published_by_another_viewer() {
    let (_directory, _path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));

    assert!(matches!(
        first
            .read_listing_snapshot(&codex, 10)
            .expect("initial cache read"),
        ListingCacheRead::Miss
    ));
    let publisher = claimed(&second, &codex, 11);
    let row = complete_session(BackendKind::Codex);
    assert_eq!(
        second
            .publish_listing(&publisher, snapshot(vec![row.clone()]), 12)
            .expect("publish between observation and claim"),
        ListingCacheWrite::Published
    );

    match first
        .claim_listing_refresh(Some(&codex), None, 13, FRESHNESS_MS, LEASE_MS)
        .expect("locked reread claim")
    {
        ListingCacheClaim::Fresh(snapshot) => assert_eq!(snapshot.sessions(), &[row]),
        other => panic!("claim must reread the fresh publication, got {other:?}"),
    }
}

#[test]
fn expired_lease_takeover_and_generation_fencing_reject_stale_publishers() {
    let (_directory, _path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let stale_owner = claimed(&first, &codex, 10);

    assert_eq!(
        second
            .claim_listing_refresh(
                Some(&codex),
                None,
                10 + LEASE_MS - 1,
                FRESHNESS_MS,
                LEASE_MS,
            )
            .expect("unexpired lease"),
        ListingCacheClaim::LeaseHeld
    );
    let replacement = claimed(&second, &codex, 10 + LEASE_MS);
    assert_eq!(
        first
            .publish_listing(
                &stale_owner,
                snapshot(vec![complete_session(BackendKind::Codex)]),
                2_011
            )
            .expect("fenced stale publish"),
        ListingCacheWrite::Fenced
    );
    assert_eq!(
        second
            .publish_listing(&replacement, snapshot(Vec::new()), 2_012)
            .expect("replacement publish"),
        ListingCacheWrite::Published
    );
}

#[test]
fn invalid_json_is_a_cache_miss_and_invalidation_fences_the_old_lease() {
    let (_directory, path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let lease = claimed(&first, &codex, 10);
    assert_eq!(
        first
            .publish_listing(
                &lease,
                snapshot(vec![complete_session(BackendKind::Codex)]),
                11
            )
            .expect("publish valid snapshot"),
        ListingCacheWrite::Published
    );

    Connection::open(path)
        .expect("corrupt cache row")
        .execute(
            "UPDATE backend_listing_cache SET snapshot_json = ?1 WHERE scope = ?2",
            params!["{not valid JSON", codex.as_str()],
        )
        .expect("write malformed snapshot JSON");
    assert!(matches!(
        first
            .read_listing_snapshot(&codex, 12)
            .expect("corrupt cache fallback"),
        ListingCacheRead::Miss
    ));

    let invalidated_lease = claimed(&first, &codex, 13);
    assert!(
        first
            .invalidate_listing_scope(Some(&codex))
            .expect("invalidate cached backend scope")
    );
    assert_eq!(
        first
            .publish_listing(
                &invalidated_lease,
                snapshot(vec![complete_session(BackendKind::Codex)]),
                14,
            )
            .expect("fenced invalidated publish"),
        ListingCacheWrite::Fenced
    );
}

#[test]
fn successful_empty_snapshot_survives_a_later_source_error() {
    let (_directory, _path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let initial = claimed(&first, &codex, 10);
    assert_eq!(
        first
            .publish_listing(&initial, snapshot(Vec::new()), 11)
            .expect("publish successful empty listing"),
        ListingCacheWrite::Published
    );

    let failed_refresh = claimed(&first, &codex, 11 + FRESHNESS_MS);
    first
        .fail_listing_refresh(&failed_refresh)
        .expect("release failed source refresh without overwriting snapshot");

    match first
        .read_listing_snapshot(&codex, 11 + FRESHNESS_MS + 1)
        .expect("read preserved empty snapshot")
    {
        ListingCacheRead::Stale(snapshot) => {
            assert_eq!(snapshot.sessions(), &[]);
            assert_eq!(snapshot.published_at_ms(), 11);
        }
        other => panic!("source errors must preserve the prior empty snapshot, got {other:?}"),
    }
}

#[test]
fn matching_published_generation_returns_unchanged_without_decoding_json() {
    let (_directory, path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let lease = claimed(&first, &codex, 10);
    first
        .publish_listing(
            &lease,
            snapshot(vec![complete_session(BackendKind::Codex)]),
            11,
        )
        .expect("publish initial snapshot");

    let published_generation = match first
        .claim_listing_refresh(Some(&codex), None, 12, FRESHNESS_MS, LEASE_MS)
        .expect("read fresh snapshot and generation")
    {
        ListingCacheClaim::Fresh(snapshot) => snapshot.published_generation(),
        other => panic!("expected fresh snapshot, got {other:?}"),
    };
    Connection::open(path)
        .expect("open cache for malformed payload")
        .execute(
            "UPDATE backend_listing_cache SET snapshot_json = ?1 \
             WHERE backend = ?2 AND scope = ?3",
            params!["{malformed", BackendKind::Codex.name(), codex.as_str()],
        )
        .expect("replace payload without changing generation");

    assert_eq!(
        first
            .claim_listing_refresh(
                Some(&codex),
                Some(published_generation),
                13,
                FRESHNESS_MS,
                LEASE_MS,
            )
            .expect("matching generation lookup"),
        ListingCacheClaim::Unchanged
    );
}

#[test]
fn absent_or_different_generation_returns_snapshot_and_published_generation() {
    let (_directory, _path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let row = complete_session(BackendKind::Codex);
    let lease = claimed(&first, &codex, 10);
    first
        .publish_listing(&lease, snapshot(vec![row.clone()]), 11)
        .expect("publish initial snapshot");

    let published_generation = match first
        .claim_listing_refresh(Some(&codex), None, 12, FRESHNESS_MS, LEASE_MS)
        .expect("lookup without a known generation")
    {
        ListingCacheClaim::Fresh(snapshot) => {
            assert_eq!(snapshot.sessions(), &[row.clone()]);
            snapshot.published_generation()
        }
        other => panic!("expected decoded snapshot, got {other:?}"),
    };

    match first
        .claim_listing_refresh(
            Some(&codex),
            Some(published_generation.saturating_add(1)),
            13,
            FRESHNESS_MS,
            LEASE_MS,
        )
        .expect("lookup with a different generation")
    {
        ListingCacheClaim::Fresh(snapshot) => {
            assert_eq!(snapshot.sessions(), &[row]);
            assert_eq!(snapshot.published_generation(), published_generation);
        }
        other => panic!("different generation must return the snapshot, got {other:?}"),
    }
}

#[test]
fn refresh_lease_preserves_published_generation_until_successful_publication() {
    let (_directory, _path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let initial_lease = claimed(&first, &codex, 10);
    first
        .publish_listing(&initial_lease, snapshot(Vec::new()), 11)
        .expect("publish initial snapshot");
    let initial_generation = match first
        .read_listing_snapshot(&codex, 12)
        .expect("read initial generation")
    {
        ListingCacheRead::Fresh(snapshot) => snapshot.published_generation(),
        other => panic!("expected fresh initial snapshot, got {other:?}"),
    };

    let refresh_lease = match first
        .claim_listing_refresh(
            Some(&codex),
            Some(initial_generation),
            11 + FRESHNESS_MS,
            FRESHNESS_MS,
            LEASE_MS,
        )
        .expect("claim stale refresh")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected refresh lease, got {other:?}"),
    };
    match first
        .read_listing_snapshot(&codex, 11 + FRESHNESS_MS)
        .expect("read while refresh lease is held")
    {
        ListingCacheRead::Stale(snapshot) => {
            assert_eq!(snapshot.published_generation(), initial_generation);
        }
        other => panic!("lease claim must preserve the published snapshot, got {other:?}"),
    }

    first
        .publish_listing(
            &refresh_lease,
            snapshot(vec![complete_session(BackendKind::Codex)]),
            11 + FRESHNESS_MS,
        )
        .expect("publish refreshed snapshot");
    match first
        .read_listing_snapshot(&codex, 12 + FRESHNESS_MS)
        .expect("read refreshed generation")
    {
        ListingCacheRead::Fresh(snapshot) => {
            assert_ne!(snapshot.published_generation(), initial_generation);
        }
        other => panic!("expected refreshed snapshot, got {other:?}"),
    }
}

#[test]
fn invalidation_changes_the_published_generation() {
    let (_directory, _path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let lease = claimed(&first, &codex, 10);
    first
        .publish_listing(&lease, snapshot(Vec::new()), 11)
        .expect("publish initial snapshot");
    let initial_generation = match first
        .read_listing_snapshot(&codex, 12)
        .expect("read initial generation")
    {
        ListingCacheRead::Fresh(snapshot) => snapshot.published_generation(),
        other => panic!("expected fresh initial snapshot, got {other:?}"),
    };

    assert!(
        first
            .invalidate_listing_scope(Some(&codex))
            .expect("invalidate listing scope")
    );
    match first
        .read_listing_snapshot(&codex, 13)
        .expect("read invalidated snapshot")
    {
        ListingCacheRead::Stale(snapshot) => {
            assert_ne!(snapshot.published_generation(), initial_generation);
        }
        other => panic!("invalidation must preserve a stale snapshot, got {other:?}"),
    }
}

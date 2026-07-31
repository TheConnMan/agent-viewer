use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::{
    Backend, BackendKind, ListingCacheClaim, ListingCacheRead, ListingCacheScope,
    ListingCacheSnapshot, ListingCacheWrite, PrRef, Session, SessionOrigin, Status, ViewerDb,
};
use rusqlite::{Connection, params};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

const FRESHNESS_MS: i64 = agent_viewer_core::state::LISTING_CACHE_FRESHNESS_MS;
const LEASE_MS: i64 = 2_000;
const RENEWABLE_LEASE_MS: i64 = 600;

fn open_viewers() -> (TempDir, PathBuf, ViewerDb, ViewerDb) {
    let directory = tempfile::tempdir().expect("temporary viewer database directory");
    let path = directory.path().join("viewer.sqlite");
    let first = ViewerDb::open(&path).expect("first viewer database");
    let second = ViewerDb::open(&path).expect("second viewer database");
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
        subagent: false,
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
    claimed_for(db, scope, now_ms, LEASE_MS)
}

fn claimed_for(
    db: &ViewerDb,
    scope: &ListingCacheScope,
    now_ms: i64,
    lease_ms: i64,
) -> agent_viewer_core::ListingCacheLease {
    match db
        .claim_listing_refresh(Some(scope), None, now_ms, FRESHNESS_MS, lease_ms)
        .expect("claim listing refresh")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected a lease claim, got {other:?}"),
    }
}

/// A listing big enough for a payload rewrite to be visible in the write ahead log. The real
/// snapshot on this box is tens of megabytes; a few hundred kilobytes is enough to separate
/// "rewrote the payload" from "stamped the metadata" without slowing the suite down.
fn bulk_sessions(count: usize) -> Vec<Session> {
    (0..count)
        .map(|index| {
            let mut session = complete_session(BackendKind::Codex);
            session.id = format!("0198f1e2-4c18-7bc3-9f42-c8c8d91e{index:04}");
            session.title = format!("Coordinate shared listings {index}");
            session
        })
        .collect()
}

fn wal_len(path: &Path) -> u64 {
    std::fs::metadata(PathBuf::from(format!("{}-wal", path.display())))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

/// Fold the write ahead log back into the database so the next measurement starts from a
/// known floor. Best effort: a failed checkpoint only raises the baseline, and every
/// assertion here is on the DELTA.
fn checkpoint(path: &Path) {
    if let Ok(connection) = Connection::open(path) {
        let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis()
        .try_into()
        .expect("epoch milliseconds fit i64")
}

fn stored_lease(path: &Path, scope: &ListingCacheScope) -> (i64, Option<String>, i64) {
    Connection::open(path)
        .expect("inspect viewer database")
        .query_row(
            "SELECT generation, lease_owner, lease_until_ms \
             FROM backend_listing_cache WHERE backend = ?1 AND scope = ?2",
            params![scope.backend().name(), scope.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("stored listing lease")
}

#[test]
fn complete_session_json_round_trip_preserves_every_field() {
    let original = complete_session(BackendKind::Codex);
    let json = serde_json::to_string(&original).expect("serialize Session");
    let restored: Session = serde_json::from_str(&json).expect("deserialize Session");

    assert_eq!(restored, original);
}

#[test]
fn backend_advertised_scopes_change_with_compatibility_namespace_fields() {
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
    assert!(
        !first
            .invalidate_listing_scope(None)
            .expect("scope less invalidation")
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
fn refresh_guard_renews_the_same_owner_and_generation_past_the_initial_deadline() {
    let (_directory, path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let claimed_at_ms = now_ms();
    let lease = claimed_for(&first, &codex, claimed_at_ms, RENEWABLE_LEASE_MS);
    let initial = stored_lease(&path, &codex);
    let guard = first
        .guard_listing_refresh(&lease, RENEWABLE_LEASE_MS)
        .expect("start listing lease renewal");

    std::thread::sleep(Duration::from_millis(900));

    let renewed = stored_lease(&path, &codex);
    assert_eq!(renewed.0, initial.0, "renewal must not advance generation");
    assert_eq!(renewed.1, initial.1, "renewal must preserve lease owner");
    assert!(
        renewed.2 > initial.2,
        "renewal must extend the lease deadline"
    );
    assert_eq!(
        second
            .claim_listing_refresh(
                Some(&codex),
                None,
                now_ms(),
                FRESHNESS_MS,
                RENEWABLE_LEASE_MS,
            )
            .expect("second viewer claim after initial deadline"),
        ListingCacheClaim::LeaseHeld
    );

    drop(guard);
}

#[test]
fn dropping_refresh_guard_allows_takeover_after_the_renewed_deadline() {
    let (_directory, path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let lease = claimed_for(&first, &codex, now_ms(), RENEWABLE_LEASE_MS);
    let guard = first
        .guard_listing_refresh(&lease, RENEWABLE_LEASE_MS)
        .expect("start listing lease renewal");

    std::thread::sleep(Duration::from_millis(900));
    drop(guard);

    let (_, _, renewed_until_ms) = stored_lease(&path, &codex);
    let wait_ms = renewed_until_ms
        .saturating_sub(now_ms())
        .saturating_add(150)
        .max(150);
    std::thread::sleep(Duration::from_millis(wait_ms as u64));

    assert!(matches!(
        second
            .claim_listing_refresh(
                Some(&codex),
                None,
                now_ms(),
                FRESHNESS_MS,
                RENEWABLE_LEASE_MS,
            )
            .expect("take over released renewal lease"),
        ListingCacheClaim::Claimed(_)
    ));
}

#[test]
fn publication_fences_guard_renewal_and_advances_generation() {
    let (_directory, path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let lease = claimed_for(&first, &codex, now_ms(), RENEWABLE_LEASE_MS);
    let initial_generation = stored_lease(&path, &codex).0;
    let guard = first
        .guard_listing_refresh(&lease, RENEWABLE_LEASE_MS)
        .expect("start listing lease renewal");

    assert_eq!(
        first
            .publish_listing(
                &lease,
                snapshot(vec![complete_session(BackendKind::Codex)]),
                now_ms(),
            )
            .expect("publish guarded listing"),
        ListingCacheWrite::Published
    );
    std::thread::sleep(Duration::from_millis(800));

    let published = stored_lease(&path, &codex);
    assert_eq!(published.0, initial_generation.saturating_add(1));
    assert_eq!(published.1, None);
    assert_eq!(published.2, 0);
    let replacement = match second
        .claim_listing_refresh(Some(&codex), None, now_ms(), 0, RENEWABLE_LEASE_MS)
        .expect("claim after guarded publication")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("published lease must remain released, got {other:?}"),
    };
    assert_eq!(
        replacement.next_published_generation(),
        initial_generation.saturating_add(2)
    );

    drop(guard);
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
            "UPDATE backend_listing_snapshot SET snapshot_json = ?1 WHERE scope = ?2",
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
fn decoded_cache_snapshot_cannot_be_republished_as_an_empty_payload() {
    let (_directory, path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let row = complete_session(BackendKind::Codex);
    let initial = claimed(&first, &codex, 10);
    first
        .publish_listing(&initial, snapshot(vec![row.clone()]), 11)
        .expect("publish initial snapshot");
    let decoded = match second
        .read_listing_snapshot(&codex, 12)
        .expect("read decoded snapshot")
    {
        ListingCacheRead::Fresh(snapshot) => {
            assert_eq!(snapshot.sessions(), std::slice::from_ref(&row));
            snapshot
        }
        other => panic!("expected fresh decoded snapshot, got {other:?}"),
    };
    let stale_at_ms = 11 + FRESHNESS_MS;
    let later_lease = claimed(&second, &codex, stale_at_ms);

    let publish_result = second.publish_listing(&later_lease, decoded, stale_at_ms + 1);
    let raw_payload: Option<String> = Connection::open(path)
        .expect("inspect viewer database")
        .query_row(
            "SELECT snapshot_json FROM backend_listing_snapshot \
             WHERE backend = ?1 AND scope = ?2",
            params![BackendKind::Codex.name(), codex.as_str()],
            |sqlite_row| sqlite_row.get(0),
        )
        .expect("stored snapshot payload");
    let raw_payload_is_valid_or_absent = raw_payload.as_deref().is_none_or(|json| {
        serde_json::from_str::<Vec<Session>>(json)
            .is_ok_and(|sessions| sessions == std::slice::from_ref(&row))
    });
    let subsequent_read_is_valid_or_absent = match first
        .read_listing_snapshot(&codex, stale_at_ms + 2)
        .expect("read after rejected decoded publication")
    {
        ListingCacheRead::Fresh(snapshot) | ListingCacheRead::Stale(snapshot) => {
            snapshot.sessions() == std::slice::from_ref(&row)
        }
        ListingCacheRead::Miss => raw_payload.is_none(),
    };

    assert!(
        publish_result.is_err(),
        "publishing a decoded cache snapshot must return an error"
    );
    assert!(
        raw_payload_is_valid_or_absent,
        "rejected publication must not commit malformed empty JSON"
    );
    assert!(
        subsequent_read_is_valid_or_absent,
        "subsequent reads must retain the valid prior snapshot or miss"
    );
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
            "UPDATE backend_listing_snapshot SET snapshot_json = ?1 \
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
            assert_eq!(snapshot.sessions(), std::slice::from_ref(&row));
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

/// A refresh cycle that finds nothing changed must not rewrite the payload, and taking the
/// lease for it must not rewrite the payload either. Measured on the live box before this was
/// fixed: a 24 MB snapshot, republished every 2 s tick, left a 71 MB write ahead log. The
/// assertion is on bytes written, because that IS the defect - a generation counter would go
/// green with the whole payload still going to disk every cycle.
#[test]
fn an_unchanged_refresh_cycle_does_not_rewrite_the_stored_payload() {
    let (_directory, path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let rows = bulk_sessions(1_200);
    let payload_len = serde_json::to_string(&rows)
        .expect("serialize bulk listing")
        .len() as u64;
    assert!(
        payload_len > 400_000,
        "the payload must be large enough to dominate any metadata write, got {payload_len}"
    );
    let lease = claimed(&first, &codex, 10);
    assert_eq!(
        first
            .publish_listing(&lease, snapshot(rows.clone()), 11)
            .expect("publish initial listing"),
        ListingCacheWrite::Published
    );

    checkpoint(&path);
    let before = wal_len(&path);
    let republish = claimed(&first, &codex, 11 + FRESHNESS_MS);
    let write = first
        .publish_listing(&republish, snapshot(rows.clone()), 12 + FRESHNESS_MS)
        .expect("republish an identical listing");
    let written = wal_len(&path).saturating_sub(before);

    assert_eq!(write, ListingCacheWrite::Published);
    assert!(
        written < payload_len / 4,
        "an unchanged refresh cycle wrote {written} bytes for a {payload_len} byte payload"
    );
    match first
        .read_listing_snapshot(&codex, 13 + FRESHNESS_MS)
        .expect("read the republished listing")
    {
        ListingCacheRead::Fresh(snapshot) => assert_eq!(snapshot.sessions(), rows),
        other => panic!("the skipped payload write must leave a fresh snapshot, got {other:?}"),
    }
}

/// An existing viewer.db carries its listing payload in the old `backend_listing_cache` column.
/// Opening it must release that column - a payload left there keeps every metadata write
/// rewriting the whole record - and the scope must degrade to a plain cache miss that the next
/// refresh republishes, never an error that takes the backend out.
#[test]
fn opening_a_database_with_a_legacy_payload_column_releases_it_and_reads_as_a_miss() {
    let (_directory, path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let row = complete_session(BackendKind::Codex);
    let lease = claimed(&first, &codex, 10);
    first
        .publish_listing(&lease, snapshot(vec![row.clone()]), 11)
        .expect("publish a snapshot to move into the legacy column");
    drop(first);

    let legacy = Connection::open(&path).expect("rewrite the database in its pre split shape");
    legacy
        .execute(
            "UPDATE backend_listing_cache SET snapshot_json = \
             (SELECT snapshot_json FROM backend_listing_snapshot \
              WHERE backend = backend_listing_cache.backend AND scope = backend_listing_cache.scope)",
            [],
        )
        .expect("move the payload back into the legacy column");
    legacy
        .execute("DELETE FROM backend_listing_snapshot", [])
        .expect("drop the split payload row");
    drop(legacy);

    let reopened = ViewerDb::open(&path).expect("reopen a database carrying a legacy payload");
    let legacy_payload: Option<String> = Connection::open(&path)
        .expect("inspect the reopened database")
        .query_row(
            "SELECT snapshot_json FROM backend_listing_cache WHERE backend = ?1 AND scope = ?2",
            params![BackendKind::Codex.name(), codex.as_str()],
            |sqlite_row| sqlite_row.get(0),
        )
        .expect("read the legacy payload column");

    assert_eq!(
        legacy_payload, None,
        "the legacy payload column must be released so metadata writes stay small"
    );
    assert!(
        matches!(
            reopened
                .read_listing_snapshot(&codex, 12)
                .expect("read a scope whose payload predates the split"),
            ListingCacheRead::Miss
        ),
        "a payload that predates the split must read as a miss, not an error"
    );
    let republished = claimed(&reopened, &codex, 13);
    assert_eq!(
        reopened
            .publish_listing(&republished, snapshot(vec![row]), 14)
            .expect("republish after the migration miss"),
        ListingCacheWrite::Published
    );
}

/// Freshness is a wall clock comparison, so a backward clock step leaves a stamp in the
/// future. Comparing only the upper bound pins that snapshot as fresh until the clock catches
/// up, which can be hours: nobody refreshes and every viewer serves stale rows.
#[test]
fn a_snapshot_stamped_in_the_future_reads_as_stale_and_stays_claimable() {
    let (_directory, _path, first, _second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let row = complete_session(BackendKind::Codex);
    let lease = claimed(&first, &codex, 10);
    let ahead_of_the_clock = 6 * 60 * 60 * 1_000;
    first
        .publish_listing(&lease, snapshot(vec![row.clone()]), ahead_of_the_clock)
        .expect("publish a snapshot stamped by a clock that later rolled back");

    match first
        .read_listing_snapshot(&codex, 11)
        .expect("read after the clock rolled back")
    {
        ListingCacheRead::Stale(snapshot) => assert_eq!(snapshot.sessions(), &[row]),
        other => panic!("a future stamp must read as stale, not fresh, got {other:?}"),
    }
    assert!(
        matches!(
            first
                .claim_listing_refresh(Some(&codex), None, 12, FRESHNESS_MS, LEASE_MS)
                .expect("claim after the clock rolled back"),
            ListingCacheClaim::Claimed(_)
        ),
        "a future stamp must not keep every viewer out of the refresh"
    );
}

/// Same rollback, applied to the lease deadline: a deadline further out than any live viewer
/// could have written holds the scope hostage for the whole difference, and no viewer refreshes
/// until it passes.
#[test]
fn a_lease_deadline_beyond_the_horizon_does_not_hold_the_scope() {
    let (_directory, _path, first, second) = open_viewers();
    let root = tempfile::tempdir().expect("Codex home");
    let codex = advertised_scope(&CodexBackend::new(root.path().join("codex")));
    let ahead_of_the_clock = 24 * 60 * 60 * 1_000;
    let _stranded = claimed(&first, &codex, ahead_of_the_clock);

    assert!(
        matches!(
            second
                .claim_listing_refresh(Some(&codex), None, 10, FRESHNESS_MS, LEASE_MS)
                .expect("claim after the clock rolled back"),
            ListingCacheClaim::Claimed(_)
        ),
        "a lease deadline a day out must be treated as expired, not held"
    );
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

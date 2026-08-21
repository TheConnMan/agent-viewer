//! Contract tests for viewer owned presentation state, spawn tracking, and overlays.

use agent_viewer_core::backend::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_core::state::{
    DEFAULT_RETENTION_WINDOW_MS, GroupingMode, SortOrder, SpawnRecord, ViewerDb, ViewerState,
    apply_viewer_state, match_spawn, match_spawn_between,
};
use agent_viewer_core::{
    ListingCacheClaim, ListingCacheRead, ListingCacheScope, ListingCacheSnapshot,
};
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn sess(backend: BackendKind, id: &str, cwd: &str, created_at_ms: i64, status: Status) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Exec,
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        git_branch: None,
        status,
        created_at_ms,
        updated_at_ms: created_at_ms,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn temp_db_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("viewer.db");
    (dir, path)
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .expect("path metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
#[cfg(unix)]
fn viewer_db_open_preserves_an_existing_parent_directory_mode() {
    let root = tempfile::TempDir::new().unwrap();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    ViewerDb::open(&state_dir.join("viewer.db")).expect("open viewer db");

    assert_eq!(mode(&state_dir), 0o755);
}

#[test]
#[cfg(unix)]
fn viewer_db_open_creates_a_new_state_leaf_with_owner_only_mode() {
    let root = tempfile::TempDir::new().unwrap();
    let state_parent = root.path().join("state");
    std::fs::create_dir(&state_parent).unwrap();
    std::fs::set_permissions(&state_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state_leaf = state_parent.join("agent-viewer");

    ViewerDb::open(&state_leaf.join("viewer.db")).expect("open viewer db");

    assert_eq!(mode(&state_parent), 0o755);
    assert_eq!(mode(&state_leaf), 0o700);
}

#[test]
#[cfg(unix)]
fn viewer_db_open_keeps_database_and_sqlite_sidecars_owner_only() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    db.set_group_collapsed("project:/sidecars", true)
        .expect("write state to create sqlite sidecars");

    for path in [
        path.clone(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        assert!(path.exists(), "SQLite must create {}", path.display());
        assert_eq!(
            mode(&path) & 0o077,
            0,
            "{} must be owner only",
            path.display()
        );
    }
}

/// `open` creates its state path's missing ancestors by walking upward. A RELATIVE path runs
/// off the top of that walk (`Path::new("").parent()` is None), and that is exactly the shape
/// `open_default` builds when HOME is unset or empty. The walk has to terminate and report
/// failure so the TUI's `open_default().ok()` degrades to running without viewer state.
///
/// The regression this guards is an infinite loop, so the call runs on its own thread behind a
/// deadline: a reintroduced fixpoint fails this test instead of hanging the whole suite.
#[test]
fn viewer_db_open_terminates_on_a_rootless_relative_path() {
    let probe = PathBuf::from("agent-viewer-rootless-probe/state/viewer.db");
    let (sender, receiver) = std::sync::mpsc::channel();
    let probed = probe.clone();
    std::thread::spawn(move || {
        let _ = sender.send(ViewerDb::open(&probed).is_err());
    });

    match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(is_err) => assert!(
            is_err,
            "a state path with no usable parent must report failure, not a usable database"
        ),
        Err(_) => panic!(
            "ViewerDb::open never returned for a rootless relative path: the ancestor walk does not terminate"
        ),
    }
    assert!(
        !PathBuf::from("agent-viewer-rootless-probe").exists(),
        "an abandoned ancestor walk must not create directories in the working directory"
    );
}

#[test]
fn viewer_db_spawn_roundtrip() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    let cwd = PathBuf::from("/home/user/spawned-proj");
    let rowid = db
        .record_spawn(BackendKind::Codex, &cwd, 4242, 1_000_000)
        .expect("record spawn");

    let unresolved = db.unresolved_spawns().expect("unresolved");
    assert!(unresolved.iter().any(|r| r.rowid == rowid
        && r.backend == BackendKind::Codex
        && r.cwd == cwd
        && r.pid == 4242
        && r.spawned_at_ms == 1_000_000));

    db.resolve_spawn(rowid, "session-xyz")
        .expect("resolve spawn");
    assert!(db.unresolved_spawns().expect("unresolved").is_empty());

    let state = db.viewer_state().expect("viewer state");
    assert!(
        state
            .pinned
            .contains(&(BackendKind::Codex, "session-xyz".to_string()))
    );
    assert_eq!(
        state
            .spawn_pids
            .get(&(BackendKind::Codex, "session-xyz".to_string())),
        Some(&4242)
    );
}

#[test]
fn viewer_db_grok_spawn_identity_round_trip() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    let cwd = PathBuf::from("/home/user/grok-project");
    let rowid = db
        .record_spawn(BackendKind::Grok, &cwd, 42_424, 1_787_289_306_007)
        .expect("record Grok spawn");

    let stored = db
        .unresolved_spawns()
        .expect("read unresolved Grok spawn")
        .into_iter()
        .find(|record| record.rowid == rowid)
        .expect("Grok spawn record");
    assert_eq!(stored.backend, BackendKind::Grok);
    assert_eq!(stored.cwd, cwd);

    db.resolve_spawn(rowid, "grok-session-exact")
        .expect("resolve exact Grok identity");
    drop(db);
    let reopened = ViewerDb::open(&path).expect("reopen viewer db");
    let state = reopened.viewer_state().expect("read Grok viewer state");
    assert!(
        state
            .pinned
            .contains(&(BackendKind::Grok, "grok-session-exact".to_string()))
    );
    assert_eq!(
        state
            .spawn_pids
            .get(&(BackendKind::Grok, "grok-session-exact".to_string())),
        Some(&42_424)
    );
}

#[test]
fn viewer_db_grok_listing_snapshot_round_trip_preserves_backend_and_status() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    let scope = ListingCacheScope::new(BackendKind::Grok, "official-grok-home")
        .expect("Grok listing scope");
    let lease = match db
        .claim_listing_refresh(Some(&scope), None, 10_000, 2_000, 5_000)
        .expect("claim Grok listing")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected Grok listing lease, got {other:?}"),
    };
    let original = sess(
        BackendKind::Grok,
        "grok-snapshot-session",
        "/home/user/grok-project",
        9_000,
        Status::Unknown,
    );
    db.publish_listing(
        &lease,
        ListingCacheSnapshot::from_sessions(vec![original.clone()]).expect("encode Grok listing"),
        10_000,
    )
    .expect("publish Grok listing");

    drop(db);
    let reopened = ViewerDb::open(&path).expect("reopen viewer db");
    let snapshot = match reopened
        .read_listing_snapshot(&scope, 10_001)
        .expect("read Grok listing")
    {
        ListingCacheRead::Fresh(snapshot) => snapshot,
        other => panic!("expected fresh Grok listing, got {other:?}"),
    };
    assert_eq!(snapshot.into_sessions(), vec![original]);
}

#[test]
fn viewer_db_recreates_on_corrupt() {
    let (_dir, path) = temp_db_path();
    std::fs::write(&path, b"this is not a sqlite database at all").unwrap();

    // A file that is not a database at all is replaced with a fresh empty DB, not an error.
    let db = ViewerDb::open(&path).expect("recreate on corrupt");
    assert_eq!(
        db.viewer_state().expect("viewer state"),
        ViewerState::default()
    );
    // The replacement is a working DB, not just an openable one.
    db.set_group_collapsed("project:/after-recreate", true)
        .expect("write to recreated db");
    assert!(
        db.collapsed_groups()
            .expect("collapsed groups")
            .contains("project:/after-recreate")
    );
}

#[test]
fn viewer_db_recreates_on_malformed_pages() {
    let (_dir, path) = temp_db_path();

    // A real SQLite file (valid header magic) whose page 1 b-tree is shredded. This is the
    // SQLITE_CORRUPT arm of the recreate rule, distinct from the not-a-database arm above.
    {
        let db = ViewerDb::open(&path).expect("seed real db");
        db.set_group_collapsed("project:/doomed", true)
            .expect("seed collapsed group");
    }
    let mut bytes = std::fs::read(&path).expect("read seeded db");
    assert!(bytes.len() > 512, "seeded db unexpectedly small");
    // Keep the 100-byte file header (so sqlite accepts the file) and shred everything after.
    for byte in bytes.iter_mut().skip(100) {
        *byte = 0xff;
    }
    std::fs::write(&path, &bytes).expect("write malformed db");

    let db = ViewerDb::open(&path).expect("recreate on malformed pages");
    assert!(
        !db.collapsed_groups()
            .expect("collapsed groups")
            .contains("project:/doomed"),
        "a malformed file must be replaced by a fresh empty DB"
    );
    db.set_group_collapsed("project:/after-recreate", true)
        .expect("write to recreated db");
    assert!(
        db.collapsed_groups()
            .expect("collapsed groups")
            .contains("project:/after-recreate")
    );
}

#[test]
fn viewer_db_open_under_write_lock_errors_without_destroying_data() {
    let (_dir, path) = temp_db_path();

    // An existing user's viewer.db from before the `settings` table existed, holding the two
    // things a user actually loses if the file is deleted: a resolved spawn pin and a
    // collapsed group. Opening it now runs a genuine CREATE TABLE, which takes a write lock.
    {
        let conn = rusqlite::Connection::open(&path).expect("build pre-settings db");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.execute_batch(
            "CREATE TABLE spawned (\
               id INTEGER PRIMARY KEY,\
               backend TEXT NOT NULL,\
               session_id TEXT,\
               cwd TEXT NOT NULL,\
               pid INTEGER NOT NULL,\
               spawned_at_ms INTEGER NOT NULL\
             );\
             CREATE TABLE collapsed_groups (group_key TEXT PRIMARY KEY);\
             INSERT INTO spawned (backend, session_id, cwd, pid, spawned_at_ms) \
               VALUES ('codex', 'pinned-sess', '/p/pinned', 4242, 1000);\
             INSERT INTO collapsed_groups (group_key) VALUES ('project:/p/pinned');",
        )
        .expect("seed pre-settings db");
        conn.close().expect("close seed writer");
    }

    // A second viewer holds the write lock, deterministically: BEGIN IMMEDIATE takes it up
    // front, and the uncommitted INSERT proves it is genuinely held.
    let blocker = rusqlite::Connection::open(&path).expect("open blocking connection");
    blocker
        .execute_batch(
            "BEGIN IMMEDIATE;\
             INSERT INTO collapsed_groups (group_key) VALUES ('project:/blocker');",
        )
        .expect("hold write lock");

    let error = match ViewerDb::open(&path) {
        Ok(_) => panic!("open must fail while another connection holds the write lock"),
        Err(error) => error,
    };
    match &error {
        agent_viewer_core::error::Error::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) => {
            assert!(
                matches!(
                    failure.code,
                    rusqlite::ffi::ErrorCode::DatabaseBusy
                        | rusqlite::ffi::ErrorCode::DatabaseLocked
                ),
                "expected a busy/locked failure, got {failure:?}"
            );
        }
        other => panic!("expected a sqlite busy/locked failure, got {other:?}"),
    }
    assert!(
        path.exists(),
        "the losing open must not unlink the database"
    );

    // Release the lock and roll the blocker's write back; the winner's data must be intact.
    blocker.execute_batch("ROLLBACK;").expect("release lock");
    blocker.close().expect("close blocking connection");

    let db = ViewerDb::open(&path).expect("reopen after the lock is released");
    let state = db.viewer_state().expect("viewer state");
    assert!(
        state
            .pinned
            .contains(&(BackendKind::Codex, "pinned-sess".to_string())),
        "the spawn pin must survive a lost lock race"
    );
    assert_eq!(
        state
            .spawn_pids
            .get(&(BackendKind::Codex, "pinned-sess".to_string())),
        Some(&4242)
    );
    assert!(
        db.collapsed_groups()
            .expect("collapsed groups")
            .contains("project:/p/pinned"),
        "the collapsed group must survive a lost lock race"
    );
}

#[test]
fn viewer_db_open_drops_legacy_tables_and_keeps_supported_data() {
    let (_dir, path) = temp_db_path();

    // A pre-T012 viewer.db: the legacy shadow-state tables with rows in them, alongside the
    // still-supported spawned and collapsed_groups data.
    {
        let conn = rusqlite::Connection::open(&path).expect("build legacy db");
        conn.execute_batch(
            "CREATE TABLE spawned (\
               id INTEGER PRIMARY KEY,\
               backend TEXT NOT NULL,\
               session_id TEXT,\
               cwd TEXT NOT NULL,\
               pid INTEGER NOT NULL,\
               spawned_at_ms INTEGER NOT NULL\
             );\
             CREATE TABLE collapsed_groups (group_key TEXT PRIMARY KEY);\
             CREATE TABLE stopped (\
               backend TEXT NOT NULL,\
               session_id TEXT NOT NULL,\
               stopped_at_ms INTEGER NOT NULL,\
               PRIMARY KEY(backend, session_id)\
             );\
             CREATE TABLE renames (\
               backend TEXT NOT NULL,\
               session_id TEXT NOT NULL,\
               name TEXT NOT NULL,\
               PRIMARY KEY(backend, session_id)\
             );\
             INSERT INTO spawned (backend, session_id, cwd, pid, spawned_at_ms) \
               VALUES ('codex', 'legacy-sess', '/p/legacy', 4242, 1000);\
             INSERT INTO collapsed_groups (group_key) VALUES ('project:/p/legacy');\
             INSERT INTO stopped (backend, session_id, stopped_at_ms) \
               VALUES ('codex', 'legacy-sess', 2000);\
             INSERT INTO renames (backend, session_id, name) \
               VALUES ('codex', 'legacy-sess', 'my old label');",
        )
        .expect("seed legacy db");
        conn.close().expect("close legacy writer");
    }

    {
        let db = ViewerDb::open(&path).expect("open legacy viewer db");

        // The recreate path must NOT have fired: supported state survives the migration.
        let state = db.viewer_state().expect("viewer state");
        assert!(
            state
                .pinned
                .contains(&(BackendKind::Codex, "legacy-sess".to_string())),
            "spawned row must survive the legacy-table migration"
        );
        assert_eq!(
            state
                .spawn_pids
                .get(&(BackendKind::Codex, "legacy-sess".to_string())),
            Some(&4242)
        );
        assert!(
            db.collapsed_groups()
                .expect("collapsed groups")
                .contains("project:/p/legacy"),
            "collapsed group must survive the legacy-table migration"
        );
    }

    // Both shadow-state tables are gone from the file.
    let conn = rusqlite::Connection::open(&path).expect("reopen migrated db");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("prepare table list");
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query table list")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table list");
    assert!(!tables.iter().any(|name| name == "renames"), "{tables:?}");
    assert!(!tables.iter().any(|name| name == "stopped"), "{tables:?}");
    assert!(tables.iter().any(|name| name == "spawned"), "{tables:?}");
    assert!(
        tables.iter().any(|name| name == "collapsed_groups"),
        "{tables:?}"
    );
}

/// A daemon-hosted codex row must come out of the overlay with `pid: None`. Its pid is None by
/// design (the app-server holds the fd, and its pid belongs to every other thread it hosts) and
/// it is never "finished" while the daemon keeps hosting it, so it matches this overlay's
/// fill-the-hole condition exactly. Filling it would hand `stop`/`remove` a pid that SIGTERMs
/// the daemon and every session in it.
#[test]
fn apply_viewer_state_never_puts_a_pid_back_on_a_daemon_hosted_row() {
    let mut hosted = sess(BackendKind::Codex, "hosted", "/p", 10, Status::Working);
    hosted.daemon_hosted = true;
    let ordinary = sess(BackendKind::Codex, "ordinary", "/p", 20, Status::Working);
    let mut sessions = vec![hosted, ordinary];

    let spawn_pids = HashMap::from([
        ((BackendKind::Codex, "hosted".to_string()), 9999),
        ((BackendKind::Codex, "ordinary".to_string()), 4242),
    ]);
    let state = ViewerState {
        pinned: HashSet::new(),
        spawn_pids,
    };

    apply_viewer_state(&mut sessions, &state);

    assert_eq!(sessions[0].pid, None, "daemon-hosted row must stay pidless");
    assert_eq!(
        sessions[1].pid,
        Some(4242),
        "an ordinary viewer-spawned row still gets its pid back"
    );
}

#[test]
fn apply_viewer_state_preserves_backend_titles() {
    let mut pinned = sess(BackendKind::Codex, "pinned", "/p", 10, Status::Idle);
    pinned.title = "Backend Pinned Title".to_string();
    let mut unpinned = sess(BackendKind::Claude, "unpinned", "/p", 20, Status::Working);
    unpinned.title = "Backend Unpinned Title".to_string();
    let mut spawned = sess(BackendKind::Claude, "spawned", "/p", 30, Status::Working);
    spawned.title = "Backend Spawn Title".to_string();
    let mut sessions = vec![pinned, unpinned, spawned];

    let mut pinned = HashSet::new();
    pinned.insert((BackendKind::Codex, "pinned".to_string()));
    let mut spawn_pids = HashMap::new();
    spawn_pids.insert((BackendKind::Claude, "spawned".to_string()), 555);
    let state = ViewerState { pinned, spawn_pids };

    apply_viewer_state(&mut sessions, &state);

    assert_eq!(sessions[0].title, "Backend Pinned Title");
    assert_eq!(sessions[1].title, "Backend Unpinned Title");
    assert_eq!(sessions[2].title, "Backend Spawn Title");
}

#[test]
fn apply_viewer_state_preserves_all_backend_statuses() {
    let expected = vec![
        Status::Working,
        Status::NeedsInput {
            reason: Some("awaiting approval".to_string()),
        },
        Status::Idle,
        Status::Done,
        Status::Error,
        Status::Unknown,
    ];
    let mut sessions = expected
        .iter()
        .enumerate()
        .map(|(index, status)| {
            sess(
                BackendKind::Codex,
                &format!("status-{index}"),
                "/p",
                index as i64,
                status.clone(),
            )
        })
        .collect::<Vec<_>>();
    let pinned = sessions
        .iter()
        .map(|session| (session.backend, session.id.clone()))
        .collect();
    let spawn_pids = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            (
                (session.backend, session.id.clone()),
                u32::try_from(index + 100).unwrap(),
            )
        })
        .collect();
    let state = ViewerState { pinned, spawn_pids };

    apply_viewer_state(&mut sessions, &state);

    let actual = sessions
        .iter()
        .map(|session| session.status.clone())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn apply_viewer_state_preserves_backend_hidden_flags() {
    let mut hidden = sess(BackendKind::Codex, "hidden", "/p", 10, Status::Working);
    hidden.hidden = true;
    let visible = sess(BackendKind::Codex, "visible", "/p", 20, Status::Working);
    let mut sessions = vec![hidden, visible];

    let pinned = HashSet::from([(BackendKind::Codex, "hidden".to_string())]);
    let spawn_pids = HashMap::from([((BackendKind::Codex, "visible".to_string()), 555)]);
    let state = ViewerState { pinned, spawn_pids };

    apply_viewer_state(&mut sessions, &state);

    assert!(sessions[0].hidden);
    assert!(!sessions[1].hidden);
}

#[test]
fn apply_viewer_state_clears_companion_only_for_pinned_session() {
    let mut pinned = sess(BackendKind::Codex, "pinned", "/p", 10, Status::Idle);
    pinned.companion = true;
    let mut unpinned = sess(BackendKind::Codex, "unpinned", "/p", 20, Status::Idle);
    unpinned.companion = true;
    let mut sessions = vec![pinned, unpinned];

    let pinned = HashSet::from([(BackendKind::Codex, "pinned".to_string())]);
    let state = ViewerState {
        pinned,
        spawn_pids: HashMap::new(),
    };

    apply_viewer_state(&mut sessions, &state);

    assert!(!sessions[0].companion);
    assert!(sessions[1].companion);
}

#[test]
fn apply_viewer_state_skips_pid_on_terminal_sessions() {
    let mut has_pid = sess(BackendKind::Claude, "withpid", "/p", 60, Status::Working);
    has_pid.pid = Some(999);
    let mut sessions = vec![
        sess(BackendKind::Claude, "working", "/p", 10, Status::Working),
        sess(
            BackendKind::Claude,
            "needs-input",
            "/p",
            20,
            Status::NeedsInput {
                reason: Some("permission required".to_string()),
            },
        ),
        sess(BackendKind::Claude, "idle", "/p", 30, Status::Idle),
        sess(BackendKind::Claude, "done", "/p", 40, Status::Done),
        sess(BackendKind::Claude, "error", "/p", 50, Status::Error),
        sess(BackendKind::Claude, "unknown", "/p", 60, Status::Unknown),
        has_pid,
    ];
    let mut spawn_pids = HashMap::new();
    for id in [
        "working",
        "needs-input",
        "idle",
        "done",
        "error",
        "unknown",
        "withpid",
    ] {
        spawn_pids.insert((BackendKind::Claude, id.to_string()), 777);
    }
    let state = ViewerState {
        pinned: HashSet::new(),
        spawn_pids,
    };

    apply_viewer_state(&mut sessions, &state);

    assert_eq!(sessions[0].pid, Some(777));
    assert_eq!(sessions[1].pid, Some(777));
    assert_eq!(sessions[2].pid, Some(777));
    // Terminal sessions never take the recorded pid because it may have been reused.
    assert_eq!(sessions[3].pid, None);
    assert_eq!(sessions[4].pid, None);
    // Unknown is not terminal, so the recorded pid is still safe to hand out.
    assert_eq!(sessions[5].pid, Some(777));
    // An existing backend pid is never overwritten.
    assert_eq!(sessions[6].pid, Some(999));
}

#[test]
fn prune_resolved_missing_keeps_live_and_young_prunes_old_dead() {
    let (_dir, path) = temp_db_path();
    let now = agent_viewer_core::spawn::now_ms();
    let eight_days = 8 * 24 * 60 * 60 * 1000;
    let db = ViewerDb::open(&path).expect("open viewer db");

    // Old and live: must survive despite its age.
    let old_live = db
        .record_spawn(
            BackendKind::Codex,
            &PathBuf::from("/p/live"),
            1,
            now - eight_days,
        )
        .expect("record old-live");
    db.resolve_spawn(old_live, "old-live-sess")
        .expect("resolve");
    // Old and dead: the only row that should be pruned.
    let old_dead = db
        .record_spawn(
            BackendKind::Codex,
            &PathBuf::from("/p/dead"),
            2,
            now - eight_days,
        )
        .expect("record old-dead");
    db.resolve_spawn(old_dead, "old-dead-sess")
        .expect("resolve");
    // Young and dead: survives while it remains under the cutoff.
    let young_dead = db
        .record_spawn(BackendKind::Codex, &PathBuf::from("/p/young"), 3, now)
        .expect("record young-dead");
    db.resolve_spawn(young_dead, "young-dead-sess")
        .expect("resolve");

    // Only old-live-sess is present in the live set.
    let mut live = HashSet::new();
    live.insert((BackendKind::Codex, "old-live-sess".to_string()));
    db.prune_resolved_missing(&live).expect("prune");

    let pinned = db.viewer_state().expect("viewer state").pinned;
    assert!(
        pinned.contains(&(BackendKind::Codex, "old-live-sess".to_string())),
        "old row whose session is still live must keep its pin"
    );
    assert!(
        !pinned.contains(&(BackendKind::Codex, "old-dead-sess".to_string())),
        "old row absent from live must be pruned"
    );
    assert!(
        pinned.contains(&(BackendKind::Codex, "young-dead-sess".to_string())),
        "young row must survive even when absent from live"
    );
}

#[test]
fn match_spawn_window_and_nearest() {
    let record = SpawnRecord {
        rowid: 1,
        backend: BackendKind::Codex,
        cwd: PathBuf::from("/home/user/match-proj"),
        pid: 4242,
        spawned_at_ms: 1_000_000,
    };

    // In-window, correct cwd and backend gives a match.
    let one = vec![sess(
        BackendKind::Codex,
        "in-window",
        "/home/user/match-proj",
        1_005_000,
        Status::Working,
    )];
    assert_eq!(match_spawn(&record, &one), Some("in-window".to_string()));

    // Two candidates choose the nearest creation time.
    let two = vec![
        sess(
            BackendKind::Codex,
            "far",
            "/home/user/match-proj",
            1_020_000,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "near",
            "/home/user/match-proj",
            1_002_000,
            Status::Working,
        ),
    ];
    assert_eq!(match_spawn(&record, &two), Some("near".to_string()));

    // Outside the window, wrong cwd, and wrong backend all miss.
    let miss = vec![
        sess(
            BackendKind::Codex,
            "too-late",
            "/home/user/match-proj",
            1_040_000,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "wrong-cwd",
            "/home/user/other",
            1_005_000,
            Status::Working,
        ),
        sess(
            BackendKind::Claude,
            "wrong-backend",
            "/home/user/match-proj",
            1_005_000,
            Status::Working,
        ),
    ];
    assert_eq!(match_spawn(&record, &miss), None);
}

/// A routed spawn is matched over the INTERVAL the session could have been created in (the router
/// invocation through its return), not one instant. The router keeps polling for the winning
/// backend's job id for seconds after the job already exists, so the row it must select predates
/// the return stamp by far more than the 2s of backward slack a single stamp reaches. The interval
/// keeps that same slack on each side, so nothing created outside the router call gets in.
#[test]
fn match_spawn_between_spans_the_whole_routing_interval() {
    let cwd = PathBuf::from("/home/user/routed-proj");
    let invoked_at = 1_000_000;
    let returned_at = 1_010_000;

    let created_while_routing = vec![sess(
        BackendKind::Codex,
        "routed",
        "/home/user/routed-proj",
        // 5s before the router returned: outside a single stamp's window, inside the interval.
        1_005_000,
        Status::Working,
    )];
    assert_eq!(
        match_spawn_between(
            BackendKind::Codex,
            &cwd,
            invoked_at,
            returned_at,
            &created_while_routing
        ),
        Some("routed".to_string())
    );

    let outside = vec![
        sess(
            BackendKind::Codex,
            "before-the-router-ran",
            "/home/user/routed-proj",
            invoked_at - 2_001,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "long-after-it-returned",
            "/home/user/routed-proj",
            returned_at + 30_001,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "wrong-cwd",
            "/home/user/other",
            1_005_000,
            Status::Working,
        ),
        sess(
            BackendKind::Claude,
            "wrong-backend",
            "/home/user/routed-proj",
            1_005_000,
            Status::Working,
        ),
    ];
    assert_eq!(
        match_spawn_between(BackendKind::Codex, &cwd, invoked_at, returned_at, &outside),
        None
    );
}

#[test]
fn match_spawn_between_normalizes_a_reversed_clock_interval() {
    let cwd = PathBuf::from("/home/user/routed-proj");
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "created-before-clock-jump",
            "/home/user/routed-proj",
            1_009_000,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "outside-the-normalized-window",
            "/home/user/routed-proj",
            999_000 - 2_001,
            Status::Working,
        ),
    ];

    assert_eq!(
        match_spawn_between(BackendKind::Codex, &cwd, 1_010_000, 1_000_000, &sessions),
        Some("created-before-clock-jump".to_string())
    );
}

#[test]
fn viewer_db_collapsed_groups_roundtrip() {
    let (_dir, path) = temp_db_path();

    {
        let db = ViewerDb::open(&path).expect("open viewer db");

        // Insert two collapsed keys. The TUI owns the meaning of these opaque strings.
        db.set_group_collapsed("project:/a", true)
            .expect("set project");
        db.set_group_collapsed("state:done", true)
            .expect("set state");

        let collapsed = db.collapsed_groups().expect("collapsed groups");
        assert!(collapsed.contains("project:/a"));
        assert!(collapsed.contains("state:done"));

        // Clearing one removes only that key.
        db.set_group_collapsed("project:/a", false)
            .expect("clear project");
        let collapsed = db.collapsed_groups().expect("collapsed groups");
        assert!(!collapsed.contains("project:/a"));
        assert!(collapsed.contains("state:done"));
    }

    // Reopen the same path: the surviving key persists across a restart.
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    let collapsed = db.collapsed_groups().expect("collapsed groups");
    assert!(collapsed.contains("state:done"));
    assert!(!collapsed.contains("project:/a"));
}

#[test]
fn viewer_db_grouping_mode_defaults_and_roundtrips() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    assert_eq!(
        db.grouping_mode().expect("default grouping"),
        GroupingMode::Project
    );

    db.set_grouping_mode(GroupingMode::State)
        .expect("set grouping");
    assert_eq!(
        db.grouping_mode().expect("read grouping"),
        GroupingMode::State
    );

    drop(db);
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    assert_eq!(
        db.grouping_mode().expect("read persisted grouping"),
        GroupingMode::State
    );

    db.set_grouping_mode(GroupingMode::Project)
        .expect("overwrite grouping");
    assert_eq!(
        db.grouping_mode().expect("read overwritten grouping"),
        GroupingMode::Project
    );
}

#[test]
fn viewer_db_sort_order_defaults_and_roundtrips() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    assert_eq!(db.sort_order().expect("default sort"), SortOrder::Recency);

    db.set_sort_order(SortOrder::Title).expect("set sort");
    assert_eq!(db.sort_order().expect("read sort"), SortOrder::Title);

    drop(db);
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    assert_eq!(
        db.sort_order().expect("read persisted sort"),
        SortOrder::Title
    );

    db.set_sort_order(SortOrder::Recency)
        .expect("overwrite sort");
    assert_eq!(
        db.sort_order().expect("read overwritten sort"),
        SortOrder::Recency
    );
}

#[test]
fn viewer_db_age_ramp_defaults_off_and_roundtrips() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    assert!(
        !db.age_ramp().expect("default age ramp"),
        "the age ramp is opt-in, so an untouched viewer db must read off"
    );

    db.set_age_ramp(true).expect("enable age ramp");
    assert!(db.age_ramp().expect("read age ramp"));

    drop(db);
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    assert!(db.age_ramp().expect("read persisted age ramp"));

    db.set_age_ramp(false).expect("disable age ramp");
    assert!(!db.age_ramp().expect("read disabled age ramp"));
}

#[test]
fn viewer_db_retention_window_defaults_and_roundtrips() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    assert_eq!(
        db.retention_window_ms().expect("default retention"),
        DEFAULT_RETENTION_WINDOW_MS
    );

    db.set_retention_window_ms(86_400_000)
        .expect("set retention");
    assert_eq!(
        db.retention_window_ms().expect("read retention"),
        86_400_000
    );

    drop(db);
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    assert_eq!(
        db.retention_window_ms().expect("read persisted retention"),
        86_400_000
    );

    db.set_retention_window_ms(172_800_000)
        .expect("overwrite retention");
    assert_eq!(
        db.retention_window_ms()
            .expect("read overwritten retention"),
        172_800_000
    );
}

// Model-catalog cache: a backend's discovered model ids survive a viewer restart, so the
// composer's picker is populated instantly instead of waiting on a multi-second CLI probe.
// The DB is dumb storage here - it stamps the fetch time and hands it back; whether that is
// stale is the caller's policy.

#[test]
fn cached_models_round_trip_survives_reopen() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    let models = vec![
        "default".to_string(),
        "zai/kimi-k3".to_string(),
        "anthropic/claude-opus-5".to_string(),
    ];

    db.set_cached_models(BackendKind::Claude, &models, 1_000)
        .expect("write model cache");

    drop(db);
    let db = ViewerDb::open(&path).expect("reopen viewer db");
    let hit = db
        .cached_models(BackendKind::Claude)
        .expect("read model cache")
        .expect("row present");
    assert_eq!(hit.models, models);
    assert_eq!(hit.fetched_at_ms, 1_000);
}

#[test]
fn cached_models_absent_backend_is_none() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    db.set_cached_models(BackendKind::Claude, &["default".to_string()], 1_000)
        .expect("write model cache");

    // Another backend's row is not a hit; discovery still has to run for it.
    assert!(
        db.cached_models(BackendKind::Codex)
            .expect("read model cache")
            .is_none()
    );
}

#[test]
fn cached_models_overwrite_replaces_previous_list() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    db.set_cached_models(BackendKind::Claude, &["default".to_string()], 1_000)
        .expect("write first list");
    db.set_cached_models(
        BackendKind::Claude,
        &["default".to_string(), "zai/glm-5.2".to_string()],
        2_000,
    )
    .expect("overwrite list");

    let hit = db
        .cached_models(BackendKind::Claude)
        .expect("read model cache")
        .expect("row present");
    assert_eq!(
        hit.models,
        vec!["default".to_string(), "zai/glm-5.2".to_string()]
    );
    assert_eq!(hit.fetched_at_ms, 2_000);
}

#[test]
fn cached_models_preserve_order_and_survive_odd_ids() {
    // Order is the picker's order (default first), so the encoding must not sort or split
    // on characters that appear inside real provider/model ids.
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");
    let models = vec![
        "default".to_string(),
        "zzz/last-alphabetically".to_string(),
        "vendor/big-pickle".to_string(),
        "anthropic/claude-opus-5[1m]".to_string(),
    ];
    db.set_cached_models(BackendKind::Claude, &models, 1_000)
        .expect("write model cache");

    let hit = db
        .cached_models(BackendKind::Claude)
        .expect("read model cache")
        .expect("row present");
    assert_eq!(hit.models, models);
}

use agent_viewer_core::backend::{BackendKind, Session, Status};
use agent_viewer_core::state::{
    SpawnRecord, ViewerDb, ViewerState, apply_viewer_state, match_spawn,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn sess(backend: BackendKind, id: &str, cwd: &str, created_at_ms: i64, status: Status) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        created_at_ms,
        updated_at_ms: created_at_ms,
        status,
        hidden: false,
        source_label: "test".to_string(),
        summary: String::new(),
        companion: false,
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
    }
}

fn temp_db_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("viewer.db");
    (dir, path)
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
fn viewer_db_recreates_on_corrupt() {
    let (_dir, path) = temp_db_path();
    std::fs::write(&path, b"this is not a sqlite database at all").unwrap();

    // A corrupt file is replaced with a fresh empty DB, not an error.
    let db = ViewerDb::open(&path).expect("recreate on corrupt");
    assert_eq!(
        db.viewer_state().expect("viewer state"),
        ViewerState::default()
    );
}

#[test]
fn viewer_db_stopped_and_rename_roundtrip() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    db.mark_stopped(BackendKind::Opencode, "ses_1")
        .expect("mark");
    assert!(
        db.viewer_state()
            .unwrap()
            .stopped
            .contains(&(BackendKind::Opencode, "ses_1".to_string()))
    );
    db.clear_stopped(BackendKind::Opencode, "ses_1")
        .expect("clear");
    assert!(
        !db.viewer_state()
            .unwrap()
            .stopped
            .contains(&(BackendKind::Opencode, "ses_1".to_string()))
    );

    // Codex, not claude: claude overrides are skipped at load (see
    // viewer_state_skips_claude_name_overrides), so only a rename-capable backend
    // exercises the store's set/load round-trip.
    db.set_name_override(BackendKind::Codex, "abc", "My Renamed Session")
        .expect("set name");
    assert_eq!(
        db.viewer_state()
            .unwrap()
            .renames
            .get(&(BackendKind::Codex, "abc".to_string())),
        Some(&"My Renamed Session".to_string())
    );
}

#[test]
fn apply_viewer_state_stops_and_stale() {
    let mut sessions = vec![
        sess(BackendKind::Codex, "failed", "/p", 10, Status::Failed),
        sess(BackendKind::Codex, "done", "/p", 20, Status::Done),
        sess(BackendKind::Codex, "live", "/p", 30, Status::Working),
    ];
    let mut stopped = HashSet::new();
    stopped.insert((BackendKind::Codex, "failed".to_string()));
    stopped.insert((BackendKind::Codex, "done".to_string()));
    stopped.insert((BackendKind::Codex, "live".to_string()));
    let state = ViewerState {
        stopped,
        ..Default::default()
    };

    let stale = apply_viewer_state(&mut sessions, &state);

    // Failed / Done stopped rows fold to Stopped.
    assert_eq!(sessions[0].status, Status::Stopped);
    assert_eq!(sessions[1].status, Status::Stopped);
    // A live session keeps its status and its stopped record is reported stale.
    assert_eq!(sessions[2].status, Status::Working);
    assert!(stale.contains(&(BackendKind::Codex, "live".to_string())));
    assert!(!stale.contains(&(BackendKind::Codex, "failed".to_string())));
}

#[test]
fn apply_viewer_state_pins_renames_pids() {
    let mut companion = sess(BackendKind::Codex, "comp", "/p", 10, Status::Idle);
    companion.companion = true;
    let mut has_pid = sess(BackendKind::Opencode, "withpid", "/p", 20, Status::Working);
    has_pid.pid = Some(999);
    let none_pid = sess(BackendKind::Opencode, "nopid", "/p", 30, Status::Working);
    let mut sessions = vec![companion, has_pid, none_pid];

    let mut pinned = HashSet::new();
    pinned.insert((BackendKind::Codex, "comp".to_string()));
    let mut renames = HashMap::new();
    renames.insert(
        (BackendKind::Codex, "comp".to_string()),
        "Pinned Name".to_string(),
    );
    let mut spawn_pids = HashMap::new();
    spawn_pids.insert((BackendKind::Opencode, "nopid".to_string()), 555);
    spawn_pids.insert((BackendKind::Opencode, "withpid".to_string()), 111);
    let state = ViewerState {
        pinned,
        renames,
        spawn_pids,
        ..Default::default()
    };

    apply_viewer_state(&mut sessions, &state);

    // Pinned session becomes visible (companion cleared) and takes the rename.
    assert!(!sessions[0].companion);
    assert_eq!(sessions[0].title, "Pinned Name");
    // spawn_pid fills a None pid but does not clobber an existing Some.
    assert_eq!(sessions[1].pid, Some(999));
    assert_eq!(sessions[2].pid, Some(555));
}

#[test]
fn prune_resolved_missing_keeps_live_and_young_prunes_old_dead() {
    let (_dir, path) = temp_db_path();
    let now = agent_viewer_core::spawn::now_ms();
    let eight_days = 8 * 24 * 60 * 60 * 1000;
    let db = ViewerDb::open(&path).expect("open viewer db");

    // Old + live (session still exists): must survive despite its age.
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
    // Old + dead (session gone): the only row that should be pruned.
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
    // Young + dead (session gone but row still fresh): survives (under the cutoff).
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
fn clear_name_override_removes_rename() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    // Codex, not claude: a claude key is absent from viewer_state either way, so it could
    // not tell a real clear from the load-time skip.
    db.set_name_override(BackendKind::Codex, "abc", "Local Name")
        .expect("set name");
    db.clear_name_override(BackendKind::Codex, "abc")
        .expect("clear name");

    assert!(
        !db.viewer_state()
            .unwrap()
            .renames
            .contains_key(&(BackendKind::Codex, "abc".to_string())),
        "cleared override must leave no rename for the key"
    );
}

// A legacy claude rename row (written by a build that still allowed the local fallback) must
// never reach the overlay: applying it would show a fabricated title only the viewer believes,
// and Ctrl+R on a claude row is now an unsupported notice, so the user could never clear it.
// The row is left in the table; it is skipped at load.
#[test]
fn viewer_state_skips_claude_name_overrides() {
    let (_dir, path) = temp_db_path();
    let db = ViewerDb::open(&path).expect("open viewer db");

    db.set_name_override(BackendKind::Claude, "legacy", "Fabricated Claude Name")
        .expect("set claude name");
    db.set_name_override(BackendKind::Codex, "live", "Real Codex Name")
        .expect("set codex name");

    let renames = db.viewer_state().expect("viewer state").renames;

    assert!(
        !renames.contains_key(&(BackendKind::Claude, "legacy".to_string())),
        "a persisted claude override must be skipped at load, not applied"
    );
    // The control: a rename-capable backend still loads, so the skip is claude-specific and
    // not an accidental disabling of the whole rename overlay.
    assert_eq!(
        renames.get(&(BackendKind::Codex, "live".to_string())),
        Some(&"Real Codex Name".to_string()),
        "a codex override must still load"
    );
}

#[test]
fn apply_viewer_state_skips_pid_on_terminal_sessions() {
    let mut sessions = vec![
        sess(BackendKind::Opencode, "done", "/p", 10, Status::Done),
        sess(BackendKind::Opencode, "failed", "/p", 20, Status::Failed),
        sess(BackendKind::Opencode, "stopped", "/p", 30, Status::Stopped),
        sess(BackendKind::Opencode, "live", "/p", 40, Status::Working),
    ];
    let mut spawn_pids = HashMap::new();
    for id in ["done", "failed", "stopped", "live"] {
        spawn_pids.insert((BackendKind::Opencode, id.to_string()), 777);
    }
    let state = ViewerState {
        spawn_pids,
        ..Default::default()
    };

    apply_viewer_state(&mut sessions, &state);

    // Terminal sessions never take the recorded pid (it may be reused by another process).
    assert_eq!(sessions[0].pid, None);
    assert_eq!(sessions[1].pid, None);
    assert_eq!(sessions[2].pid, None);
    // A live session still gets the overlay (opencode stop path).
    assert_eq!(sessions[3].pid, Some(777));
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

    // In-window, correct cwd + backend -> matched.
    let one = vec![sess(
        BackendKind::Codex,
        "in-window",
        "/home/user/match-proj",
        1_005_000,
        Status::Working,
    )];
    assert_eq!(match_spawn(&record, &one), Some("in-window".to_string()));

    // Two candidates -> nearest created_at wins (1_002_000 beats 1_020_000).
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

    // Outside window, wrong cwd, and wrong backend all miss.
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
            BackendKind::Opencode,
            "wrong-backend",
            "/home/user/match-proj",
            1_005_000,
            Status::Working,
        ),
    ];
    assert_eq!(match_spawn(&record, &miss), None);
}

#[test]
fn viewer_db_collapsed_groups_roundtrip() {
    let (_dir, path) = temp_db_path();

    {
        let db = ViewerDb::open(&path).expect("open viewer db");

        // Insert two collapsed keys (opaque strings; the TUI owns their meaning).
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

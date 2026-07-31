mod common;

use agent_viewer_core::backend::Status;
use agent_viewer_core::codex::status::{FdSignal, RolloutOwner, StatusResolver, resolve_status};
use std::io::Write;
use std::path::PathBuf;

fn closed() -> FdSignal {
    FdSignal::Closed
}

fn unavailable() -> FdSignal {
    FdSignal::Unavailable
}

/// The fd is held by the session's OWN codex process: an ordinary killable pid.
fn held_by_session() -> FdSignal {
    FdSignal::Held(RolloutOwner {
        pid: 4242,
        daemon: false,
    })
}

/// The fd is held by the shared `codex app-server` daemon.
fn held_by_daemon() -> FdSignal {
    FdSignal::Held(RolloutOwner {
        pid: 9999,
        daemon: true,
    })
}

#[test]
fn failed_when_file_missing() {
    // Missing file, empty map: Failed, never panics.
    let path = PathBuf::from("/nonexistent/rollout-does-not-exist.jsonl");
    assert_eq!(resolve_status(&path, closed(), false), Status::Error);
}

#[test]
fn resolver_reresolves_when_session_closes_without_file_change() {
    // Regression: status depends on the open/closed state, not just (mtime, len). An
    // open+complete rollout resolves Idle; when the owning process exits with the file
    // UNCHANGED it must resolve Done, never the cached Idle.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_complete.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(
        resolver.resolve(&path, held_by_session(), false).0,
        Status::Idle
    );
    assert_eq!(resolver.resolve(&path, closed(), false).0, Status::Done);
}

#[test]
fn resolver_matches_pure_and_recomputes_on_change() {
    // The cache wrapper agrees with the pure function across states...
    let complete = common::fixture_path("rollout_complete.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let missing = PathBuf::from("/nonexistent/x.jsonl");
    let mut resolver = StatusResolver::new();
    for p in [&complete, &midturn, &missing] {
        assert_eq!(
            resolver.resolve(p, closed(), false).0,
            resolve_status(p, closed(), false)
        );
    }

    // ...and invalidates its (mtime, len) cache when the file changes.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_midturn.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(resolver.resolve(&path, closed(), false).0, Status::Error);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t","completed_at":1,"duration_ms":1}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(resolver.resolve(&path, closed(), false).0, Status::Done);
}

// --- who holds the fd: the session's own process, or the shared app-server daemon ---

#[test]
fn resolver_returns_the_pid_when_the_sessions_own_process_holds_the_fd() {
    let path = common::fixture_path("rollout_midturn.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(
        resolver.resolve(&path, held_by_session(), false),
        (Status::Working, Some(4242), false)
    );
}

/// THE GUARD plus the daemon rule table. The `codex app-server` daemon holds the rollout fd for
/// every thread it hosts, so the /proc/<pid>/fd scan reports the DAEMON's pid for those rows.
/// Returning it as the session's pid would let `stop` SIGTERM the daemon and take down every
/// other session it hosts, so the resolver must return None and say daemon_hosted instead.
///
/// The tail rules for those rows are NOT the fd rules, because the daemon never closes the fd
/// (observed still held 20+ minutes past completion), so the fd carries no liveness at all:
///   MidTurn or unreadable -> Working, AwaitingApproval -> NeedsInput,
///   Complete -> Done (Idle only while a client is attached; covered by the next test).
#[test]
fn resolver_withholds_the_daemon_pid_and_flags_the_row_daemon_hosted() {
    let cases = [
        ("rollout_midturn.jsonl", Status::Working),
        ("rollout_complete.jsonl", Status::Done),
        (
            "rollout_approval.jsonl",
            Status::NeedsInput {
                reason: Some("awaiting approval".to_string()),
            },
        ),
    ];
    for (fixture, expected) in cases {
        let path = common::fixture_path(fixture);
        let mut resolver = StatusResolver::new();
        assert_eq!(
            resolver.resolve(&path, held_by_daemon(), false),
            (expected.clone(), None, true),
            "{fixture} under a daemon-held fd"
        );
        // The pure entry point sees the same status; only the pid/host answer is new.
        assert_eq!(
            resolve_status(&path, held_by_daemon(), false),
            expected,
            "{fixture} via resolve_status"
        );
    }
}

/// The regression this table exists for: a finished background task must reach Done. Under the
/// fd rule a daemon-hosted row sat at Idle forever, because the daemon holds every hosted
/// thread's rollout open indefinitely, so "working" and "finished" became indistinguishable for
/// exactly the sessions the viewer spawns. A test that only checked the non-daemon rules would
/// pass with the daemon branch deleted; this one fails.
#[test]
fn a_finished_daemon_hosted_row_reaches_done_even_though_the_fd_is_still_open() {
    let path = common::fixture_path("rollout_complete.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(
        resolver.resolve(&path, held_by_daemon(), false),
        (Status::Done, None, true)
    );
    // The same file under the session's OWN process is still Idle: that fd does mean "alive".
    assert_eq!(
        resolver.resolve(&path, held_by_session(), false),
        (Status::Idle, Some(4242), false)
    );
}

/// The refinement: a human sitting in the thread (a `codex resume --remote ... <id>` client)
/// must not be reported as finished. Only the Complete case moves; a live turn stays Working
/// and an approval stays NeedsInput whether or not somebody is watching.
#[test]
fn an_attached_client_holds_a_daemon_hosted_row_at_idle_instead_of_done() {
    let complete = common::fixture_path("rollout_complete.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(
        resolver.resolve(&complete, held_by_daemon(), true),
        (Status::Idle, None, true)
    );

    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let approval = common::fixture_path("rollout_approval.jsonl");
    assert_eq!(
        resolver.resolve(&midturn, held_by_daemon(), true),
        (Status::Working, None, true)
    );
    assert_eq!(
        resolver.resolve(&approval, held_by_daemon(), true),
        (
            Status::NeedsInput {
                reason: Some("awaiting approval".to_string())
            },
            None,
            true
        )
    );
}

/// The spawn race is preserved on the daemon path: the registry row can appear before the
/// rollout has any parsable tail, and that must read Working, not Error or Done.
#[test]
fn a_daemon_hosted_row_with_no_readable_tail_is_working() {
    let path = PathBuf::from("/nonexistent/rollout-not-written-yet.jsonl");
    let mut resolver = StatusResolver::new();
    for attached in [false, true] {
        assert_eq!(
            resolver.resolve(&path, held_by_daemon(), attached),
            (Status::Working, None, true),
            "attached={attached}"
        );
    }
}

/// Rows the daemon does not host keep TODAY's rules exactly, and `attached` never touches them:
/// their fd is their own process's, so it is still the liveness signal.
#[test]
fn the_attached_flag_changes_nothing_for_a_row_the_daemon_does_not_host() {
    let complete = common::fixture_path("rollout_complete.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let approval = common::fixture_path("rollout_approval.jsonl");
    let awaiting = Status::NeedsInput {
        reason: Some("awaiting approval".to_string()),
    };
    let cases = [
        (&complete, true, Status::Idle),
        (&complete, false, Status::Done),
        (&midturn, true, Status::Working),
        (&midturn, false, Status::Error),
        (&approval, true, awaiting.clone()),
        (&approval, false, Status::Error),
    ];
    for (path, is_held, expected) in cases {
        let evidence = if is_held { held_by_session() } else { closed() };
        for attached in [false, true] {
            assert_eq!(
                resolve_status(path, evidence, attached),
                expected,
                "{path:?} held={is_held} attached={attached}"
            );
        }
    }
}

#[test]
fn resolver_reports_no_pid_and_no_daemon_host_when_the_fd_is_closed() {
    let path = common::fixture_path("rollout_complete.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(
        resolver.resolve(&path, closed(), false),
        (Status::Done, None, false)
    );
}

#[test]
fn unavailable_process_evidence_is_not_observed_closed() {
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    assert_eq!(
        resolve_status(&midturn, unavailable(), false),
        Status::Unknown
    );
    assert_eq!(resolve_status(&midturn, closed(), false), Status::Error);
}

#[test]
fn unavailable_process_evidence_reports_only_proven_completion() {
    let complete = common::fixture_path("rollout_complete.jsonl");
    let approval = common::fixture_path("rollout_approval.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let missing = PathBuf::from("/nonexistent/rollout-not-written-yet.jsonl");

    assert_eq!(
        resolve_status(&complete, unavailable(), false),
        Status::Done
    );
    for path in [&approval, &midturn, &missing] {
        assert_eq!(
            resolve_status(path, unavailable(), false),
            Status::Unknown,
            "{path:?} is not proven complete"
        );
    }
}

#[test]
fn unavailable_process_evidence_never_claims_a_pid_or_daemon() {
    let complete = common::fixture_path("rollout_complete.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let mut resolver = StatusResolver::new();

    assert_eq!(
        resolver.resolve(&complete, unavailable(), false),
        (Status::Done, None, false)
    );
    assert_eq!(
        resolver.resolve(&midturn, unavailable(), false),
        (Status::Unknown, None, false)
    );
}

/// PRODUCTION PATH COVERAGE. Every rule above is asserted through the pure `resolve`, but
/// `list` calls `resolve_scanned`, which canonicalizes the row's path and looks the fd signal up
/// in the sweep itself. That step was untested: forcing `fd_signal` to `Unavailable` left the
/// whole suite green while the running/done distinction collapsed on the real listing.
#[test]
fn resolve_scanned_reads_the_fd_signal_out_of_the_scan() {
    use agent_viewer_core::codex::status::CodexProcessScan;
    use std::collections::{HashMap, HashSet};

    // A real file on disk: `resolve_scanned` canonicalizes before the lookup, so the scan's key
    // has to be the canonical path, exactly as the /proc/<pid>/fd sweep records it.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_complete.jsonl");
    let canonical = std::fs::canonicalize(&path).expect("canonicalize the rollout");
    let mut resolver = StatusResolver::new();

    let held = CodexProcessScan::from_evidence(
        HashMap::from([(
            canonical,
            RolloutOwner {
                pid: 4242,
                daemon: false,
            },
        )]),
        HashSet::new(),
        true,
    );
    assert_eq!(
        resolver.resolve_scanned(&path, &held, false),
        (Status::Idle, Some(4242), false),
        "a rollout the sweep found open is a LIVE session between turns"
    );

    // Same file, same resolver (so the tail cache is warm): nothing holds it now.
    let closed_scan = CodexProcessScan::from_evidence(HashMap::new(), HashSet::new(), true);
    assert_eq!(
        resolver.resolve_scanned(&path, &closed_scan, false),
        (Status::Done, None, false),
        "no holder plus a complete tail is Done"
    );

    // Only a platform without procfs may report unavailable, and it must not claim a pid.
    let blind = CodexProcessScan::from_evidence(HashMap::new(), HashSet::new(), false);
    assert_eq!(
        resolver.resolve_scanned(&path, &blind, false),
        (Status::Done, None, false)
    );
    let (_dir, midturn) = common::copy_fixture_to_temp("rollout_midturn.jsonl");
    assert_eq!(
        resolver.resolve_scanned(&midturn, &blind, false),
        (Status::Unknown, None, false),
        "an unreadable process table proves nothing about a mid-turn rollout"
    );
}

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
};

use agent_viewer_core::{
    BackendKind, Error, ListingCacheScope, Session, SessionOrigin, Status, ViewerDb,
};
use agent_viewer_tui::shared_listing::{
    CachedPaletteAction, RefreshCursor, RefreshOutcome, TargetRequest, TargetResolution,
    dispatch_cached_palette, refresh_scoped, resolve_target,
};
use tempfile::TempDir;

fn open_viewers() -> (TempDir, ViewerDb, ViewerDb) {
    let directory = tempfile::tempdir().expect("temporary viewer database directory");
    let path = directory.path().join("viewer.sqlite");
    let first = ViewerDb::open_at(&path).expect("first viewer database");
    let second = ViewerDb::open_at(&path).expect("second viewer database");
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
    let calls = Cell::new(0);
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    let initial = refresh_scoped(&first, Some(&scope), &mut first_cursor, 1_000, || {
        calls.set(calls.get() + 1);
        Ok(vec![expected.clone()])
    });
    let shared = refresh_scoped(&second, Some(&scope), &mut second_cursor, 2_999, || {
        calls.set(calls.get() + 1);
        Ok(Vec::<Session>::new())
    });

    assert!(matches!(initial, RefreshOutcome::Authoritative { .. }));
    assert!(matches!(shared, RefreshOutcome::Shared { sessions } if sessions == vec![expected]));
    assert_eq!(calls.get(), 1);
}

#[test]
fn unchanged_shared_generation_skips_json_decode_and_authoritative_source() {
    let (_directory, publisher, follower) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let expected = session(BackendKind::Codex, "thread_123", Some(101), false);
    let calls = Cell::new(0);
    let mut publisher_cursor = RefreshCursor::default();
    let mut follower_cursor = RefreshCursor::default();

    let published = refresh_scoped(
        &publisher,
        Some(&scope),
        &mut publisher_cursor,
        1_000,
        || {
            calls.set(calls.get() + 1);
            Ok(vec![expected.clone()])
        },
    );
    let loaded = refresh_scoped(
        &follower,
        Some(&scope),
        &mut follower_cursor,
        2_000,
        || {
            calls.set(calls.get() + 1);
            Ok(Vec::<Session>::new())
        },
    );
    let unchanged = refresh_scoped(
        &follower,
        Some(&scope),
        &mut follower_cursor,
        2_001,
        || {
            calls.set(calls.get() + 1);
            Ok(Vec::<Session>::new())
        },
    );

    assert!(
        matches!(published, RefreshOutcome::Authoritative { sessions } if sessions == vec![expected.clone()])
    );
    assert!(
        matches!(loaded, RefreshOutcome::Shared { sessions } if sessions == vec![expected])
    );
    assert!(matches!(unchanged, RefreshOutcome::Unchanged));
    assert_eq!(calls.get(), 1);
}

#[test]
fn expired_snapshot_lists_once_then_the_second_viewer_uses_the_publication() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Claude);
    let before_expiry = session(BackendKind::Claude, "agent_123", Some(201), false);
    let after_expiry = session(BackendKind::Claude, "agent_123", Some(202), false);
    let calls = Cell::new(0);
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    refresh_scoped(
        &first,
        Some(&scope),
        &mut first_cursor,
        1_000,
        || Ok(vec![before_expiry]),
    );
    let refreshed = refresh_scoped(&second, Some(&scope), &mut second_cursor, 3_000, || {
        calls.set(calls.get() + 1);
        Ok(vec![after_expiry.clone()])
    });
    let shared = refresh_scoped(&first, Some(&scope), &mut first_cursor, 3_001, || {
        calls.set(calls.get() + 1);
        Ok(Vec::<Session>::new())
    });

    assert!(
        matches!(refreshed, RefreshOutcome::Authoritative { sessions } if sessions == vec![after_expiry.clone()])
    );
    assert!(
        matches!(shared, RefreshOutcome::Shared { sessions } if sessions == vec![after_expiry])
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn missing_or_corrupt_cache_falls_back_to_the_authoritative_source() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Opencode);
    let calls = Cell::new(0);
    let expected = session(BackendKind::Opencode, "ses_123", None, true);
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    let missing = refresh_scoped(&first, Some(&scope), &mut first_cursor, 1_000, || {
        calls.set(calls.get() + 1);
        Ok(vec![expected.clone()])
    });
    second
        .overwrite_listing_snapshot_json(&scope, "not valid json", 1_001)
        .expect("corrupt cached snapshot");
    let corrupt = refresh_scoped(&second, Some(&scope), &mut second_cursor, 2_001, || {
        calls.set(calls.get() + 1);
        Ok(vec![expected.clone()])
    });

    assert!(
        matches!(missing, RefreshOutcome::Authoritative { sessions } if sessions == vec![expected.clone()])
    );
    assert!(
        matches!(corrupt, RefreshOutcome::Authoritative { sessions } if sessions == vec![expected])
    );
    assert_eq!(calls.get(), 2);
}

#[test]
fn successful_empty_listing_replaces_rows_and_source_error_keeps_last_good_notice() {
    let (_directory, first, second) = open_viewers();
    let scope = scope(BackendKind::Codex);
    let stale_row = session(BackendKind::Codex, "thread_123", Some(101), false);
    let mut first_cursor = RefreshCursor::default();
    let mut second_cursor = RefreshCursor::default();

    refresh_scoped(
        &first,
        Some(&scope),
        &mut first_cursor,
        1_000,
        || Ok(vec![stale_row]),
    );
    let empty = refresh_scoped(
        &second,
        Some(&scope),
        &mut second_cursor,
        3_000,
        || Ok(Vec::<Session>::new()),
    );
    let failed = refresh_scoped(
        &first,
        Some(&scope),
        &mut first_cursor,
        5_000,
        || Err(Error::Command("source unavailable".to_string())),
    );

    assert!(matches!(empty, RefreshOutcome::Authoritative { sessions } if sessions.is_empty()));
    assert!(
        matches!(failed, RefreshOutcome::SourceError { sessions, notice } if sessions.is_empty() && notice == "codex: command failed: source unavailable")
    );
}

#[test]
fn fresh_target_resolution_uses_the_new_pid_and_fresh_refusal_beats_displayed_permission() {
    let displayed = session(BackendKind::Codex, "thread_123", Some(101), false);
    let fresh = session(BackendKind::Codex, "thread_123", Some(202), false);
    let request = TargetRequest::from(&displayed);
    let action_calls = Cell::new(0);

    let permitted = resolve_target(
        request.clone(),
        || Ok(vec![fresh.clone()]),
        |resolved| {
            action_calls.set(action_calls.get() + 1);
            assert_eq!(resolved.pid, Some(202));
            TargetResolution::permitted()
        },
    );
    let refused = resolve_target(
        request,
        || Ok(vec![session(BackendKind::Codex, "thread_123", None, true)]),
        |resolved| {
            action_calls.set(action_calls.get() + 1);
            assert_eq!(resolved.pid, None);
            assert!(resolved.daemon_hosted);
            TargetResolution::refused("fresh daemon state refuses stop")
        },
    );

    assert!(matches!(permitted, TargetResolution::Permitted));
    assert!(matches!(refused, TargetResolution::Refused { .. }));
    assert_eq!(action_calls.get(), 2);
}

#[test]
fn fresh_target_resolution_allows_a_displayed_refusal_only_when_the_source_allows_it() {
    let displayed = session(BackendKind::Opencode, "ses_123", None, false);
    let fresh = session(BackendKind::Opencode, "ses_123", None, true);
    let request = TargetRequest::from(&displayed);
    let action_calls = Cell::new(0);

    let permitted = resolve_target(
        request.clone(),
        || Ok(vec![fresh.clone()]),
        |resolved| {
            action_calls.set(action_calls.get() + 1);
            assert_eq!(resolved, &fresh);
            TargetResolution::permitted()
        },
    );
    let missing = resolve_target(
        request,
        || Ok(Vec::<Session>::new()),
        |_| {
            action_calls.set(action_calls.get() + 1);
            TargetResolution::permitted()
        },
    );

    assert!(matches!(permitted, TargetResolution::Permitted));
    assert!(matches!(missing, TargetResolution::Missing { .. }));
    assert_eq!(action_calls.get(), 1);
}

#[test]
fn cached_opencode_palette_dispatches_identity_and_obeys_fresh_resolution_alone() {
    let displayed = session(BackendKind::Opencode, "ses_123", None, false);
    let fresh = session(BackendKind::Opencode, "ses_123", None, true);
    let request = TargetRequest::from(&displayed);
    let received = RefCell::new(Vec::new());

    let permitted = dispatch_cached_palette(
        request.clone(),
        CachedPaletteAction::Attach,
        |target, action| {
            received.borrow_mut().push((target.clone(), action));
            resolve_target(
                target,
                || Ok(vec![fresh.clone()]),
                |resolved| {
                    assert_eq!(resolved, &fresh);
                    TargetResolution::permitted()
                },
            )
        },
    );
    let refused = dispatch_cached_palette(request, CachedPaletteAction::Attach, |target, _| {
        resolve_target(
            target,
            || Ok(Vec::<Session>::new()),
            |_| TargetResolution::permitted(),
        )
    });

    assert!(matches!(permitted, TargetResolution::Permitted));
    assert!(matches!(refused, TargetResolution::Missing { .. }));
    assert_eq!(received.borrow().len(), 1);
    assert_eq!(received.borrow()[0].0.backend(), BackendKind::Opencode);
    assert_eq!(received.borrow()[0].0.id(), "ses_123");
}

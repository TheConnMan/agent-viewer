mod common;

use agent_viewer_core::Session;
use agent_viewer_core::backend::{Backend, BackendKind, PrRef, SessionOrigin, Status};
use agent_viewer_core::claude::{
    ClaudeBackend, SessionRegistryEntry, is_sdk_entrypoint, mark_sdk_companions, parse_agents_json,
    parse_claude_json_models, parse_job_state, parse_session_registry, read_claude_transcript,
    sessions_root_from,
};
use agent_viewer_core::codex::rollout::TranscriptItem;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Find the session whose title matches. parse_agents_json now returns `Vec<Session>`
/// with the short id folded into `Session.short_id` (no more (Session, String) tuple).
fn by_title<'a>(
    parsed: &'a [agent_viewer_core::Session],
    title: &str,
) -> &'a agent_viewer_core::Session {
    parsed
        .iter()
        .find(|s| s.title == title)
        .unwrap_or_else(|| panic!("no session titled {title}"))
}

#[test]
fn claude_parse_maps_six_states_and_pid() {
    let json = common::read_fixture("claude_agents_all.json");
    let parsed = parse_agents_json(&json).expect("parse agents json");
    // 8 valid entries (the malformed missing-sessionId entry is skipped).
    assert_eq!(parsed.len(), 8);
    assert!(parsed.iter().all(|s| s.title != "Orphan Missing SessionId"));

    let working = by_title(&parsed, "Working Task");
    assert_eq!(working.backend, BackendKind::Claude);
    assert_eq!(working.id, "work0001-6603-4ad5-be5b-f3ad6391d595"); // id = sessionId
    assert_eq!(working.cwd, PathBuf::from("/home/user/proj-working"));
    assert_eq!(working.created_at_ms, 1783659603260); // = startedAt
    assert_eq!(working.updated_at_ms, 1783659603260);
    assert_eq!(working.origin, SessionOrigin::Background);
    assert!(!working.hidden);
    assert!(!working.companion); // entrypoint "cli" is a real fleet member
    assert_eq!(working.status, Status::Working);
    assert_eq!(working.pid, Some(111));
    // short id (entry "id") is now folded into the Session itself.
    assert_eq!(working.short_id, Some("work0001".to_string()));

    assert_eq!(
        by_title(&parsed, "Blocked Task").status,
        Status::needs_input()
    );
    assert_eq!(by_title(&parsed, "Idle Task").status, Status::Idle);
    assert_eq!(by_title(&parsed, "Done Task").status, Status::Done);
    assert_eq!(by_title(&parsed, "Failed Task").status, Status::Error);
    assert_eq!(by_title(&parsed, "Stopped Task").status, Status::Done);

    // Missing state field maps to Unknown, and pid remains absent.
    let nostate = by_title(&parsed, "No State Task");
    assert_eq!(nostate.status, Status::Unknown);
    assert_eq!(nostate.pid, None);
    assert_eq!(nostate.short_id, Some("nost0001".to_string()));
}

// `kind` is the only input to SessionOrigin: "background" -> Background, EVERYTHING else
// (including an absent kind) -> Interactive. Both branches are asserted so a mapping that
// collapsed every row onto a single origin cannot pass.
#[test]
fn claude_origin_is_background_only_for_kind_background() {
    let json = common::read_fixture("claude_agents_all.json");
    let parsed = parse_agents_json(&json).expect("parse agents json");
    assert_eq!(
        by_title(&parsed, "Working Task").origin,
        SessionOrigin::Background
    );
    // The fixture's one kind=="interactive" row.
    assert_eq!(
        by_title(&parsed, "Idle Task").origin,
        SessionOrigin::Interactive
    );

    // A missing kind and an unrecognized kind both land on Interactive too.
    let inline = r#"[
      {"id": "nkin0001",
       "cwd": "/home/user/proj-nokind",
       "startedAt": 1783711000000,
       "sessionId": "nkin0001-5555-4133-a473-d3d004454cdd",
       "name": "No Kind Task",
       "state": "idle"},
      {"id": "okin0001",
       "cwd": "/home/user/proj-oddkind",
       "kind": "vscode",
       "startedAt": 1783711010000,
       "sessionId": "okin0001-6666-4133-a473-d3d004454cee",
       "name": "Odd Kind Task",
       "state": "idle"}
    ]"#;
    let parsed = parse_agents_json(inline).expect("parse inline agents json");
    assert_eq!(
        by_title(&parsed, "No Kind Task").origin,
        SessionOrigin::Interactive
    );
    assert_eq!(
        by_title(&parsed, "Odd Kind Task").origin,
        SessionOrigin::Interactive
    );
}

// A nested `claude -p` (the Agent SDK's headless entrypoint) registers itself in the same
// agents list as a real fleet member, carrying a derived `<dir>-<n>` name nobody chose. It is
// a companion, not a session Brian started, so the default view must exclude it while Ctrl+A
// still reveals it. Both directions are asserted so a blanket `companion = true` cannot pass.
#[test]
fn mark_sdk_companions_flags_only_the_sdk_row() {
    let json = common::read_fixture("claude_agents_all.json");
    let mut parsed = parse_agents_json(&json).expect("parse agents json");
    // The agents output alone never decides this: it does not publish `entrypoint`.
    assert!(parsed.iter().all(|s| !s.companion));

    // pid 888 is the fixture's nested row; 111/222 are live background jobs. Each entry
    // reports the session id the row actually has, so the pid-reuse guard passes.
    let ids: Vec<(u32, String)> = parsed
        .iter()
        .filter_map(|s| s.pid.map(|pid| (pid, s.id.clone())))
        .collect();
    mark_sdk_companions(&mut parsed, |pid| {
        Some(SessionRegistryEntry {
            session_id: ids
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, id)| id.clone()),
            entrypoint: Some(match pid {
                888 => "sdk-cli".to_string(),
                _ => "cli".to_string(),
            }),
        })
    });

    let nested = by_title(&parsed, "feature-x-61");
    assert!(nested.companion);
    // Still a fully-formed row (hidden, not dropped), and it carries no short id, so the
    // per-row capability gate already denies rename/delete on it.
    assert_eq!(nested.id, "sdkc0001-7777-4133-a473-d3d004454dd0");
    assert_eq!(nested.short_id, None);
    assert_eq!(nested.origin, SessionOrigin::Interactive);

    for session in &parsed {
        if session.title == "feature-x-61" {
            continue;
        }
        assert!(
            !session.companion,
            "{} must not be a companion",
            session.title
        );
    }
}

// PURE classifier, exhaustive over the values seen live on this box plus the defensive cases.
// `cli` covers BOTH a `claude --bg` job and an interactive terminal session, which is exactly
// why the `sdk-` prefix is the discriminator and the absence of a job id is not.
#[test]
fn is_sdk_entrypoint_matches_only_the_sdk_prefix() {
    assert!(is_sdk_entrypoint(Some("sdk-cli"))); // verified live: nested `claude -p`
    assert!(is_sdk_entrypoint(Some("sdk-ts")));
    assert!(is_sdk_entrypoint(Some("sdk-py")));

    assert!(!is_sdk_entrypoint(Some("cli"))); // --bg jobs AND interactive sessions
    assert!(!is_sdk_entrypoint(Some("vscode")));
    assert!(!is_sdk_entrypoint(Some("sdk"))); // no separator: not the SDK family
    assert!(!is_sdk_entrypoint(Some("")));
    assert!(!is_sdk_entrypoint(None)); // absent field is a real session
}

// The registry is keyed by PID and the kernel recycles PIDs, so between the agents snapshot
// and the registry read the file at <pid>.json can describe a DIFFERENT session. Flagging on
// entrypoint alone would then hide a real row. The entry must prove it is this session.
#[test]
fn mark_sdk_companions_ignores_a_recycled_pid() {
    let json = common::read_fixture("claude_agents_all.json");
    let mut parsed = parse_agents_json(&json).expect("parse agents json");

    // Every lookup returns an SDK entry belonging to somebody ELSE (or naming nobody).
    mark_sdk_companions(&mut parsed, |_| {
        Some(SessionRegistryEntry {
            session_id: Some("some-other-session-id".to_string()),
            entrypoint: Some("sdk-cli".to_string()),
        })
    });
    assert!(
        parsed.iter().all(|s| !s.companion),
        "a mismatched sessionId must never flag a row"
    );

    mark_sdk_companions(&mut parsed, |_| {
        Some(SessionRegistryEntry {
            session_id: None,
            entrypoint: Some("sdk-cli".to_string()),
        })
    });
    assert!(
        parsed.iter().all(|s| !s.companion),
        "an entry with no sessionId proves nothing and must never flag a row"
    );
}

// Both fixtures are verbatim captures of `~/.claude/sessions/<pid>.json` from this box
// (claude 2.1.220), so a schema drift in that file shows up here rather than as a silently
// un-filtered row.
#[test]
fn parse_session_registry_reads_identity_and_entrypoint() {
    let sdk = parse_session_registry(&common::read_fixture("claude_session_sdk_cli.json"))
        .expect("parse sdk registry entry");
    assert_eq!(sdk.entrypoint.as_deref(), Some("sdk-cli"));
    assert_eq!(
        sdk.session_id.as_deref(),
        Some("sdkc0001-7777-4133-a473-d3d004454dd0")
    );
    assert!(is_sdk_entrypoint(sdk.entrypoint.as_deref()));

    let cli = parse_session_registry(&common::read_fixture("claude_session_cli.json"))
        .expect("parse cli registry entry");
    assert_eq!(cli.entrypoint.as_deref(), Some("cli"));
    assert_eq!(
        cli.session_id.as_deref(),
        Some("work0001-6603-4ad5-be5b-f3ad6391d595")
    );
    assert!(!is_sdk_entrypoint(cli.entrypoint.as_deref()));

    // Missing and wrong-typed fields degrade to None rather than erroring.
    let sparse = parse_session_registry(r#"{"pid":1,"entrypoint":7}"#).expect("object parses");
    assert_eq!(sparse, SessionRegistryEntry::default());

    // Unparseable text yields no entry at all, so the row keeps its default (visible).
    assert_eq!(parse_session_registry("not json"), None);
}

// Same CLAUDE_CONFIG_DIR precedence as the jobs root: a viewer that inherited the env var must
// read the registry the `claude` it shells out to is actually writing.
#[test]
fn sessions_root_follows_the_config_dir_override() {
    let home = PathBuf::from("/home/user");
    assert_eq!(
        sessions_root_from(None, &home),
        PathBuf::from("/home/user/.claude/sessions")
    );
    assert_eq!(
        sessions_root_from(Some("  "), &home),
        PathBuf::from("/home/user/.claude/sessions")
    );
    assert_eq!(
        sessions_root_from(Some("/alt/config"), &home),
        PathBuf::from("/alt/config/sessions")
    );
}

#[test]
fn parse_claude_json_models_cache_first_then_sorted_usage_union() {
    let json = common::read_fixture("claude_config_models.json");
    let got = parse_claude_json_models(&json);
    // additionalModelOptionsCache values first, in array order; then the union of all
    // projects' lastModelUsage keys sorted ascending (overlapping sonnet appears once).
    assert_eq!(
        got,
        vec![
            "opusplan",
            "sonnet[1m]",
            "claude-haiku-4-5",
            "claude-opus-4-8",
            "claude-sonnet-4-5",
        ]
    );
}

#[test]
fn parse_claude_json_models_malformed_is_empty() {
    assert!(parse_claude_json_models("not json").is_empty());
    assert!(parse_claude_json_models("{}").is_empty());
}

#[test]
fn claude_parse_rejects_non_array_top_level() {
    // Documented contract: a non-array top level is Err(Json), not silently empty.
    assert!(parse_agents_json("{\"sessions\": []}").is_err());
    assert!(parse_agents_json("not json").is_err());
}

#[test]
fn parse_job_state_blocked_prefers_needs() {
    let text = common::read_fixture("claude_state_blocked.json");
    let detail = parse_job_state(&text);
    assert_eq!(
        detail.summary,
        "Approve running the migration against the live DB"
    );
    assert_eq!(
        detail.transcript_path,
        Some(PathBuf::from(
            "/home/user/.claude/jobs/block001/transcript.jsonl"
        ))
    );
    // children[kind=="pr"] only — the "note" child is skipped, order preserved. Each
    // carries its id and github href.
    assert_eq!(
        detail.prs,
        vec![
            PrRef {
                id: "315".to_string(),
                href: Some("https://github.com/acme/repo/pull/315".to_string()),
            },
            PrRef {
                id: "318".to_string(),
                href: Some("https://github.com/acme/repo/pull/318".to_string()),
            },
        ]
    );
}

#[test]
fn parse_job_state_no_children_has_empty_prs() {
    let text = common::read_fixture("claude_state_working.json");
    let detail = parse_job_state(&text);
    assert!(detail.prs.is_empty());
}

#[test]
fn parse_job_state_working_uses_detail_and_empty_default() {
    let text = common::read_fixture("claude_state_working.json");
    let detail = parse_job_state(&text);
    // No `needs`, so the summary falls back to `detail`.
    assert_eq!(detail.summary, "Running the test suite");

    // Empty object -> empty summary, no transcript path.
    let empty = parse_job_state("{}");
    assert_eq!(empty.summary, "");
    assert_eq!(empty.transcript_path, None);
}

#[test]
fn read_claude_transcript_extracts_text_tail() {
    let path = common::fixture_path("claude_transcript.jsonl");

    // Full read: string-content user, list-content user, assistant text; thinking,
    // tool_use, and system/attachment/queue noise all skipped.
    let all = read_claude_transcript(&path, 100).expect("read transcript");
    assert_eq!(
        all,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "first user line as a plain string".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply one".to_string(),
            },
            TranscriptItem {
                role: "user".to_string(),
                text: "second user line from a block list".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply two".to_string(),
            },
        ]
    );

    // max_items caps to the LAST two.
    let tail = read_claude_transcript(&path, 2).expect("read transcript");
    assert_eq!(
        tail,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "second user line from a block list".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply two".to_string(),
            },
        ]
    );
}

#[test]
fn read_claude_transcript_skips_text_empty_message() {
    // An assistant message whose content is only a thinking/tool block extracts to "" and
    // must be dropped (no blank role-only line in peek).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("empty_assistant.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"user","message":{{"content":"a real user line"}}}}"#
    )
    .unwrap();
    // Only thinking + tool_use blocks -> text == "" -> skipped.
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"content":[{{"type":"thinking","thinking":"hmm"}},{{"type":"tool_use","name":"bash"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"a real reply"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let items = read_claude_transcript(&path, 100).expect("read transcript");
    assert_eq!(
        items,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "a real user line".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "a real reply".to_string(),
            },
        ]
    );
}

fn claude_session(rollout_path: Option<PathBuf>) -> Session {
    Session {
        backend: BackendKind::Claude,
        id: "activity-session".to_string(),
        short_id: Some("activity".to_string()),
        origin: SessionOrigin::Background,
        title: "Activity session".to_string(),
        cwd: PathBuf::from("/home/user/project"),
        git_branch: None,
        status: Status::Done,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

#[test]
fn claude_turn_activity_normalizes_filters_and_missing_is_empty() {
    let backend = ClaudeBackend::with_binary("/unused/claude");
    let path = common::fixture_path("claude_transcript.jsonl");
    let session = claude_session(Some(path));
    assert_eq!(
        backend
            .turn_activity(&session, Duration::MAX)
            .expect("activity"),
        vec![1_783_717_630_000, 1_783_717_631_250]
    );
    assert!(
        backend
            .turn_activity(&session, Duration::ZERO)
            .expect("old turns excluded")
            .is_empty()
    );
    assert!(
        backend
            .turn_activity(&claude_session(None), Duration::MAX)
            .expect("no transcript")
            .is_empty()
    );
    assert!(
        backend
            .turn_activity(
                &claude_session(Some(PathBuf::from("/missing/claude.jsonl"))),
                Duration::MAX
            )
            .expect("missing transcript")
            .is_empty()
    );
}

// --- Preserved v1 test ---

#[test]
fn claude_list_orders_enriched_updates_ascending_with_stable_ties() {
    let dir = tempfile::TempDir::new().unwrap();
    let binary = dir.path().join("claude");
    let agents = serde_json::json!([
        {
            "id": "tie_z",
            "cwd": "/tmp/project",
            "kind": "background",
            "startedAt": 3000,
            "sessionId": "session_tie_z",
            "name": "Tie Z",
            "state": "done"
        },
        {
            "id": "oldest",
            "cwd": "/tmp/project",
            "kind": "background",
            "startedAt": 1000,
            "sessionId": "session_oldest",
            "name": "Oldest Update",
            "state": "done"
        },
        {
            "id": "tie_a",
            "cwd": "/tmp/project",
            "kind": "background",
            "startedAt": 2000,
            "sessionId": "session_tie_a",
            "name": "Tie A",
            "state": "done"
        }
    ]);
    std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", agents)).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

    let jobs_root = dir.path().join("jobs");
    for (short_id, updated_at_ms) in [("tie_z", 2000), ("oldest", 1000), ("tie_a", 2000)] {
        let state_path = jobs_root.join(short_id).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&state_path, "{}").unwrap();
        std::fs::File::open(&state_path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(updated_at_ms)),
            )
            .unwrap();
    }

    let mut backend = ClaudeBackend::with_roots(
        binary.to_str().unwrap(),
        jobs_root,
        dir.path().join("sessions"),
    );
    let sessions = backend.list().expect("list claude sessions");
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["session_oldest", "session_tie_z", "session_tie_a"]
    );
    assert_eq!(
        sessions
            .iter()
            .map(|session| (session.created_at_ms, session.updated_at_ms))
            .collect::<Vec<_>>(),
        vec![(1000, 1000), (3000, 2000), (2000, 2000)]
    );
}

#[test]
fn claude_missing_binary_lists_empty() {
    // A missing binary is a quiet empty backend, not an error.
    let mut backend = ClaudeBackend::with_binary("/nonexistent/claude");
    let sessions = backend.list().expect("missing binary must be Ok(empty)");
    assert!(sessions.is_empty());
}

// --- Per-row remove capability ---

fn session_with_short_id(short_id: Option<&str>) -> agent_viewer_core::Session {
    agent_viewer_core::Session {
        backend: BackendKind::Claude,
        id: "3f9c1a2e-0000-4000-8000-000000000001".to_string(),
        short_id: short_id.map(str::to_string),
        origin: SessionOrigin::Background,
        title: "probe".to_string(),
        cwd: PathBuf::from("/tmp"),
        git_branch: None,
        status: Status::Working,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

// `remove` is advertised true for the claude backend as a whole, but `claude rm` needs the
// short id and a row without one has no bg job to remove. The capability is therefore
// per-row, and callers must be able to ask BEFORE taking any destructive step.
#[test]
fn claude_delete_capability_is_per_row_and_requires_a_short_id() {
    let backend = ClaudeBackend::new();
    assert!(
        backend.capabilities().delete,
        "backend wide delete stays true"
    );

    assert!(
        backend
            .capabilities_for(&session_with_short_id(Some("ab12")))
            .delete
    );
    // An interactive row carries no short id at all.
    assert!(
        !backend
            .capabilities_for(&session_with_short_id(None))
            .delete
    );
    // A blank short id is the same absence wearing the historical unwrap_or_default shape.
    assert!(
        !backend
            .capabilities_for(&session_with_short_id(Some("")))
            .delete
    );
}

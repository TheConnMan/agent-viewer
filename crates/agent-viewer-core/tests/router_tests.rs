//! The `agent-router` shell-out behind the composer's Auto entry: PATH gating, decision
//! parsing, and the failure path.
//!
//! `router_run_dry_run.json` was captured live from
//! `agent-router run --dry-run --json --dir /tmp "<task>"`, so the field shapes here are the
//! CLI's real output, not an assumed schema.

mod common;

use agent_viewer_core::BackendKind;
use agent_viewer_core::platform::Platform;
use agent_viewer_core::router::{
    ROUTER_BIN, RouterOutcome, find_on_path, parse_outcome, route_command,
};
use common::read_fixture;
use std::ffi::OsStr;

/// The dispatched shape: the same live output with the `dispatch` object the router writes
/// once it has actually started the job (`{job_id, job_name}`).
fn dispatched_fixture() -> String {
    let dry_run = read_fixture("router_run_dry_run.json");
    assert!(
        dry_run.contains("\"dispatch\": null"),
        "the captured dry run must carry a null dispatch"
    );
    dry_run
        .replace("\"dry_run\": true", "\"dry_run\": false")
        .replace(
            "\"dispatch\": null",
            "\"dispatch\": {\n    \"job_id\": \"0199c0de-thread\",\n    \"job_name\": \"Add a unit test for the trunc\"\n  }",
        )
}

#[test]
fn a_dispatched_decision_parses_provider_effort_and_weekly_headroom() {
    let outcome = parse_outcome(&dispatched_fixture()).expect("parsed decision");

    assert_eq!(outcome.provider, BackendKind::Codex);
    // The router scales model and effort to the classified complexity; this capture was a
    // trivial contract, so codex runs the fast tier at low effort.
    assert_eq!(outcome.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(outcome.effort.as_deref(), Some("low"));
    assert_eq!(outcome.gates, Vec::<String>::new());
    assert_eq!(outcome.codex_weekly_pct, 3.0);
    assert_eq!(outcome.claude_weekly_pct, 47.0);
    assert!(
        outcome.rationale.contains("codex_ready 6/6"),
        "the rationale must survive parsing, got {:?}",
        outcome.rationale
    );
    assert_eq!(outcome.job_id.as_deref(), Some("0199c0de-thread"));
    assert_eq!(
        outcome.job_name.as_deref(),
        Some("Add a unit test for the trunc")
    );
}

#[test]
fn a_dispatched_decision_rejects_an_empty_job_id() {
    let empty_id =
        dispatched_fixture().replace("\"job_id\": \"0199c0de-thread\"", "\"job_id\": \"\"");

    let error = parse_outcome(&empty_id).expect_err("an empty job id is not an identity");
    assert!(
        error.contains("job_id") && error.contains("empty"),
        "got {error:?}"
    );
}

#[test]
fn a_dispatched_decision_rejects_an_empty_job_name() {
    let empty_name = dispatched_fixture().replace(
        "\"job_name\": \"Add a unit test for the trunc\"",
        "\"job_name\": \"\"",
    );

    let error = parse_outcome(&empty_name).expect_err("an empty job name is not an identity");
    assert!(
        error.contains("job_name") && error.contains("empty"),
        "got {error:?}"
    );
}

#[test]
fn a_dispatched_decision_requires_a_job_name_but_allows_a_null_job_id() {
    let null_id =
        dispatched_fixture().replace("\"job_id\": \"0199c0de-thread\"", "\"job_id\": null");
    let outcome = parse_outcome(&null_id).expect("a backend may omit its job id");
    assert_eq!(outcome.job_id, None);
    assert_eq!(
        outcome.job_name.as_deref(),
        Some("Add a unit test for the trunc")
    );

    let null_name = null_id.replace(
        "\"job_name\": \"Add a unit test for the trunc\"",
        "\"job_name\": null",
    );
    let error = parse_outcome(&null_name).expect_err("every dispatched job requires a name");
    assert!(
        error.contains("job_name") && error.contains("null"),
        "got {error:?}"
    );
}

#[test]
fn the_footer_notice_is_one_line_naming_the_provider_and_both_headrooms() {
    let outcome = parse_outcome(&dispatched_fixture()).expect("parsed decision");
    let notice = outcome.notice();
    assert_eq!(
        notice,
        "auto: codex gpt-5.6-luna effort low job 0199c0de-thread (codex weekly 3%, claude 47%)"
    );
    assert!(!notice.contains('\n'), "the footer is one line: {notice:?}");
}

#[test]
fn gate_tags_reach_the_notice_so_a_flipped_decision_says_why() {
    let flipped =
        dispatched_fixture().replace("\"gates\": []", "\"gates\": [\"headroom_tiebreak\"]");
    let outcome = parse_outcome(&flipped).expect("parsed decision");

    assert_eq!(outcome.gates, vec!["headroom_tiebreak".to_string()]);
    assert!(
        outcome.notice().contains("gates[headroom_tiebreak]"),
        "got {:?}",
        outcome.notice()
    );
}

#[test]
fn a_real_router_run_rejects_dry_run_or_undispatched_output() {
    let dry_run = dispatched_fixture().replace("\"dry_run\": false", "\"dry_run\": true");
    let error = parse_outcome(&dry_run).expect_err("dry run output must not become a viewer spawn");
    assert!(error.contains("dry_run"), "got {error:?}");

    let undispatched = dispatched_fixture().replace(
        "\"dispatch\": {\n    \"job_id\": \"0199c0de-thread\",\n    \"job_name\": \"Add a unit test for the trunc\"\n  }",
        "\"dispatch\": null",
    );
    let error = parse_outcome(&undispatched).expect_err("a null dispatch means no job was started");
    assert!(error.contains("dispatch"), "got {error:?}");
}

#[test]
fn a_real_router_run_requires_gates_rationale_and_usage() {
    for field in ["gates", "rationale", "usage"] {
        let mut value: serde_json::Value =
            serde_json::from_str(&dispatched_fixture()).expect("fixture json");
        value.as_object_mut().expect("fixture object").remove(field);

        let error = parse_outcome(&value.to_string())
            .expect_err("missing required router fields must reject the decision");
        assert!(error.contains(field), "{field} error was {error:?}");
    }
}

#[test]
fn malformed_router_field_types_reject_the_decision() {
    let malformed = [
        ("provider", serde_json::json!(17)),
        ("model", serde_json::json!({"unexpected": true})),
        ("effort", serde_json::json!(false)),
        ("gates", serde_json::json!("headroom_tiebreak")),
        ("gates", serde_json::json!(["headroom_tiebreak", 17])),
        ("rationale", serde_json::json!(["not", "text"])),
        ("usage", serde_json::json!("unavailable")),
        ("dry_run", serde_json::json!("false")),
        ("dispatch", serde_json::json!([])),
        (
            "dispatch",
            serde_json::json!({
                "job_id": 17,
                "job_name": "Add a unit test for the trunc"
            }),
        ),
        (
            "dispatch",
            serde_json::json!({
                "job_id": null,
                "job_name": false
            }),
        ),
    ];

    for (field, replacement) in malformed {
        let mut value: serde_json::Value =
            serde_json::from_str(&dispatched_fixture()).expect("fixture json");
        value
            .as_object_mut()
            .expect("fixture object")
            .insert(field.to_string(), replacement);

        let error = parse_outcome(&value.to_string())
            .expect_err("malformed router fields must reject the decision");
        assert!(error.contains(field), "{field} error was {error:?}");
    }

    for provider in ["claude", "codex"] {
        let mut value: serde_json::Value =
            serde_json::from_str(&dispatched_fixture()).expect("fixture json");
        value["usage"][provider]["weekly_pct"] = serde_json::json!("unknown");

        let error = parse_outcome(&value.to_string()).expect_err("weekly usage must be numeric");
        assert!(
            error.contains("usage"),
            "{provider} usage error was {error:?}"
        );
    }
}

/// Every unreadable decision is an error the user sees, never a default that would spawn on a
/// guessed provider (and never a panic on the mutation worker).
#[test]
fn an_unreadable_decision_is_an_error_not_a_panic_and_not_a_fallback() {
    let truncated = parse_outcome("{\"provider\": \"codex\"").unwrap_err();
    assert!(
        truncated.contains(ROUTER_BIN) && truncated.contains("unreadable json"),
        "got {truncated:?}"
    );

    let not_json = parse_outcome("agent-router: config is broken").unwrap_err();
    assert!(not_json.contains("unreadable json"), "got {not_json:?}");

    let no_provider = parse_outcome("{\"dispatch\": null}").unwrap_err();
    assert!(no_provider.contains("no provider"), "got {no_provider:?}");

    let unknown = parse_outcome("{\"provider\": \"gemini\"}").unwrap_err();
    assert!(
        unknown.contains("unknown provider") && unknown.contains("gemini"),
        "got {unknown:?}"
    );
}

/// The Auto entry is gated on the binary being discoverable, tested against a controlled PATH
/// value rather than the real environment (where the router happens to be installed).
#[test]
fn the_auto_gate_is_off_when_the_router_binary_is_not_on_the_path() {
    let empty = tempfile::tempdir().expect("temp dir");
    let installed = tempfile::tempdir().expect("temp dir");
    write_router(installed.path(), ROUTER_BIN, true);

    assert_eq!(
        find_on_path(Platform::Linux, ROUTER_BIN, Some(empty.path().as_os_str())),
        None,
        "a PATH without the router must not offer Auto"
    );
    assert_eq!(
        find_on_path(Platform::Linux, ROUTER_BIN, None),
        None,
        "no PATH at all must not offer Auto"
    );

    let path = std::env::join_paths([empty.path(), installed.path()]).expect("join paths");
    assert_eq!(
        find_on_path(Platform::Linux, ROUTER_BIN, Some(path.as_os_str())),
        Some(installed.path().join(ROUTER_BIN)),
        "the router must be found in the second PATH entry"
    );
}

/// A file named `agent-router` that cannot be executed is not a router: offering Auto for it
/// would put an entry in the Tab cycle whose every submission fails on exec.
#[test]
#[cfg(unix)]
fn the_auto_gate_is_off_for_a_non_executable_file_named_like_the_router() {
    let unexecutable = tempfile::tempdir().expect("temp dir");
    write_router(unexecutable.path(), ROUTER_BIN, false);

    assert_eq!(
        find_on_path(
            Platform::Linux,
            ROUTER_BIN,
            Some(unexecutable.path().as_os_str())
        ),
        None,
        "a non-executable agent-router must not offer Auto"
    );
}

/// The windows release installs `agent-router.exe`, so a gate that only ever probes the bare name
/// leaves Auto permanently missing on a supported platform. The platform is a parameter (as in
/// `platform_tests.rs`), so the windows branch is exercised on this host too.
#[test]
fn the_windows_router_is_found_by_its_exe_suffix() {
    let installed = tempfile::tempdir().expect("temp dir");
    write_router(installed.path(), "agent-router.exe", true);

    assert_eq!(
        find_on_path(
            Platform::Windows,
            ROUTER_BIN,
            Some(installed.path().as_os_str())
        ),
        Some(installed.path().join("agent-router.exe")),
        "a windows install must satisfy the Auto gate"
    );
    assert_eq!(
        find_on_path(
            Platform::Linux,
            ROUTER_BIN,
            Some(installed.path().as_os_str())
        ),
        None,
        "a unix router is the bare name; an .exe must not satisfy the gate off windows"
    );
}

/// The task is user text and can begin with a hyphen (`--help ...`, `-v ...`). Passed as a bare
/// positional it is parsed by the router's own clap as an option, so the run fails instead of
/// routing. A literal `--` ends option parsing and keeps the task a positional whatever it says.
#[test]
fn the_task_is_passed_after_a_double_dash_so_a_hyphen_task_stays_the_task() {
    let command = route_command(
        std::path::Path::new("/tmp/routed proj"),
        "--help me refactor",
    );

    assert_eq!(command.get_program(), OsStr::new(ROUTER_BIN));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("run"),
            OsStr::new("--json"),
            OsStr::new("--dir"),
            OsStr::new("/tmp/routed proj"),
            OsStr::new("--provider"),
            OsStr::new("auto"),
            OsStr::new("--"),
            OsStr::new("--help me refactor"),
        ]
    );
}

/// The router's job name falls back to a prefix of the task when the backend reported no id, and a
/// task is multi-line often enough. A newline in the footer notice splits or clips the one line the
/// user reads, so the name is collapsed to single spaces before it lands there.
#[test]
fn a_multiline_job_name_still_yields_a_one_line_notice() {
    let multiline = dispatched_fixture().replace(
        "\"job_id\": \"0199c0de-thread\",\n    \"job_name\": \"Add a unit test for the trunc\"",
        "\"job_id\": null,\n    \"job_name\": \"Fix the parser\\n\\nthen add a test\"",
    );
    let outcome = parse_outcome(&multiline).expect("parsed decision");

    let notice = outcome.notice();
    assert_eq!(
        notice,
        "auto: codex gpt-5.6-luna effort low job Fix the parser then add a test (codex weekly 3%, claude 47%)"
    );
    assert!(
        !notice.contains('\n') && !notice.contains('\r'),
        "the footer is one line: {notice:?}"
    );
}

/// Write a fake router named `name` into `dir`, with or without the executable bit (a no-op off
/// unix, where there is no bit to set).
fn write_router(dir: &std::path::Path, name: &str, executable: bool) {
    let path = dir.join(name);
    std::fs::write(&path, b"#!/bin/sh\n").expect("write router");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("set router mode");
    }
    let _ = executable;
}

/// The Auto model entry exists so the picker has something to show; the router owns the real
/// model and effort choice, so a routed decision's own model must survive untouched.
#[test]
fn the_router_model_is_reported_when_the_router_chose_one() {
    let claude = dispatched_fixture()
        .replace("\"provider\": \"codex\"", "\"provider\": \"claude\"")
        .replace("\"model\": \"gpt-5.6-luna\"", "\"model\": \"opus[1m]\"")
        .replace("\"effort\": \"low\"", "\"effort\": null");
    let outcome: RouterOutcome = parse_outcome(&claude).expect("parsed decision");

    assert_eq!(outcome.provider, BackendKind::Claude);
    assert_eq!(outcome.model.as_deref(), Some("opus[1m]"));
    assert_eq!(outcome.effort, None);
    assert_eq!(
        outcome.notice(),
        "auto: claude opus[1m] job 0199c0de-thread (codex weekly 3%, claude 47%)"
    );
}

/// Serializes the tests that point PATH at a fake router: two of them racing would restore
/// each other's temporary PATH mid-run.
#[cfg(target_os = "linux")]
static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A router failure lands in the one line footer, and the error carries both the described
/// argv (which embeds the raw, possibly multi-line task) and the router's stderr. The route
/// boundary must collapse them.
#[test]
#[cfg(target_os = "linux")]
fn a_failing_router_reports_a_one_line_error() {
    let _lock = PATH_LOCK.lock().expect("path lock");
    let original_path = std::env::var_os("PATH");
    let router_dir = tempfile::tempdir().expect("router dir");
    let router_path = router_dir.path().join(ROUTER_BIN);
    let script = "#!/bin/sh\necho 'boom line one' >&2\necho 'boom line two' >&2\nexit 7\n";
    std::fs::write(&router_path, script).expect("write router");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&router_path, std::fs::Permissions::from_mode(0o755))
        .expect("set router mode");

    unsafe {
        std::env::set_var("PATH", router_dir.path());
    }
    let result = agent_viewer_core::router::route(
        std::path::Path::new("/tmp"),
        "fix the parser\n\nthen add a test",
    );
    if let Some(path) = original_path {
        unsafe {
            std::env::set_var("PATH", path);
        }
    } else {
        unsafe {
            std::env::remove_var("PATH");
        }
    }

    let error = result.expect_err("a non-zero router exit is a failure");
    assert!(
        error.contains("boom line one") && error.contains("boom line two"),
        "the router's stderr must survive into the error, got {error:?}"
    );
    assert!(
        !error.contains('\n') && !error.contains('\r'),
        "the footer shows one line, got {error:?}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_router_stdout_larger_than_the_json_limit_is_drained_and_rejected() {
    let _lock = PATH_LOCK.lock().expect("path lock");
    let original_path = std::env::var_os("PATH");
    let router_dir = tempfile::tempdir().expect("router dir");
    let router_path = router_dir.path().join(ROUTER_BIN);
    let script = r#"#!/bin/sh
printf '%s' '{"provider":"codex","model":null,"effort":"xhigh","dry_run":false,"dispatch":{"job_id":"large","job_name":"large"},"gates":[],"rationale":"large","usage":{"claude":{"weekly_pct":52},"codex":{"weekly_pct":87}},"padding":"'
/usr/bin/head -c 524288 /dev/zero | /usr/bin/tr '\000' x
printf '%s' '"}'
"#;
    std::fs::write(&router_path, script).expect("write router");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&router_path, std::fs::Permissions::from_mode(0o755))
        .expect("set router mode");

    unsafe {
        std::env::set_var("PATH", router_dir.path());
    }
    let started = std::time::Instant::now();
    let result = agent_viewer_core::router::route(std::path::Path::new("/tmp"), "task");
    if let Some(path) = original_path {
        unsafe {
            std::env::set_var("PATH", path);
        }
    } else {
        unsafe {
            std::env::remove_var("PATH");
        }
    }

    let error = result.expect_err("oversized router json must be rejected");
    assert!(
        error.contains(ROUTER_BIN) && error.contains("truncated"),
        "got {error:?}"
    );
    assert!(!error.contains("timed out"), "got {error:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the oversized pipe was not drained promptly"
    );
}

#[cfg(unix)]
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use agent_viewer_core::{
    Backend as _, ListingCacheClaim, ListingCacheRead, ListingCacheSnapshot, ViewerDb,
    codex::CodexBackend,
};
#[cfg(unix)]
use rusqlite::Connection;
#[cfg(unix)]
use serde_json::{Value, json};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const DONE_A: &str = "11111111-aaaa-4aaa-8aaa-000000000001";
#[cfg(unix)]
const ERROR_B: &str = "11111111-bbbb-4bbb-8bbb-000000000002";
#[cfg(unix)]
const HIDDEN: &str = "22222222-aaaa-4aaa-8aaa-000000000003";
#[cfg(unix)]
const SUBAGENT: &str = "33333333-aaaa-4aaa-8aaa-000000000004";
#[cfg(unix)]
const MISSING_CWD: &str = "44444444-aaaa-4aaa-8aaa-000000000005";
#[cfg(unix)]
const HOLD: &str = "55555555-aaaa-4aaa-8aaa-000000000006";
#[cfg(unix)]
const EXEC_COMPANION: &str = "66666666-aaaa-4aaa-8aaa-000000000007";
#[cfg(unix)]
const CLAUDE_BLOCKED: &str = "block001-b237-4133-a473-d3d004454c52";
#[cfg(unix)]
const CLAUDE_IDLE: &str = "idle0001-1111-4133-a473-d3d004454c99";
#[cfg(unix)]
const CLAUDE_DONE: &str = "done0001-e217-4cf7-938c-6c5fd82429d5";
#[cfg(unix)]
const CLAUDE_ERROR: &str = "fail0001-2222-4133-a473-d3d004454caa";

#[cfg(unix)]
struct CliFixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    claude_config: PathBuf,
    grok_home: PathBuf,
    bin: PathBuf,
    claude_agents: PathBuf,
    calls: PathBuf,
    backend_init: PathBuf,
}

#[cfg(unix)]
impl CliFixture {
    fn empty() -> CliFixture {
        let root = tempfile::tempdir().expect("create CLI fixture root");
        let home = root.path().join("home");
        let codex_home = root.path().join("codex");
        let claude_config = root.path().join("claude");
        let grok_home = root.path().join("grok");
        let bin = root.path().join("bin");
        for path in [&home, &codex_home, &claude_config, &grok_home, &bin] {
            std::fs::create_dir_all(path).expect("create isolated provider directory");
        }
        let claude_agents = root.path().join("claude-agents.json");
        let calls = root.path().join("provider-calls");
        let backend_init = root.path().join("backend-init");
        std::fs::write(&claude_agents, "[]\n").expect("write empty Claude listing");
        write_executable(
            &bin.join("claude"),
            "#!/bin/sh\nif [ \"$1\" = agents ]; then\n  : > \"$BACKEND_INIT\"\n  while IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\"; done < \"$CLAUDE_AGENTS_FIXTURE\"\nelse\n  printf '%s\\n' \"$@\" >> \"$CLI_CALLS\"\nfi\n",
        );
        write_executable(
            &bin.join("codex"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$CLI_CALLS\"\nif [ \"$1\" = app-server ]; then\n  while IFS= read -r line; do\n    case \"$line\" in *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}' ;; esac\n  done\nfi\n",
        );
        CliFixture {
            _root: root,
            home,
            codex_home,
            claude_config,
            grok_home,
            bin,
            claude_agents,
            calls,
            backend_init,
        }
    }

    fn with_sessions() -> CliFixture {
        let fixture = CliFixture::empty();
        let project_a = fixture.home.join("project-a");
        let project_b = fixture.home.join("project-b");
        std::fs::create_dir_all(&project_a).expect("create first project");
        std::fs::create_dir_all(&project_b).expect("create second project");
        let conn = Connection::open(fixture.codex_home.join("state_5.sqlite"))
            .expect("create Codex registry");
        conn.execute_batch(include_str!(
            "../../agent-viewer-core/tests/fixtures/threads_schema.sql"
        ))
        .expect("install real Codex registry schema");
        add_codex(
            &conn,
            &fixture,
            DONE_A,
            "Completed primary",
            &project_a,
            "cli",
            false,
            "rollout_complete.jsonl",
            10,
        );
        add_codex(
            &conn,
            &fixture,
            ERROR_B,
            "Crashed primary",
            &project_b,
            "vscode",
            false,
            "rollout_midturn.jsonl",
            20,
        );
        add_codex(
            &conn,
            &fixture,
            HIDDEN,
            "Archived primary",
            &project_a,
            "cli",
            true,
            "rollout_complete.jsonl",
            30,
        );
        add_codex(
            &conn,
            &fixture,
            SUBAGENT,
            "Worker child",
            &project_a,
            r#"{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}"#,
            false,
            "rollout_complete.jsonl",
            40,
        );
        add_codex(
            &conn,
            &fixture,
            MISSING_CWD,
            "Deleted worktree",
            &fixture.home.join("gone"),
            "cli",
            false,
            "rollout_complete.jsonl",
            50,
        );
        add_codex(
            &conn,
            &fixture,
            HOLD,
            "HOLD",
            &project_a,
            "cli",
            false,
            "rollout_complete.jsonl",
            60,
        );
        add_codex(
            &conn,
            &fixture,
            EXEC_COMPANION,
            "Exec companion",
            &project_a,
            "exec",
            false,
            "rollout_complete.jsonl",
            70,
        );
        drop(conn);

        let claude_project = fixture.home.join("claude-project");
        std::fs::create_dir_all(&claude_project).expect("create Claude project");
        let agents = json!([
            claude_row(
                "block001",
                CLAUDE_BLOCKED,
                "Blocked Task",
                "blocked",
                &claude_project
            ),
            claude_row(
                "idle0001",
                CLAUDE_IDLE,
                "Idle Task",
                "idle",
                &claude_project
            ),
            claude_row(
                "done0001",
                CLAUDE_DONE,
                "Done Task",
                "done",
                &claude_project
            ),
            claude_row(
                "fail0001",
                CLAUDE_ERROR,
                "Failed Task",
                "failed",
                &claude_project
            ),
        ]);
        std::fs::write(&fixture.claude_agents, format!("{agents}\n"))
            .expect("write Claude provider fixture");
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-viewer"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CODEX_HOME", &self.codex_home)
            .env("CLAUDE_CONFIG_DIR", &self.claude_config)
            .env("GROK_HOME", &self.grok_home)
            .env("PATH", &self.bin)
            .env("CLAUDE_AGENTS_FIXTURE", &self.claude_agents)
            .env("CLI_CALLS", &self.calls)
            .env("BACKEND_INIT", &self.backend_init);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        run_bounded(self.command().args(args))
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
fn add_codex(
    conn: &Connection,
    fixture: &CliFixture,
    id: &str,
    title: &str,
    cwd: &Path,
    source: &str,
    archived: bool,
    rollout_fixture: &str,
    updated: i64,
) {
    let rollout_dir = fixture.codex_home.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).expect("create rollout directory");
    let rollout = rollout_dir.join(format!("{id}.jsonl"));
    std::fs::copy(core_fixture(rollout_fixture), &rollout).expect("copy real rollout fixture");
    conn.execute(
        "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived, git_branch, created_at_ms, updated_at_ms, preview) VALUES (?1, ?2, 1, ?3, ?4, 'openai', ?5, ?6, 'workspace-write', 'never', ?7, 'task/thread-cli', 1000, ?8, 'fixture summary')",
        rusqlite::params![id, rollout.to_string_lossy(), updated, source, cwd.to_string_lossy(), title, i64::from(archived), updated * 1000],
    )
    .expect("insert Codex thread");
}

#[cfg(unix)]
fn claude_row(short_id: &str, id: &str, title: &str, state: &str, cwd: &Path) -> Value {
    json!({"id": short_id, "cwd": cwd, "kind": "background", "startedAt": 2000, "sessionId": id, "name": title, "state": state})
}

#[cfg(unix)]
fn core_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-viewer-core/tests/fixtures")
        .join(name)
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write provider stub");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make provider stub executable");
}

#[cfg(unix)]
fn install_wait_sequence(fixture: &CliFixture, middle: &str, final_rows: &Value) {
    let final_path = fixture.home.join("wait-final.json");
    let first_seen = fixture.home.join("wait-first-seen");
    let middle_seen = fixture.home.join("wait-middle-seen");
    std::fs::write(&final_path, format!("{final_rows}\n")).expect("write final wait listing");
    let emit_final = format!(
        "while IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\"; done < \"{}\"",
        final_path.display()
    );
    write_executable(
        &fixture.bin.join("claude"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = agents ]; then\n  if [ ! -e \"{}\" ]; then\n    : > \"{}\"\n    while IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\"; done < \"$CLAUDE_AGENTS_FIXTURE\"\n  elif [ ! -e \"{}\" ]; then\n    : > \"{}\"\n    {middle}\n  else\n    {emit_final}\n  fi\nfi\n",
            first_seen.display(),
            first_seen.display(),
            middle_seen.display(),
            middle_seen.display(),
        ),
    );
}

#[cfg(unix)]
fn seed_codex_listing_cache(
    fixture: &CliFixture,
) -> (ViewerDb, agent_viewer_core::ListingCacheScope) {
    let db = ViewerDb::open(&fixture.home.join(".local/state/agent-viewer/viewer.db"))
        .expect("open fixture viewer database");
    let backend = CodexBackend::new(fixture.codex_home.clone());
    let scope = backend.listing_scope().expect("Codex listing scope");
    let lease = match db
        .claim_listing_refresh(Some(&scope), None, 10, 2_000, 5_000)
        .expect("claim fixture listing")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected listing lease, got {other:?}"),
    };
    db.publish_listing(
        &lease,
        ListingCacheSnapshot::from_sessions(Vec::new()).expect("empty listing snapshot"),
        11,
    )
    .expect("publish fixture listing");
    assert!(matches!(
        db.read_listing_snapshot(&scope, 12)
            .expect("read fresh listing"),
        ListingCacheRead::Fresh(_)
    ));
    (db, scope)
}

fn run_bounded(command: &mut Command) -> Output {
    let mut stdout = tempfile::tempfile().expect("create stdout capture");
    let mut stderr = tempfile::tempfile().expect("create stderr capture");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()));
    let mut child = command.spawn().expect("spawn agent-viewer");
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll agent-viewer") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = reap_bounded(&mut child, Duration::from_secs(1));
            panic!("agent-viewer did not exit within {PROCESS_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: read_capture(&mut stdout),
        stderr: read_capture(&mut stderr),
    }
}

fn reap_bounded(child: &mut std::process::Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return None,
        }
    }
}

#[cfg(target_os = "linux")]
fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read pid from {}: {error}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parse pid from {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn process_alive(pid: i32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn assert_processes_gone(pids: &[i32]) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let alive = loop {
        let alive = pids
            .iter()
            .copied()
            .filter(|pid| process_alive(*pid))
            .collect::<Vec<_>>();
        if alive.is_empty() || Instant::now() >= deadline {
            break alive;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let leaked = alive.clone();
    for pid in &leaked {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    let cleanup_survivors = loop {
        let survivors = leaked
            .iter()
            .copied()
            .filter(|pid| process_alive(*pid))
            .collect::<Vec<_>>();
        if survivors.is_empty() || Instant::now() >= cleanup_deadline {
            break survivors;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        cleanup_survivors.is_empty(),
        "could not clean up owned test processes: {cleanup_survivors:?}"
    );
    assert!(
        leaked.is_empty(),
        "owned test processes survived: {leaked:?}"
    );
}

#[cfg(target_os = "linux")]
fn wait_for_pid_files(paths: &[&Path]) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if paths.iter().all(|path| path.is_file()) {
            return paths.iter().map(|path| read_pid(path)).collect();
        }
        thread::sleep(Duration::from_millis(10));
    }
    paths
        .iter()
        .filter(|path| path.is_file())
        .map(|path| read_pid(path))
        .collect()
}

fn read_capture(capture: &mut std::fs::File) -> Vec<u8> {
    capture.seek(SeekFrom::Start(0)).expect("rewind capture");
    let mut bytes = Vec::new();
    capture.read_to_end(&mut bytes).expect("read capture");
    bytes
}

fn assert_version_flag(flag: &str) {
    let home = tempfile::tempdir().expect("create isolated home");
    let output = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_agent-viewer"))
            .arg(flag)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path()),
    );

    assert_success(output.status, flag);
    assert_eq!(
        output.stdout,
        format!("agent-viewer {}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
        "{flag} must print only the package version"
    );
    assert_eq!(
        output.stderr, b"",
        "{flag} must not emit startup diagnostics"
    );
    assert!(
        home.path()
            .read_dir()
            .expect("read isolated home")
            .next()
            .is_none(),
        "{flag} must exit before backend or viewer state initialization"
    );
}

fn assert_success(status: ExitStatus, flag: &str) {
    assert!(status.success(), "{flag} exited with {status}");
}

#[test]
fn long_version_flag_exits_before_terminal_and_backend_startup() {
    assert_version_flag("--version");
}

#[test]
fn short_version_flag_exits_before_terminal_and_backend_startup() {
    assert_version_flag("-V");
}

#[cfg(unix)]
fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON stdout ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[cfg(unix)]
fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn assert_error(output: &Output, code: i32) {
    assert_exit(output, code);
    let value = parse_json(output);
    assert_eq!(value["schema_version"], 1);
    assert!(
        value["error"]["code"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[cfg(unix)]
fn assert_error_kind(output: &Output, code: i32, kind: &str) {
    assert_error(output, code);
    assert_eq!(parse_json(output)["error"]["code"], kind);
}

#[cfg(unix)]
fn sessions(value: &Value) -> &[Value] {
    value["sessions"].as_array().expect("sessions array")
}

#[test]
#[cfg(unix)]
fn help_exits_before_backend_initialization() {
    for flag in ["--help", "-h"] {
        let fixture = CliFixture::empty();
        let output = fixture.run(&[flag]);
        assert_exit(&output, 0);
        let help = String::from_utf8(output.stdout).expect("UTF8 help");
        assert!(
            help.contains("list") && help.contains("show") && help.contains("wait"),
            "{help}"
        );
        assert_eq!(output.stderr, b"");
        assert!(!fixture.calls.exists());
        assert!(!fixture.backend_init.exists());
    }
}

#[test]
#[cfg(unix)]
fn subcommand_version_exits_before_backend_initialization() {
    for flag in ["--version", "-V"] {
        let fixture = CliFixture::empty();
        let output = fixture.run(&["list", flag]);
        assert_exit(&output, 0);
        assert_eq!(
            output.stdout,
            format!("agent-viewer {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
        );
        assert_eq!(output.stderr, b"");
        assert!(!fixture.backend_init.exists());
    }
}

#[test]
#[cfg(unix)]
fn human_errors_write_only_to_stderr() {
    let fixture = CliFixture::empty();
    let output = fixture.run(&["show", "missing", "--provider", "codex"]);
    assert_exit(&output, 3);
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"agent-viewer: session missing not found or provider unavailable\n"
    );
}

#[test]
#[cfg(unix)]
fn json_shorthand_matches_list_and_empty_backends_are_not_errors() {
    let fixture = CliFixture::empty();
    let shorthand = fixture.run(&["--json"]);
    let explicit = fixture.run(&["list", "--json"]);
    assert_exit(&shorthand, 0);
    assert_exit(&explicit, 0);
    assert_eq!(parse_json(&shorthand), parse_json(&explicit));
    let value = parse_json(&explicit);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "list");
    assert_eq!(value["sessions"], json!([]));
    assert!(value["providers"].is_array());

    let codex = parse_json(&fixture.run(&["list", "--provider", "codex", "--json"]));
    assert_eq!(
        codex["providers"],
        json!([{"provider": "codex", "observation": "unavailable"}])
    );
    let claude = parse_json(&fixture.run(&["list", "--provider", "claude", "--json"]));
    assert_eq!(
        claude["providers"],
        json!([{"provider": "claude", "observation": "empty_or_unavailable"}])
    );

    let rooted = fixture.run(&["--json", "list", "--provider", "codex"]);
    let subcommand = fixture.run(&["list", "--provider", "codex", "--json"]);
    assert_exit(&rooted, 0);
    assert_eq!(parse_json(&rooted), parse_json(&subcommand));
}

#[test]
#[cfg(unix)]
fn list_uses_real_statuses_filters_and_default_visibility() {
    let fixture = CliFixture::with_sessions();
    let output = fixture.run(&["list", "--provider", "codex", "--json"]);
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(
        value["providers"],
        json!([{"provider": "codex", "observation": "listed"}])
    );
    let ids = sessions(&value)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from([DONE_A, ERROR_B]));
    let done = sessions(&value)
        .iter()
        .find(|row| row["id"] == DONE_A)
        .unwrap();
    assert_eq!(done["target"], format!("codex:{DONE_A}"));
    assert_eq!(done["status"], "done");
    assert_eq!(done["archive_mode"], "reversible");
    assert_eq!(
        done["capabilities"],
        json!({"archive": true, "stop": false, "rename": true, "unarchive": true})
    );
    let keys = done
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "provider",
            "id",
            "short_id",
            "target",
            "title",
            "status",
            "cwd",
            "git_branch",
            "summary",
            "created_at_ms",
            "updated_at_ms",
            "hidden",
            "companion",
            "subagent",
            "pr_refs",
            "archive_mode",
            "capabilities"
        ])
    );

    let all = parse_json(&fixture.run(&["list", "--provider", "codex", "--all", "--json"]));
    assert_eq!(sessions(&all).len(), 7);
    assert!(sessions(&all).iter().any(|row| row["id"] == EXEC_COMPANION));
    let filtered = parse_json(&fixture.run(&[
        "list",
        "--provider",
        "codex",
        "--status",
        "done",
        "--dir",
        fixture.home.join("project-a").to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(
        sessions(&filtered)
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![DONE_A]
    );
}

#[test]
#[cfg(unix)]
fn list_dir_normalizes_relative_and_equivalent_paths() {
    let fixture = CliFixture::with_sessions();
    let project = fixture.home.join("project-a");

    let mut from_project = fixture.command();
    from_project.current_dir(&project).args([
        "list",
        "--provider",
        "codex",
        "--dir",
        ".",
        "--json",
    ]);
    let relative = run_bounded(&mut from_project);
    assert_exit(&relative, 0);
    assert_eq!(
        sessions(&parse_json(&relative))
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![DONE_A]
    );

    let mut from_home = fixture.command();
    from_home.current_dir(&fixture.home).args([
        "list",
        "--provider",
        "codex",
        "--dir",
        "project-a/../project-a",
        "--json",
    ]);
    let equivalent = run_bounded(&mut from_home);
    assert_exit(&equivalent, 0);
    assert_eq!(parse_json(&equivalent), parse_json(&relative));
}

#[test]
#[cfg(unix)]
fn show_prefers_exact_ids_and_refuses_ambiguous_or_missing_prefixes() {
    let fixture = CliFixture::with_sessions();
    let exact = fixture.run(&["show", DONE_A, "--provider", "codex", "--json"]);
    assert_exit(&exact, 0);
    let exact = parse_json(&exact);
    assert_eq!(exact["command"], "show");
    assert_eq!(exact["session"]["id"], DONE_A);
    assert_error(
        &fixture.run(&["show", "11111111", "--provider", "codex", "--json"]),
        4,
    );
    assert_error(
        &fixture.run(&["show", "does-not-exist", "--provider", "codex", "--json"]),
        3,
    );
}

#[test]
#[cfg(unix)]
fn exact_full_and_short_ids_beat_prefix_collisions() {
    let fixture = CliFixture::empty();
    let cwd = fixture.home.join("claude-collisions");
    std::fs::create_dir_all(&cwd).expect("create collision project");
    let collision_id = format!("{CLAUDE_DONE}-prefix-collision");
    let agents = json!([
        claude_row("done0001", CLAUDE_DONE, "Exact", "done", &cwd),
        claude_row(
            "done0001-extra",
            &collision_id,
            "Prefix collision",
            "done",
            &cwd
        ),
    ]);
    std::fs::write(&fixture.claude_agents, format!("{agents}\n")).expect("write collision listing");

    for target in [CLAUDE_DONE, "done0001"] {
        let output = fixture.run(&["show", target, "--provider", "claude", "--json"]);
        assert_exit(&output, 0);
        assert_eq!(parse_json(&output)["session"]["id"], CLAUDE_DONE);
    }
}

#[test]
#[cfg(unix)]
fn tail_reads_real_rollout_events_and_validates_limits() {
    let fixture = CliFixture::with_sessions();
    let output = fixture.run(&[
        "tail",
        DONE_A,
        "--provider",
        "codex",
        "--limit",
        "2",
        "--json",
    ]);
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["command"], "tail");
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["id"], DONE_A);
    assert_eq!(value["events"].as_array().unwrap().len(), 2);
    assert_eq!(value["events"][0]["type"], "agent");
    assert_eq!(value["events"][0]["text"], "DONE");
    assert_eq!(value["events"][1]["type"], "tool");
    for limit in ["0", "10001"] {
        assert_error(
            &fixture.run(&["tail", DONE_A, "--limit", limit, "--json"]),
            2,
        );
    }
}

#[test]
#[cfg(unix)]
fn wait_handles_done_needs_input_error_and_deadline_timeout() {
    let fixture = CliFixture::with_sessions();
    for (provider, id) in [("codex", DONE_A), ("claude", CLAUDE_BLOCKED)] {
        let output = fixture.run(&[
            "wait",
            id,
            "--provider",
            provider,
            "--timeout",
            "1",
            "--json",
        ]);
        assert_exit(&output, 0);
        let value = parse_json(&output);
        assert_eq!(value["command"], "wait");
        assert_eq!(value["session"]["id"], id);
    }
    let error = fixture.run(&[
        "wait",
        CLAUDE_ERROR,
        "--provider",
        "claude",
        "--timeout",
        "1",
        "--json",
    ]);
    assert_exit(&error, 1);
    let error = parse_json(&error);
    assert_eq!(error["command"], "wait");
    assert_eq!(error["session"]["id"], CLAUDE_ERROR);
    assert_eq!(error["session"]["status"], "error");
    assert_error(
        &fixture.run(&[
            "wait",
            CLAUDE_IDLE,
            "--provider",
            "claude",
            "--timeout",
            "0",
            "--json",
        ]),
        124,
    );
}

#[test]
#[cfg(unix)]
fn wait_distinguishes_initial_missing_from_backend_failure() {
    let missing = CliFixture::empty();
    assert_error_kind(
        &missing.run(&[
            "wait",
            "missing-session",
            "--provider",
            "claude",
            "--timeout",
            "1",
            "--json",
        ]),
        3,
        "not_found",
    );

    let broken = CliFixture::empty();
    std::fs::write(&broken.claude_agents, "{}\n").expect("write malformed Claude listing");
    assert_error_kind(
        &broken.run(&[
            "wait",
            "missing-session",
            "--provider",
            "claude",
            "--timeout",
            "1",
            "--json",
        ]),
        1,
        "backend",
    );
}

#[test]
#[cfg(unix)]
fn wait_drains_large_provider_output_before_the_child_exits() {
    let fixture = CliFixture::empty();
    let cwd = fixture.home.join("large-claude-output");
    std::fs::create_dir_all(&cwd).expect("create large output project");
    let title = "t".repeat(100_000);
    let agents = json!([claude_row("done0001", CLAUDE_DONE, &title, "done", &cwd),]);
    std::fs::write(&fixture.claude_agents, format!("{agents}\n"))
        .expect("write large Claude listing");

    let output = fixture.run(&[
        "wait",
        CLAUDE_DONE,
        "--provider",
        "claude",
        "--timeout",
        "2",
        "--json",
    ]);
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["session"]["id"], CLAUDE_DONE);
    assert_eq!(value["session"]["status"], "done");
    assert_eq!(value["session"]["title"], title);
}

#[test]
#[cfg(unix)]
fn wait_retries_transient_claude_failures_after_a_positive_match() {
    for middle in ["exit 7", "printf '%s\\n' '{}'"] {
        let fixture = CliFixture::empty();
        let cwd = fixture.home.join("claude-wait");
        std::fs::create_dir_all(&cwd).expect("create wait project");
        let idle = json!([claude_row(
            "idle0001",
            CLAUDE_IDLE,
            "Idle Task",
            "idle",
            &cwd
        )]);
        let done = json!([claude_row(
            "idle0001",
            CLAUDE_IDLE,
            "Idle Task",
            "done",
            &cwd
        )]);
        std::fs::write(&fixture.claude_agents, format!("{idle}\n"))
            .expect("write initial wait listing");
        install_wait_sequence(&fixture, middle, &done);

        let output = fixture.run(&[
            "wait",
            CLAUDE_IDLE,
            "--provider",
            "claude",
            "--timeout",
            "2",
            "--json",
        ]);
        assert_exit(&output, 0);
        let value = parse_json(&output);
        assert_eq!(value["session"]["id"], CLAUDE_IDLE);
        assert_eq!(value["session"]["status"], "done");
    }
}

#[test]
#[cfg(unix)]
fn wait_times_out_when_a_resolved_claude_session_stays_absent() {
    let fixture = CliFixture::empty();
    let cwd = fixture.home.join("claude-wait");
    std::fs::create_dir_all(&cwd).expect("create wait project");
    let idle = json!([claude_row(
        "idle0001",
        CLAUDE_IDLE,
        "Idle Task",
        "idle",
        &cwd
    )]);
    std::fs::write(&fixture.claude_agents, format!("{idle}\n"))
        .expect("write initial wait listing");
    install_wait_sequence(&fixture, "printf '%s\\n' '[]'", &json!([]));

    assert_error(
        &fixture.run(&[
            "wait",
            CLAUDE_IDLE,
            "--provider",
            "claude",
            "--timeout",
            "0.4",
            "--json",
        ]),
        124,
    );
}

#[test]
#[cfg(target_os = "linux")]
fn wait_deadline_bounds_the_initial_provider_call_and_reaps_it() {
    let fixture = CliFixture::empty();
    let pid_path = fixture.home.join("slow-provider-pid");
    write_executable(
        &fixture.bin.join("claude"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = agents ]; then\n  printf '%s' \"$$\" > \"{}\"\n  exec /bin/sleep 2\nfi\n",
            pid_path.display()
        ),
    );
    let started = Instant::now();
    assert_error(
        &fixture.run(&[
            "wait",
            CLAUDE_IDLE,
            "--provider",
            "claude",
            "--timeout",
            "0.1",
            "--json",
        ]),
        124,
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let pid = std::fs::read_to_string(pid_path).expect("read slow provider pid");
    thread::sleep(Duration::from_millis(50));
    let alive = Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if alive {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    assert!(!alive, "wait timeout leaked provider pid {pid}");
}

#[test]
#[cfg(target_os = "linux")]
fn abnormal_wait_worker_exit_is_a_json_backend_error_and_reaps_provider() {
    let fixture = CliFixture::empty();
    let worker_pid = fixture.home.join("abnormal-worker-pid");
    let provider_pid = fixture.home.join("abnormal-provider-pid");
    write_executable(
        &fixture.bin.join("claude"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = agents ]; then\n  printf '%s' \"$PPID\" > \"{}\"\n  printf '%s' \"$$\" > \"{}\"\n  /bin/kill -KILL \"$PPID\"\n  exec /bin/sleep 5\nfi\n",
            worker_pid.display(),
            provider_pid.display()
        ),
    );

    let output = fixture.run(&[
        "wait",
        "any-session",
        "--provider",
        "claude",
        "--timeout",
        "2",
        "--json",
    ]);
    let pids = wait_for_pid_files(&[&worker_pid, &provider_pid]);
    if pids.len() != 2 {
        assert_processes_gone(&pids);
    }
    assert_eq!(
        pids.len(),
        2,
        "provider did not record worker and provider pids"
    );
    assert_processes_gone(&pids);
    assert_error_kind(&output, 1, "backend");
}

#[test]
#[cfg(target_os = "linux")]
fn killing_wait_supervisor_terminates_its_worker_and_provider() {
    let fixture = CliFixture::empty();
    let worker_pid = fixture.home.join("killed-worker-pid");
    let provider_pid = fixture.home.join("killed-provider-pid");
    write_executable(
        &fixture.bin.join("claude"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = agents ]; then\n  printf '%s' \"$PPID\" > \"{}\"\n  printf '%s' \"$$\" > \"{}\"\n  exec /bin/sleep 5\nfi\n",
            worker_pid.display(),
            provider_pid.display()
        ),
    );
    let mut command = fixture.command();
    command
        .args([
            "wait",
            "any-session",
            "--provider",
            "claude",
            "--timeout",
            "4",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut supervisor = command.spawn().expect("spawn wait supervisor");
    let pids = wait_for_pid_files(&[&worker_pid, &provider_pid]);
    let _ = supervisor.kill();
    let reaped = reap_bounded(&mut supervisor, Duration::from_secs(1));
    if pids.len() != 2 || reaped.is_none() {
        assert_processes_gone(&pids);
    }
    assert_eq!(
        pids.len(),
        2,
        "provider did not record worker and provider pids"
    );
    assert!(
        reaped.is_some(),
        "killed supervisor was not reaped promptly"
    );
    assert_processes_gone(&pids);
}

#[test]
#[cfg(unix)]
fn destructive_archive_requires_opt_in_and_delegates_to_provider_cli() {
    let fixture = CliFixture::with_sessions();
    assert_error(
        &fixture.run(&["archive", CLAUDE_DONE, "--provider", "claude", "--json"]),
        6,
    );
    assert!(!fixture.calls.exists());
    let accepted = fixture.run(&[
        "archive",
        CLAUDE_DONE,
        "--provider",
        "claude",
        "--allow-delete",
        "--json",
    ]);
    assert_exit(&accepted, 0);
    assert_eq!(
        parse_json(&accepted),
        json!({"schema_version": 1, "command": "archive", "provider": "claude", "id": CLAUDE_DONE, "target": format!("claude:{CLAUDE_DONE}"), "outcome": "accepted"})
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.calls).unwrap(),
        "rm\ndone0001\n"
    );
    assert_error(
        &fixture.run(&["unarchive", CLAUDE_DONE, "--provider", "claude", "--json"]),
        5,
    );
}

#[test]
#[cfg(unix)]
fn mutations_invalidate_shared_listing_cache_on_success_and_provider_failure() {
    let succeeded = CliFixture::with_sessions();
    let (success_db, success_scope) = seed_codex_listing_cache(&succeeded);
    let output = succeeded.run(&["unarchive", HIDDEN, "--provider", "codex", "--json"]);
    assert_exit(&output, 0);
    assert!(matches!(
        success_db
            .read_listing_snapshot(&success_scope, 12)
            .expect("read cache after successful mutation"),
        ListingCacheRead::Stale(_)
    ));

    let failed = CliFixture::with_sessions();
    let (failure_db, failure_scope) = seed_codex_listing_cache(&failed);
    write_executable(&failed.bin.join("codex"), "#!/bin/sh\nexit 7\n");
    let output = failed.run(&["unarchive", HIDDEN, "--provider", "codex", "--json"]);
    assert_error_kind(&output, 1, "backend");
    assert!(matches!(
        failure_db
            .read_listing_snapshot(&failure_scope, 12)
            .expect("read cache after failed mutation"),
        ListingCacheRead::Stale(_)
    ));
}

#[test]
#[cfg(unix)]
fn stop_rename_unarchive_and_qualified_targets_use_authoritative_provider_paths() {
    let fixture = CliFixture::with_sessions();
    let stopped = fixture.run(&["stop", CLAUDE_IDLE, "--provider", "claude", "--json"]);
    assert_exit(&stopped, 0);
    assert_eq!(
        parse_json(&stopped)["target"],
        format!("claude:{CLAUDE_IDLE}")
    );

    let state = fixture.claude_config.join("jobs/done0001/state.json");
    std::fs::create_dir_all(state.parent().unwrap()).expect("create Claude job directory");
    std::fs::write(
        &state,
        r#"{"detail":"preserve me","respawnFlags":["--model","opus"]}"#,
    )
    .expect("write Claude job state");
    let qualified = format!("claude:{CLAUDE_DONE}");
    let renamed = fixture.run(&["rename", &qualified, "New Name", "--json"]);
    assert_exit(&renamed, 0);
    assert_eq!(parse_json(&renamed)["target"], qualified);
    let state: Value = serde_json::from_str(&std::fs::read_to_string(state).unwrap()).unwrap();
    assert_eq!(state["name"], "New Name");
    assert_eq!(state["nameSource"], "user");
    assert_eq!(state["detail"], "preserve me");
    assert_eq!(state["respawnFlags"], json!(["--model", "opus"]));

    let restored = fixture.run(&["unarchive", "22222222", "--provider", "codex", "--json"]);
    assert_exit(&restored, 0);
    let restored = parse_json(&restored);
    assert_eq!(restored["id"], HIDDEN);
    assert_eq!(restored["target"], format!("codex:{HIDDEN}"));
    assert_eq!(
        std::fs::read_to_string(&fixture.calls).unwrap(),
        format!("stop\nidle0001\nunarchive\n{HIDDEN}\n")
    );

    let conflict = CliFixture::empty();
    let target = format!("claude:{CLAUDE_IDLE}");
    assert_error(
        &conflict.run(&["stop", &target, "--provider", "codex", "--json"]),
        2,
    );
    assert!(!conflict.backend_init.exists());
}

#[test]
#[cfg(unix)]
fn invalid_options_and_unqualified_mutations_fail_before_backend_init() {
    let fixture = CliFixture::empty();
    assert_error(
        &fixture.run(&["list", "--provider", "invalid", "--json"]),
        2,
    );
    assert_error(
        &fixture.run(&["rename", DONE_A, "--provider", "codex", "--json"]),
        2,
    );
    assert_error(
        &fixture.run(&["rename", DONE_A, "", "--provider", "codex", "--json"]),
        2,
    );
    for timeout in ["NaN", "inf", "1e300"] {
        assert_error(
            &fixture.run(&["wait", DONE_A, "--timeout", timeout, "--json"]),
            2,
        );
    }
    assert_error(&fixture.run(&["stop", DONE_A, "--json"]), 2);
    assert!(!fixture.calls.exists());
    assert!(!fixture.backend_init.exists());
}

use agent_viewer_core::{Backend, GrokBackend, GrokLifecycle, Status};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is required")).join(".grok")
        })
}

fn exact_id(result: agent_viewer_core::SpawnResult, label: &str) -> String {
    let id = result
        .session_id
        .unwrap_or_else(|| panic!("{label} spawn returned no exact Grok session identity"));
    assert!(!id.trim().is_empty(), "{label} identity was empty");
    id
}

fn wait_for_session(
    lifecycle: &GrokLifecycle,
    id: &str,
    accepted: &[Status],
    timeout: Duration,
) -> agent_viewer_core::Session {
    let started = Instant::now();
    loop {
        if let Some(row) = lifecycle
            .list()
            .expect("live Grok roster")
            .into_iter()
            .find(|row| row.id == id && accepted.contains(&row.status))
        {
            return row;
        }
        assert!(
            started.elapsed() < timeout,
            "session {id} did not reach one of {accepted:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "requires the official authenticated Grok runtime"]
fn official_authenticated_two_session_lifecycle_and_configuration_discovery() {
    let version = Command::new("grok")
        .arg("--version")
        .output()
        .unwrap_or_else(|_| {
            panic!(
                "resume prerequisite: install the official grok binary on PATH and authenticate it"
            )
        });
    assert!(
        version.status.success(),
        "resume prerequisite: the official grok binary on PATH must run successfully"
    );
    let home = grok_home();
    let api_key_present = std::env::var_os("XAI_API_KEY").is_some_and(|value| !value.is_empty());
    assert!(
        home.join("auth.json").is_file() || api_key_present,
        "resume prerequisite: provide authenticated Grok owned auth.json or an authorized XAI_API_KEY"
    );

    let lifecycle = GrokLifecycle::new("grok", &home);
    let diagnostics = lifecycle
        .diagnostics()
        .expect("authenticated Grok diagnostics");
    assert!(diagnostics.binary_available);
    assert!(diagnostics.registered);
    assert!(diagnostics.leader_count > 0);
    assert!(!diagnostics.methods.is_empty());
    println!("grok live: binary authenticated and leader registered");

    let project = tempfile::tempdir().expect("throwaway Grok project");
    let first_id = exact_id(
        lifecycle
            .spawn(
                project.path(),
                "Use a shell tool to wait for 20 seconds, then answer with alpha complete.",
                None,
            )
            .expect("first live Grok spawn"),
        "first",
    );
    let second_id = exact_id(
        lifecycle
            .spawn(
                project.path(),
                "Use a shell tool to wait for 40 seconds, then answer with beta complete.",
                None,
            )
            .expect("second live Grok spawn"),
        "second",
    );
    assert_ne!(first_id, second_id);
    println!("grok live: created two distinct sessions");

    let live = [
        Status::Working,
        Status::Idle,
        Status::NeedsInput { reason: None },
    ];
    let first = wait_for_session(&lifecycle, &first_id, &live, Duration::from_secs(30));
    let second = wait_for_session(&lifecycle, &second_id, &live, Duration::from_secs(30));
    assert!(first.daemon_hosted && first.pid.is_none());
    assert!(second.daemon_hosted && second.pid.is_none());
    println!("grok live: both sessions observed through the leader roster");

    let tail = GrokBackend::new()
        .tail(&first, 20)
        .expect("live Grok transcript tail");
    assert!(
        !tail.is_empty(),
        "live Grok tail must contain the submitted turn"
    );
    println!("grok live: first session transcript tailed");

    lifecycle
        .cancel(&first_id)
        .expect("cancel selected session");
    let sibling = wait_for_session(&lifecycle, &second_id, &live, Duration::from_secs(10));
    assert!(live.contains(&sibling.status));
    println!("grok live: selected session cancelled while sibling remained live");

    let title = format!("agent viewer live {}", &first_id[..first_id.len().min(8)]);
    lifecycle
        .rename(&first_id, &title)
        .expect("rename live session");
    let renamed = lifecycle
        .list()
        .expect("listing after rename")
        .into_iter()
        .find(|row| row.id == first_id)
        .expect("renamed session remains listed");
    assert_eq!(renamed.title, title);
    println!("grok live: selected session renamed");

    let smoke_prompt = "Inspect only configuration already visible to this official runtime. Do not print secrets or file contents. Return exactly four newline separated records named INSTRUCTIONS, SHARED_SKILL, STDIO_MCP, and HTTP_MCP. Each record must name one real discovered item. Use MISSING when unavailable.";
    let resumed = Command::new("grok")
        .arg("--cwd")
        .arg(project.path())
        .arg("--resume")
        .arg(&first_id)
        .arg("-p")
        .arg(smoke_prompt)
        .output()
        .expect("resume selected Grok session");
    assert!(resumed.status.success(), "official Grok resume failed");
    let output = String::from_utf8(resumed.stdout).expect("Grok smoke output is UTF8");
    for marker in ["INSTRUCTIONS", "SHARED_SKILL", "STDIO_MCP", "HTTP_MCP"] {
        assert!(
            output.lines().any(|line| line.starts_with(marker)),
            "missing {marker} proof"
        );
    }
    assert!(
        !output.contains("MISSING"),
        "configuration discovery was incomplete"
    );
    println!(
        "grok live: resumed exact identity and discovered instructions, skill, and both MCP transports"
    );

    lifecycle
        .cancel(&second_id)
        .expect("cancel sibling cleanup");
    lifecycle
        .delete(&first_id)
        .expect("delete first disposable session");
    lifecycle
        .delete(&second_id)
        .expect("delete second disposable session");
    let remaining = lifecycle.list().expect("listing after cleanup");
    assert!(
        !remaining
            .iter()
            .any(|row| row.id == first_id || row.id == second_id)
    );
    println!("grok live: both disposable sessions deleted");
}

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

struct DisposableSessionCleanup<'a> {
    lifecycle: &'a GrokLifecycle,
    ids: Vec<String>,
    armed: bool,
}

impl<'a> DisposableSessionCleanup<'a> {
    fn new(lifecycle: &'a GrokLifecycle) -> DisposableSessionCleanup<'a> {
        DisposableSessionCleanup {
            lifecycle,
            ids: Vec::new(),
            armed: true,
        }
    }

    fn record(&mut self, id: &str) {
        self.ids.push(id.to_string());
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DisposableSessionCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for id in &self.ids {
            let _ = self.lifecycle.cancel(id);
        }
        for id in &self.ids {
            let _ = self.lifecycle.delete(id);
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InspectIdentities {
    project_instructions: Vec<InspectInstruction>,
    skills: Vec<InspectSkill>,
    mcp_servers: Vec<InspectMcpServer>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InspectInstruction {
    path: String,
    scope: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(serde::Deserialize)]
struct InspectSource {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(serde::Deserialize)]
struct InspectSkill {
    name: String,
    source: InspectSource,
    #[serde(default)]
    disabled: bool,
}

#[derive(serde::Deserialize)]
struct InspectMcpServer {
    name: String,
    transport: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default, rename = "disabledReason")]
    disabled_reason: Option<String>,
}

fn valid_inspect_identity(identity: &str) -> bool {
    !identity.trim().is_empty() && !identity.chars().any(char::is_control)
}

#[test]
#[ignore = "requires the official authenticated Grok runtime"]
fn official_authenticated_lifecycle_with_inspect_configuration_proof() {
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

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository = manifest_dir
        .parent()
        .and_then(|crates| crates.parent())
        .expect("Agent Viewer workspace root");
    assert!(
        repository.join("AGENTS.md").is_file(),
        "live official inspect proof must start from the Agent Viewer repository"
    );
    let project = tempfile::Builder::new()
        .prefix(".grok-live-")
        .tempdir_in(repository)
        .expect("throwaway Grok project inside repository");
    let inspect = Command::new("grok")
        .arg("inspect")
        .arg("--json")
        .current_dir(project.path())
        .output()
        .expect("run official Grok configuration inspection");
    assert!(
        inspect.status.success(),
        "resume prerequisite: official grok inspect must succeed in the repository child cwd"
    );
    let identities: InspectIdentities = serde_json::from_slice(&inspect.stdout)
        .expect("official grok inspect must return its documented JSON identity fields");
    let instruction = identities
        .project_instructions
        .iter()
        .find(|entry| {
            entry.scope == "project" && !entry.disabled && valid_inspect_identity(&entry.path)
        })
        .map(|entry| entry.path.clone());
    let shared_skill = identities
        .skills
        .iter()
        .find(|entry| {
            !entry.disabled
                && matches!(
                    entry.source.kind.as_str(),
                    "server" | "user" | "plugin" | "configToml" | "managed"
                )
                && valid_inspect_identity(&entry.name)
        })
        .map(|entry| entry.name.clone());
    let stdio_mcp = identities
        .mcp_servers
        .iter()
        .find(|entry| {
            !entry.disabled
                && entry.disabled_reason.is_none()
                && entry.transport == "stdio"
                && valid_inspect_identity(&entry.name)
        })
        .map(|entry| entry.name.clone());
    let http_mcp = identities
        .mcp_servers
        .iter()
        .find(|entry| {
            !entry.disabled
                && entry.disabled_reason.is_none()
                && entry.transport == "http"
                && valid_inspect_identity(&entry.name)
        })
        .map(|entry| entry.name.clone());
    let (instruction, shared_skill, stdio_mcp, http_mcp) = match (
        instruction,
        shared_skill,
        stdio_mcp,
        http_mcp,
    ) {
        (Some(instruction), Some(shared_skill), Some(stdio_mcp), Some(http_mcp)) => {
            (instruction, shared_skill, stdio_mcp, http_mcp)
        }
        _ => panic!(
            "resume prerequisite: official grok inspect must expose one enabled project instruction, one shared nonbundled skill, one stdio MCP server, and one HTTP MCP server"
        ),
    };
    println!(
        "grok live: official inspect proved project instructions, a shared skill, and both MCP transports"
    );

    let lifecycle = GrokLifecycle::new("grok", &home);
    let mut cleanup = DisposableSessionCleanup::new(&lifecycle);
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
    cleanup.record(&first_id);
    let diagnostics = lifecycle
        .diagnostics()
        .expect("authenticated Grok diagnostics");
    assert!(diagnostics.binary_available);
    assert!(diagnostics.registered);
    assert!(diagnostics.leader_count > 0);
    assert!(!diagnostics.methods.is_empty());
    println!("grok live: binary authenticated and leader registered");

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
    cleanup.record(&second_id);
    assert_ne!(first_id, second_id);
    println!("grok live: created two distinct sessions");

    let working = [Status::Working];
    let first = wait_for_session(&lifecycle, &first_id, &working, Duration::from_secs(30));
    let second = wait_for_session(&lifecycle, &second_id, &working, Duration::from_secs(30));
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
    let sibling = wait_for_session(&lifecycle, &second_id, &working, Duration::from_secs(10));
    assert_eq!(sibling.status, Status::Working);
    println!("grok live: selected session cancelled while sibling remained live");

    let settled = [Status::Idle, Status::NeedsInput { reason: None }];
    let selected = wait_for_session(&lifecycle, &first_id, &settled, Duration::from_secs(30));
    assert!(settled.contains(&selected.status));
    println!("grok live: selected session settled after cancellation");

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

    let expected_records = [
        format!("INSTRUCTIONS={instruction}"),
        format!("SHARED_SKILL={shared_skill}"),
        format!("STDIO_MCP={stdio_mcp}"),
        format!("HTTP_MCP={http_mcp}"),
    ];
    // Official inspect above is the configuration discovery proof. This fixed echo turn only
    // proves that the authenticated lifecycle can resume the exact selected session.
    let resume_prompt = format!(
        "Acknowledge the four configuration identities already validated by the official inspect command. Do not inspect files, invoke tools, print secrets, or print file contents. Return exactly these four newline separated records with no extra text:\n{}",
        expected_records.join("\n")
    );
    let resumed = Command::new("grok")
        .arg("--cwd")
        .arg(project.path())
        .arg("--resume")
        .arg(&first_id)
        .arg("-p")
        .arg(&resume_prompt)
        .output()
        .expect("resume selected Grok session");
    if !resumed.status.success() {
        let failure = format!(
            "{}\n{}",
            String::from_utf8_lossy(&resumed.stdout),
            String::from_utf8_lossy(&resumed.stderr)
        )
        .to_ascii_lowercase();
        if ["usage limit", "rate limit", "quota", "credits"]
            .iter()
            .any(|needle| failure.contains(needle))
        {
            panic!(
                "resume prerequisite: authenticated Grok usage must allow one resume acknowledgement turn"
            );
        }
        panic!("official Grok authenticated resume acknowledgement failed");
    }
    let output = std::str::from_utf8(&resumed.stdout)
        .unwrap_or_else(|_| panic!("official Grok resume acknowledgement was not UTF8"));
    for marker in ["INSTRUCTIONS", "SHARED_SKILL", "STDIO_MCP", "HTTP_MCP"] {
        assert!(
            output.lines().any(|line| line.starts_with(marker)),
            "missing {marker} resume acknowledgement record"
        );
    }
    assert!(
        !output.contains("MISSING"),
        "resume acknowledgement omitted an identity already proved by official inspect"
    );
    let actual_records = output.lines().collect::<Vec<_>>();
    let expected_records = expected_records
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert!(
        actual_records.len() == expected_records.len()
            && actual_records
                .iter()
                .zip(&expected_records)
                .all(|(actual, expected)| actual == expected),
        "resume acknowledgement did not match the identities proved by official inspect"
    );
    assert!(
        actual_records.iter().all(|record| {
            record
                .split_once('=')
                .is_some_and(|(_, value)| !value.is_empty())
        }),
        "resume acknowledgement contained an empty inspected identity"
    );
    println!("grok live: authenticated resume echoed the identities proved by official inspect");

    let completed = wait_for_session(
        &lifecycle,
        &first_id,
        &[Status::Done],
        Duration::from_secs(30),
    );
    assert_eq!(completed.status, Status::Done);
    println!("grok live: resumed session reached terminal completion");

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
    cleanup.disarm();
    println!("grok live: both disposable sessions deleted");
}

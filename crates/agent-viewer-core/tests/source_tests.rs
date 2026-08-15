use agent_viewer_core::codex::source::Source;

#[test]
fn parse_plain_variants() {
    assert_eq!(Source::parse("cli"), Source::Cli);
    assert_eq!(Source::parse("exec"), Source::Exec);
    assert_eq!(Source::parse("vscode"), Source::VsCode);
}

#[test]
fn parse_subagent_string() {
    assert_eq!(
        Source::parse(r#"{"subagent":"review"}"#),
        Source::Subagent("review".to_string())
    );
    assert_eq!(
        Source::parse(r#"{"subagent":"memory_consolidation"}"#),
        Source::Subagent("memory_consolidation".to_string())
    );
}

#[test]
fn parse_subagent_thread_spawn() {
    // A nested spawn belongs to a subagent for safety, but its parent link makes it a
    // primary row in the default view.
    let raw = r#"{"subagent":{"thread_spawn":{"parent_thread_id":"019f4dda","depth":1,"agent_path":null,"agent_nickname":"Aristotle","agent_role":"worker"}}}"#;
    assert_eq!(
        Source::parse(raw),
        Source::ThreadSpawn {
            nickname: "Aristotle".to_string(),
            parent_thread_id: "019f4dda".to_string(),
        }
    );
    assert!(
        !Source::parse(raw).is_companion(),
        "a spawned agent with a real parent thread must be visible"
    );
}

#[test]
fn malformed_thread_spawn_stays_a_companion() {
    let empty_parent = r#"{"subagent":{"thread_spawn":{"parent_thread_id":""}}}"#;
    let non_string_parent = r#"{"subagent":{"thread_spawn":{"parent_thread_id":7}}}"#;

    assert!(Source::parse(empty_parent).is_companion());
    assert!(Source::parse(non_string_parent).is_companion());
}

#[test]
fn parse_garbage_never_panics() {
    assert_eq!(
        Source::parse("banana"),
        Source::Subagent("unknown".to_string())
    );
    assert_eq!(Source::parse("{"), Source::Subagent("unknown".to_string()));
    assert_eq!(
        Source::parse(r#"{"other":1}"#),
        Source::Subagent("unknown".to_string())
    );
}

// --- v2 companion predicate (test 12) ---

#[test]
fn companion_flags_by_source() {
    // Cli / VsCode are shown; Exec and bare subagents are companions.
    assert!(!Source::Cli.is_companion());
    assert!(!Source::VsCode.is_companion());
    assert!(Source::Exec.is_companion());
    assert!(Source::Subagent("review".to_string()).is_companion());
    assert!(Source::Subagent("Aristotle".to_string()).is_companion());
}

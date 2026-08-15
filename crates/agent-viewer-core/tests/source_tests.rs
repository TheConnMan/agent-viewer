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
    // Verbatim shape from the live DB: nested thread_spawn, label = agent_nickname.
    let raw = r#"{"subagent":{"thread_spawn":{"parent_thread_id":"019f4dda","depth":1,"agent_path":null,"agent_nickname":"Aristotle","agent_role":"worker"}}}"#;
    assert_eq!(
        Source::parse(raw),
        Source::Subagent("Aristotle".to_string())
    );
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
    // Cli, VsCode, and Exec are shown; Subagents are companions (hidden by default).
    assert!(!Source::Cli.is_companion());
    assert!(!Source::VsCode.is_companion());
    assert!(!Source::Exec.is_companion());
    assert!(Source::Subagent("review".to_string()).is_companion());
    assert!(Source::Subagent("Aristotle".to_string()).is_companion());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Cli,
    Exec,
    VsCode,
    Subagent(String),
}

impl Source {
    /// Never fails, never panics. Exact rules:
    /// - "cli" -> Cli, "exec" -> Exec, "vscode" -> VsCode (exact match after trim).
    /// - Else parse as JSON. Object with key "subagent":
    ///   - string value -> Subagent(that string) // "review"
    ///   - object with "thread_spawn" -> Subagent(agent_nickname.unwrap_or("spawn")) // "Aristotle"
    ///   - other shape -> Subagent("subagent")
    /// - Anything else (non-JSON, other JSON) -> Subagent("unknown".into())
    pub fn parse(raw: &str) -> Source {
        let _ = raw;
        todo!()
    }
    /// "cli" | "exec" | "vscode" | the Subagent label. For display.
    pub fn label(&self) -> &str {
        todo!()
    }
}

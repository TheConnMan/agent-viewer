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
        let trimmed = raw.trim();
        match trimmed {
            "cli" => return Source::Cli,
            "exec" => return Source::Exec,
            "vscode" => return Source::VsCode,
            _ => {}
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return Source::Subagent("unknown".into());
        };
        let Some(sub) = value.get("subagent") else {
            return Source::Subagent("unknown".into());
        };
        if let Some(label) = sub.as_str() {
            return Source::Subagent(label.to_string());
        }
        if let Some(spawn) = sub.get("thread_spawn") {
            let nickname = spawn
                .get("agent_nickname")
                .and_then(|n| n.as_str())
                .unwrap_or("spawn");
            return Source::Subagent(nickname.to_string());
        }
        Source::Subagent("subagent".into())
    }

    /// "cli" | "exec" | "vscode" | the Subagent label. For display.
    pub fn label(&self) -> &str {
        match self {
            Source::Cli => "cli",
            Source::Exec => "exec",
            Source::VsCode => "vscode",
            Source::Subagent(label) => label,
        }
    }
}

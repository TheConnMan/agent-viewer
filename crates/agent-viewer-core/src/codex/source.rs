#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Cli,
    Exec,
    VsCode,
    Subagent(String),
    ThreadSpawn {
        nickname: String,
        parent_thread_id: String,
    },
}

impl Source {
    /// Never fails, never panics. Exact rules:
    /// - "cli" -> Cli, "exec" -> Exec, "vscode" -> VsCode (exact match after trim).
    /// - Else parse as JSON. Object with key "subagent":
    ///   - string value -> Subagent(that string) // "review"
    ///   - valid object with "thread_spawn" -> ThreadSpawn
    ///   - malformed object with "thread_spawn" -> Subagent(agent_nickname.unwrap_or("spawn"))
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
            let parent_thread_id = spawn
                .get("parent_thread_id")
                .and_then(|parent| parent.as_str())
                .map(str::trim)
                .filter(|parent| !parent.is_empty());
            return match parent_thread_id {
                Some(parent_thread_id) => Source::ThreadSpawn {
                    nickname: nickname.to_string(),
                    parent_thread_id: parent_thread_id.to_string(),
                },
                None => Source::Subagent(nickname.to_string()),
            };
        }
        Source::Subagent("subagent".into())
    }

    /// Companion filter (verified on the live registry 2026-07-11: cli 25 +
    /// vscode 320 shown; exec and unlinked subagents hidden). A valid thread spawn is shown.
    /// thread_source/has_user_event are UNRELIABLE — do not use them.
    pub fn is_companion(&self) -> bool {
        matches!(self, Source::Exec | Source::Subagent(_))
    }

    pub fn is_subagent(&self) -> bool {
        matches!(self, Source::Subagent(_) | Source::ThreadSpawn { .. })
    }
}

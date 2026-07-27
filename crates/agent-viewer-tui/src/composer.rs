//! The inline spawn composer (task text + target backend + model/slash-command pickers),
//! the attached-mode `DetachTracker`, and the `subdir_names`/`file_stems` command-scan
//! helpers. Moved verbatim out of `app` to keep that module focused on the list model.

use agent_viewer_core::BackendKind;
use std::path::{Path, PathBuf};

/// Inline spawn composer (item 8): a persistent multiline input above the footer. Holds
/// the task text plus the target backend, which Tab cycles Claude -> Codex -> Opencode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    backend: BackendKind,
    /// The current per-spawn model selection for `self.backend`.
    model: String,
    /// Discovered model list for the current backend (default-first), caller-installed via
    /// `set_models`. Shift+Tab cycles it; `/model` filters it. Mirrors `commands`.
    models: Vec<String>,
    /// The backend the `models` list was installed for; the caller re-installs on a change.
    models_key: Option<BackendKind>,
    /// Slash-command names available for the current backend/target (scanned by the caller
    /// via `set_commands`), with the (backend, target) they were scanned for so the caller
    /// only re-scans on a change.
    commands: Vec<String>,
    commands_key: Option<(BackendKind, Option<PathBuf>)>,
    /// Highlighted suggestion index, and whether the popup was Esc-dismissed for this word.
    suggest_idx: usize,
    suggest_dismissed: bool,
}

/// Names of the immediate subdirectories of `dir` (claude skills). A missing/unreadable dir
/// yields an empty list, never an error.
pub fn subdir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// File stems (name without the final extension) of the files directly under `dir`
/// (opencode/codex commands). A missing/unreadable dir yields an empty list.
pub fn file_stems(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .collect()
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    /// Fresh composer: empty text, Claude backend, its default model. `models_key` is None so
    /// the first `ensure_models` call installs the real discovered Claude list.
    pub fn new() -> Composer {
        Composer {
            text: String::new(),
            backend: BackendKind::Claude,
            model: BackendKind::Claude.default_model().to_string(),
            models: vec![BackendKind::Claude.default_model().to_string()],
            models_key: None,
            commands: Vec::new(),
            commands_key: None,
            suggest_idx: 0,
            suggest_dismissed: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    /// The currently-selected model for the composer's backend.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The backend the current `models` list was installed for; the caller re-installs via
    /// `set_models` only when this changes (mirrors `commands_key`).
    pub fn models_key(&self) -> Option<BackendKind> {
        self.models_key
    }

    /// Install the discovered model list (default-first) for `backend`, selecting index 0.
    /// The one exception is a re-install for the SAME backend that still offers the current
    /// selection: discovery lands asynchronously, and a catalog arriving mid-compose must not
    /// silently rewrite a model the user deliberately picked.
    pub fn set_models(&mut self, models: Vec<String>, backend: BackendKind) {
        let keeps_selection = self.models_key == Some(backend) && models.contains(&self.model);
        if !keeps_selection {
            self.model = models
                .first()
                .cloned()
                .unwrap_or_else(|| backend.default_model().to_string());
        }
        self.models = models;
        self.models_key = Some(backend);
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn push_char(&mut self, c: char) {
        self.text.push(c);
        // Editing the command word re-opens a dismissed popup and resets the highlight.
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    /// Append pasted text as one draft, normalizing terminal line endings to `\n`.
    pub fn push_str(&mut self, text: &str) {
        self.text
            .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    /// Backspace on empty is a no-op (not a panic).
    pub fn backspace(&mut self) {
        self.text.pop();
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.suggest_idx = 0;
        self.suggest_dismissed = false;
    }

    /// Tab: Claude -> Codex -> Opencode -> Claude. The model list is left stale for this one
    /// step — `ensure_models` reinstalls (and resets to the default) at the end of the key
    /// handler before any render, so nothing observes the stale list.
    pub fn cycle_backend(&mut self) {
        self.backend = match self.backend {
            BackendKind::Claude => BackendKind::Codex,
            BackendKind::Codex => BackendKind::Opencode,
            BackendKind::Opencode => BackendKind::Claude,
        };
    }

    /// Shift+Tab: advance to the next discovered model after the current one, wrapping. A
    /// no-op for opencode (its list is huge — use the `/model` picker) or a <2-entry list.
    pub fn cycle_model(&mut self) {
        if self.backend == BackendKind::Opencode || self.models.len() < 2 {
            return;
        }
        let cur = self
            .models
            .iter()
            .position(|m| m == &self.model)
            .unwrap_or(0);
        let next = (cur + 1) % self.models.len();
        self.model = self.models[next].clone();
    }

    // --- Slash-command completion (v2.5) -----------------------------------------

    /// The (backend, target) the current command list was scanned for; the caller re-scans
    /// via `set_commands` only when this changes.
    pub fn commands_key(&self) -> Option<&(BackendKind, Option<PathBuf>)> {
        self.commands_key.as_ref()
    }

    /// Install the available slash-command names for the given (backend, target) scan key.
    pub fn set_commands(&mut self, commands: Vec<String>, key: (BackendKind, Option<PathBuf>)) {
        self.commands = commands;
        self.commands_key = Some(key);
        self.suggest_idx = 0;
    }

    /// The typed command word: text after a leading "/" up to the first space, or None when
    /// the text is not a bare "/word" (no slash, or a space already committed the command).
    fn command_word(&self) -> Option<&str> {
        let rest = self.text.strip_prefix('/')?;
        if rest.contains(' ') {
            return None; // a space means the command is chosen; stop completing
        }
        Some(rest)
    }

    /// Slash-command suggestions for the current word (prefix match, case-insensitive, capped
    /// at 8). Empty when the popup is dismissed or the text is not a bare "/word".
    pub fn suggestions(&self) -> Vec<&str> {
        // "/model" is the model meta-command: the slash popup stays closed so the two popups
        // never both show (the model picker takes over).
        if self.suggest_dismissed || self.is_model_command() {
            return Vec::new();
        }
        let Some(word) = self.command_word() else {
            return Vec::new();
        };
        let word = word.to_lowercase();
        self.commands
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&word))
            .map(String::as_str)
            .take(8)
            .collect()
    }

    pub fn suggestions_active(&self) -> bool {
        !self.suggestions().is_empty()
    }

    /// The length of whichever popup is currently active (slash-command or `/model` picker),
    /// so the shared highlight math tracks the right list.
    fn active_suggestion_len(&self) -> usize {
        if self.is_model_command() {
            self.model_suggestions().len()
        } else {
            self.suggestions().len()
        }
    }

    /// The clamped highlight index into the active popup's suggestions.
    pub fn suggestion_highlight(&self) -> usize {
        self.suggest_idx
            .min(self.active_suggestion_len().saturating_sub(1))
    }

    /// Up/Down within the active popup: wrap the highlight over its suggestions.
    pub fn move_suggestion(&mut self, delta: i32) {
        let n = self.active_suggestion_len();
        if n == 0 {
            return;
        }
        let cur = self.suggestion_highlight() as i32;
        self.suggest_idx = (cur + delta).rem_euclid(n as i32) as usize;
    }

    // --- /model picker (v2.6) ----------------------------------------------------

    /// Whether the composer text is the `/model` meta-command (bare or with a filter).
    pub fn is_model_command(&self) -> bool {
        self.text == "/model" || self.text.starts_with("/model ")
    }

    /// The filter typed after `/model` (empty for the bare command).
    fn model_filter(&self) -> &str {
        self.text
            .strip_prefix("/model")
            .map(str::trim)
            .unwrap_or("")
    }

    /// The discovered models matching the `/model` filter (case-insensitive substring, capped
    /// at 8). Empty unless the text is the `/model` command; an empty filter yields all.
    pub fn model_suggestions(&self) -> Vec<String> {
        if !self.is_model_command() {
            return Vec::new();
        }
        let filter = self.model_filter().to_lowercase();
        self.models
            .iter()
            .filter(|m| filter.is_empty() || m.to_lowercase().contains(&filter))
            .take(8)
            .cloned()
            .collect()
    }

    /// Whether the `/model` picker is currently showing suggestions.
    pub fn model_picking(&self) -> bool {
        !self.model_suggestions().is_empty()
    }

    /// Enter/Tab within the picker: set the model to the highlighted suggestion and clear the
    /// composer. Returns false when there is nothing to accept.
    pub fn accept_model(&mut self) -> bool {
        let Some(model) = self
            .model_suggestions()
            .get(self.suggestion_highlight())
            .cloned()
        else {
            return false;
        };
        self.model = model;
        self.clear();
        true
    }

    /// Tab within the popup: replace the text with "/name " for the highlighted command.
    /// Returns false when there is nothing to accept.
    pub fn accept_suggestion(&mut self) -> bool {
        let Some(name) = self
            .suggestions()
            .get(self.suggestion_highlight())
            .map(|s| s.to_string())
        else {
            return false;
        };
        self.text = format!("/{name} ");
        self.suggest_idx = 0;
        true
    }

    /// Esc within the popup: hide it without clearing the text (a second Esc clears as usual).
    pub fn dismiss_suggestions(&mut self) {
        self.suggest_dismissed = true;
    }
}

/// Tracks typed-but-unsubmitted input while attached, so a Left arrow detaches only when
/// the input line is empty (otherwise Left is forwarded to the child as cursor movement).
#[derive(Debug, Clone, Default)]
pub struct DetachTracker {
    pending: u32,
}

impl DetachTracker {
    pub fn new() -> DetachTracker {
        DetachTracker { pending: 0 }
    }

    pub fn on_char(&mut self) {
        self.pending += 1;
    }

    /// Backspace saturates at zero (no underflow).
    pub fn on_backspace(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    /// Enter (submit) resets the pending count.
    pub fn on_enter(&mut self) {
        self.pending = 0;
    }

    /// Left detaches only when there is no pending input.
    pub fn detach_on_left(&self) -> bool {
        self.pending == 0
    }
}

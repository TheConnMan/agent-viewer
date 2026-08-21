//! The inline spawn composer (task text + target backend + model/slash-command pickers),
//! the attached-mode `DetachTracker`, and the `subdir_names`/`file_stems` command-scan
//! helpers. Moved verbatim out of `app` to keep that module focused on the list model.

use agent_viewer_core::BackendKind;
use agent_viewer_core::router::AUTO_MODEL;
use std::path::{Path, PathBuf};

/// What an entry invokes. The owner is stored separately because viewer commands have no
/// provider and a provider can expose more than one command kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandKind {
    Viewer,
    Skill,
    Prompt,
}

impl CommandKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Viewer => "viewer command",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
        }
    }
}

/// One completion shared by the inline popup and Ctrl+K palette. `insertion` is literal:
/// accepting an entry copies it unchanged, while the retained owner prevents Auto from
/// silently sending that text to another provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEntry {
    name: String,
    insertion: String,
    owner: Option<BackendKind>,
    kind: CommandKind,
    codex_skill_path: Option<PathBuf>,
}

impl CommandEntry {
    pub fn viewer(name: impl Into<String>) -> Self {
        Self::new(name, None, CommandKind::Viewer, '/', None)
    }

    pub fn claude_skill(name: impl Into<String>) -> Self {
        Self::new(
            name,
            Some(BackendKind::Claude),
            CommandKind::Skill,
            '/',
            None,
        )
    }

    pub fn codex_prompt(name: impl Into<String>) -> Self {
        Self::new(
            name,
            Some(BackendKind::Codex),
            CommandKind::Prompt,
            '/',
            None,
        )
    }

    pub fn codex_skill(name: impl Into<String>, path: PathBuf) -> Self {
        Self::new(
            name,
            Some(BackendKind::Codex),
            CommandKind::Skill,
            '$',
            Some(path),
        )
    }

    fn new(
        name: impl Into<String>,
        owner: Option<BackendKind>,
        kind: CommandKind,
        sigil: char,
        codex_skill_path: Option<PathBuf>,
    ) -> Self {
        let name = name.into();
        Self {
            insertion: format!("{sigil}{name} "),
            name,
            owner,
            kind,
            codex_skill_path,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn insertion(&self) -> &str {
        &self.insertion
    }

    pub fn display(&self) -> &str {
        self.insertion.trim_end()
    }

    pub fn owner(&self) -> Option<BackendKind> {
        self.owner
    }

    pub fn kind(&self) -> CommandKind {
        self.kind
    }

    pub fn codex_skill_path(&self) -> Option<&Path> {
        self.codex_skill_path.as_deref()
    }

    pub fn detail(&self, catalog: &[CommandEntry]) -> String {
        let mut detail = match self.owner {
            Some(owner) => format!("{} {}", owner.name(), self.kind.label()),
            None => self.kind.label().to_string(),
        };
        if let Some(scope) = self.distinguishing_scope(catalog) {
            detail.push_str("  scope ");
            detail.push_str(&scope);
        }
        detail
    }

    fn distinguishing_scope(&self, catalog: &[CommandEntry]) -> Option<String> {
        let path = self.codex_skill_path()?;
        let duplicates = catalog
            .iter()
            .filter(|entry| {
                entry.name == self.name
                    && entry.owner == self.owner
                    && entry.kind == self.kind
                    && entry.codex_skill_path().is_some_and(|other| other != path)
            })
            .filter_map(CommandEntry::codex_skill_path)
            .collect::<Vec<_>>();
        if duplicates.is_empty() {
            return None;
        }

        let own_parts = path_parts(path);
        let duplicate_parts = duplicates
            .iter()
            .map(|path| path_parts(path))
            .collect::<Vec<_>>();
        let common_suffix = (0..own_parts.len())
            .take_while(|offset| {
                duplicate_parts.iter().all(|parts| {
                    parts.len().checked_sub(offset + 1).is_some_and(|index| {
                        parts[index] == own_parts[own_parts.len() - offset - 1]
                    })
                })
            })
            .count();
        let own_scope = &own_parts[..own_parts.len().saturating_sub(common_suffix)];
        let duplicate_scopes = duplicate_parts
            .iter()
            .map(|parts| &parts[..parts.len().saturating_sub(common_suffix.min(parts.len()))])
            .collect::<Vec<_>>();

        for width in 1..=own_scope.len() {
            let own_start = own_scope.len().saturating_sub(width);
            let own_suffix = &own_scope[own_start..];
            let unique = duplicate_scopes.iter().all(|parts| {
                let start = parts.len().saturating_sub(width);
                &parts[start..] != own_suffix
            });
            if unique {
                return Some(format!("…/{}", own_suffix.join("/")));
            }
        }

        Some(path.to_string_lossy().into_owned())
    }

    fn available_for(&self, auto: bool, backend: BackendKind) -> bool {
        self.owner.is_none() || auto || self.owner == Some(backend)
    }
}

fn path_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| {
            let part = component.as_os_str().to_string_lossy();
            (!part.is_empty() && part != "/").then(|| part.into_owned())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnRoute {
    DirectBackend,
    Router,
}
/// Inline spawn composer (item 8): a persistent multiline input above the footer. Holds
/// the task text plus the installed target backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    backend: BackendKind,
    available_backends: Vec<BackendKind>,
    /// Whether the selector sits on the Auto entry, where `agent-router` picks the provider.
    /// Auto is not a `BackendKind` (it lists nothing and owns no sessions), so it rides
    /// alongside the concrete selection rather than replacing it.
    auto: bool,
    /// Whether the Auto entry is offered by the Tab cycle at all: the caller installs this
    /// once at startup from the `agent-router` PATH lookup. A missing router means the entry
    /// never appears.
    auto_available: bool,
    /// The current per-spawn model selection for `self.backend`.
    model: String,
    /// Discovered model list for the current backend (default-first), caller-installed via
    /// `set_models`. Shift+Tab cycles it; `/model` filters it. Mirrors `commands`.
    models: Vec<String>,
    /// The backend the `models` list was installed for; the caller re-installs on a change.
    models_key: Option<BackendKind>,
    /// Commands available for the current backend/target (discovered by the caller
    /// via `set_commands`), with the (backend, target) they were scanned for so the caller
    /// only reinstalls on a change.
    commands: Vec<CommandEntry>,
    commands_key: Option<(BackendKind, Option<PathBuf>)>,
    /// Auto and a concrete backend can share the same underlying backend key but require
    /// different command sets, so the installed scope is tracked independently.
    commands_auto: bool,
    /// The exact accepted entry. It remains authoritative only while its insertion remains
    /// the prefix and the backend selection has not changed.
    pinned_command: Option<CommandEntry>,
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
/// (codex commands). A missing/unreadable dir yields an empty list.
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
    /// Fresh composer with all backends available for isolated tests. Startup replaces this
    /// list with host discovery before the first render.
    pub fn new() -> Composer {
        Composer {
            text: String::new(),
            backend: BackendKind::Claude,
            available_backends: vec![BackendKind::Claude, BackendKind::Codex],
            auto: false,
            auto_available: false,
            model: BackendKind::Claude.default_model().to_string(),
            models: vec![BackendKind::Claude.default_model().to_string()],
            models_key: None,
            commands: Vec::new(),
            commands_key: None,
            commands_auto: false,
            pinned_command: None,
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

    pub fn available_backends(&self) -> &[BackendKind] {
        &self.available_backends
    }

    pub fn set_available_backends(&mut self, backends: Vec<BackendKind>) {
        self.available_backends = backends;
        if let Some(first) = self.available_backends.first().copied()
            && !self.available_backends.contains(&self.backend)
        {
            self.pinned_command = None;
            self.backend = first;
            self.model = first.default_model().to_string();
            self.models = vec![first.default_model().to_string()];
            self.models_key = None;
            self.commands_key = None;
        }
    }

    /// Whether the selector is on Auto, where the spawn goes through `agent-router` instead of
    /// a backend. Every `backend()` caller that would act on the selection checks this first.
    pub fn is_auto(&self) -> bool {
        self.auto
    }

    /// Whether `agent-router` was found on PATH at startup. It gates more than the Auto entry:
    /// a spawn on a concrete backend routes through the router too (pinned with `--provider`)
    /// whenever this is true, except for Grok until Router accepts that provider. A Codex exec
    /// opt-in also stays direct.
    pub fn router_available(&self) -> bool {
        self.auto_available
    }

    pub fn spawn_route(&self, codex_exec_opt_in: bool) -> SpawnRoute {
        let routes_through_router = self.auto
            || (self.auto_available
                && self.backend != BackendKind::Grok
                && !(self.backend == BackendKind::Codex && codex_exec_opt_in));
        if routes_through_router {
            SpawnRoute::Router
        } else {
            SpawnRoute::DirectBackend
        }
    }

    /// Offer (or withdraw) the Auto entry. Withdrawing it while it is selected falls back to
    /// the concrete backend underneath, so the selector can never point at a missing router.
    pub fn set_auto_available(&mut self, available: bool) {
        self.auto_available = available;
        if !available && self.auto {
            self.pinned_command = None;
            self.auto = false;
        }
    }

    /// The provider label for the metadata row: "auto" or the backend's own name.
    pub fn provider_name(&self) -> &'static str {
        if self.auto {
            AUTO_MODEL
        } else {
            self.backend.name()
        }
    }

    /// Install the single-entry model list Auto offers. The router owns model and effort
    /// selection, so there is nothing for the user to pick and nothing to pass it.
    pub fn set_auto_model(&mut self) {
        self.model = AUTO_MODEL.to_string();
        self.models = vec![AUTO_MODEL.to_string()];
        // Deliberately left as the non-auto key it was: `models_key` tracks which BACKEND's
        // catalog is installed, so clearing it makes the next concrete selection reinstall.
        self.models_key = None;
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

    /// Install the discovered model list for `backend`, selecting index 0. When routing is
    /// available, one automatic choice is overlaid ahead of the raw provider catalog.
    /// The one exception is a re-install for the SAME backend that still offers the current
    /// selection: discovery lands asynchronously, and a catalog arriving mid-compose must not
    /// silently rewrite a model the user deliberately picked.
    pub fn set_models(&mut self, mut models: Vec<String>, backend: BackendKind) {
        if self.auto_available {
            models.retain(|model| {
                model != AUTO_MODEL
                    && !(backend == BackendKind::Codex && model == backend.default_model())
            });
            models.insert(0, AUTO_MODEL.to_string());
        }
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
        self.drop_invalid_pin();
        // Editing the command word re-opens a dismissed popup and resets the highlight.
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    /// Append pasted text as one draft, normalizing terminal line endings to `\n`.
    pub fn push_str(&mut self, text: &str) {
        self.text
            .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        self.drop_invalid_pin();
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    /// Backspace on empty is a no-op (not a panic).
    pub fn backspace(&mut self) {
        self.text.pop();
        self.drop_invalid_pin();
        self.suggest_dismissed = false;
        self.suggest_idx = 0;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.pinned_command = None;
        self.suggest_idx = 0;
        self.suggest_dismissed = false;
    }

    /// Tab advances through the available concrete backends, then Auto when offered.
    pub fn cycle_backend(&mut self) {
        self.pinned_command = None;
        if self.auto {
            if let Some(first) = self.available_backends.first().copied() {
                self.auto = false;
                self.backend = first;
            }
            return;
        }
        let Some(index) = self
            .available_backends
            .iter()
            .position(|backend| *backend == self.backend)
        else {
            return;
        };
        if let Some(next) = self.available_backends.get(index + 1).copied() {
            self.backend = next;
        } else if self.auto_available {
            self.auto = true;
        } else if let Some(first) = self.available_backends.first().copied() {
            self.backend = first;
        }
    }

    /// Start the selector on Auto when the router is offered: routed spawns are the default
    /// posture for a fresh composer, and one Tab returns to the concrete backends. Installs
    /// the single "auto" model entry too, since this runs at startup before any key handler
    /// reaches `ensure_models`.
    pub fn default_to_auto(&mut self) {
        if self.auto_available {
            self.pinned_command = None;
            self.auto = true;
            self.set_auto_model();
        }
    }

    /// Select `backend` outright (the Ctrl+K palette picking a model), which always LEAVES Auto:
    /// naming a model is a decision to use that provider, so Auto must stop routing even when the
    /// chosen backend is the one already sitting underneath Auto (where a Tab-cycle loop would
    /// exit immediately and leave Auto on, letting `ensure_models` restore the "auto" entry).
    pub fn select_backend(&mut self, backend: BackendKind) {
        self.pinned_command = None;
        self.auto = false;
        self.backend = backend;
    }

    /// Select a model only when it belongs to the installed catalog.
    pub fn select_model(&mut self, model: &str) -> bool {
        if !self.models.iter().any(|candidate| candidate == model) {
            return false;
        }
        self.model = model.to_string();
        true
    }

    /// Shift+Tab: advance to the next discovered model after the current one, wrapping. A
    /// no-op for a <2-entry list.
    pub fn cycle_model(&mut self) {
        if self.models.len() < 2 {
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

    /// Install the available commands for the given backend and target cache key.
    pub fn set_commands(
        &mut self,
        commands: Vec<CommandEntry>,
        key: (BackendKind, Option<PathBuf>),
    ) {
        let same_scope = self.commands_match_scope(&key);
        let highlighted = if same_scope {
            self.suggestions()
                .get(self.suggestion_highlight())
                .map(|command| (*command).clone())
        } else {
            None
        };
        self.commands = commands;
        self.commands_key = Some(key);
        self.commands_auto = self.auto;
        self.drop_invalid_pin();
        self.suggest_idx = highlighted
            .and_then(|selected| {
                self.suggestions()
                    .iter()
                    .position(|command| *command == &selected)
            })
            .unwrap_or(0);
    }

    pub fn commands(&self) -> &[CommandEntry] {
        &self.commands
    }

    pub fn commands_match_scope(&self, key: &(BackendKind, Option<PathBuf>)) -> bool {
        self.commands_key.as_ref() == Some(key) && self.commands_auto == self.auto
    }

    /// The typed command token, including its slash or dollar sigil. A space commits it and
    /// closes the completion popup.
    fn command_token(&self) -> Option<&str> {
        if !matches!(self.text.chars().next(), Some('/' | '$')) || self.text.contains(' ') {
            return None; // a space means the command is chosen; stop completing
        }
        Some(&self.text)
    }

    /// Command suggestions for the current word (prefix match, case-insensitive, capped at 8).
    /// Concrete Codex slash completion lists native slash commands before matching dollar-skill
    /// aliases. Empty when the popup is dismissed or the text is not a bare command word.
    pub fn suggestions(&self) -> Vec<&CommandEntry> {
        // "/model" is the model meta-command: the slash popup stays closed so the two popups
        // never both show (the model picker takes over).
        if self.suggest_dismissed || self.is_model_command() {
            return Vec::new();
        }
        let Some(token) = self.command_token() else {
            return Vec::new();
        };
        let token = token.to_lowercase();
        let native_matches = self.commands.iter().filter(|command| {
            command.available_for(self.auto, self.backend)
                && command.display().to_lowercase().starts_with(&token)
        });
        if !self.auto && self.backend == BackendKind::Codex && token.starts_with('/') {
            let alias = token.strip_prefix('/').expect("token starts with slash");
            native_matches
                .chain(self.commands.iter().filter(|command| {
                    command.owner == Some(BackendKind::Codex)
                        && command.kind == CommandKind::Skill
                        && command.name.to_lowercase().starts_with(alias)
                }))
                .take(8)
                .collect()
        } else {
            native_matches.take(8).collect()
        }
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

    pub fn is_theme_command(&self) -> bool {
        self.text.trim_end() == "/theme"
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

    /// Tab within the popup: replace the text with the entry's literal insertion and retain
    /// its owner for submission.
    /// Returns false when there is nothing to accept.
    pub fn accept_suggestion(&mut self) -> bool {
        let Some(command) = self
            .suggestions()
            .get(self.suggestion_highlight())
            .copied()
            .cloned()
        else {
            return false;
        };
        self.select_command(command);
        self.suggest_idx = 0;
        true
    }

    /// Insert a popup or palette entry. Viewer commands intentionally remain ownerless.
    pub fn select_command(&mut self, command: CommandEntry) {
        self.text = command.insertion.clone();
        self.pinned_command = command.owner.is_some().then_some(command);
        self.suggest_idx = 0;
        self.suggest_dismissed = false;
    }

    pub fn pinned_command(&self) -> Option<&CommandEntry> {
        self.pinned_command
            .as_ref()
            .filter(|command| self.pin_is_valid(command))
    }

    /// Resolve the command attached to the submitted text. An accepted entry wins even when
    /// another provider has the same insertion. Manually typed text infers an entry only when
    /// exactly one available entry has that literal prefix.
    pub fn command_for_submission(&self) -> Option<&CommandEntry> {
        if let Some(command) = self.pinned_command() {
            return Some(command);
        }
        let mut matches = self.commands.iter().filter(|command| {
            command.available_for(self.auto, self.backend)
                && self.text.starts_with(command.insertion())
        });
        let command = matches.next()?;
        matches.next().is_none().then_some(command)
    }

    fn drop_invalid_pin(&mut self) {
        if self
            .pinned_command
            .as_ref()
            .is_some_and(|command| !self.pin_is_valid(command))
        {
            self.pinned_command = None;
        }
    }

    fn pin_is_valid(&self, command: &CommandEntry) -> bool {
        self.text.starts_with(command.insertion())
            && command.available_for(self.auto, self.backend)
            && self.commands.contains(command)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_composer(commands: Vec<CommandEntry>) -> Composer {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        composer.default_to_auto();
        composer.set_commands(commands, (BackendKind::Claude, None));
        composer
    }

    #[test]
    fn theme_is_suggested_for_matching_prefix_on_every_backend() {
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut composer = Composer::new();
            while composer.backend() != backend {
                composer.cycle_backend();
            }
            let theme = CommandEntry::viewer("theme");
            composer.set_commands(
                vec![theme.clone()],
                (backend, Some(PathBuf::from("/tmp/project"))),
            );
            composer.push_str("/th");

            assert_eq!(composer.suggestions(), vec![&theme]);
        }
    }

    /// The Auto entry is gated: without a discoverable router the Tab cycle is exactly the
    /// backends it always was, and no spawn can be aimed at a missing binary.
    #[test]
    fn auto_is_absent_from_the_cycle_until_the_router_is_available() {
        let mut composer = Composer::new();
        for _ in 0..6 {
            composer.cycle_backend();
            assert!(!composer.is_auto(), "Auto must not appear ungated");
        }

        composer.set_auto_available(true);
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(composer.provider_name());
            composer.cycle_backend();
        }
        assert_eq!(seen, vec!["claude", "codex", "auto"]);
        assert_eq!(composer.provider_name(), "claude", "the cycle must wrap");
    }

    /// With the router installed the composer opens ON Auto: routed spawns are the default
    /// posture, and one Tab leads back to the concrete backends.
    #[test]
    fn a_fresh_composer_defaults_to_auto_when_the_router_is_offered() {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        composer.default_to_auto();

        assert!(composer.is_auto());
        assert_eq!(composer.provider_name(), "auto");
        assert_eq!(composer.model(), AUTO_MODEL);

        composer.cycle_backend();
        assert!(!composer.is_auto());
        assert_eq!(composer.provider_name(), "claude");
    }

    /// Without the router the default posture is unchanged: Claude, exactly as before Auto
    /// existed.
    #[test]
    fn defaulting_to_auto_without_the_router_stays_on_claude() {
        let mut composer = Composer::new();
        composer.default_to_auto();

        assert!(!composer.is_auto());
        assert_eq!(composer.provider_name(), "claude");
    }

    #[test]
    fn provider_cycle_only_includes_backends_available_on_the_host() {
        let mut composer = Composer::new();
        composer.set_available_backends(vec![BackendKind::Codex]);

        assert_eq!(composer.provider_name(), "codex");
        composer.cycle_backend();
        assert_eq!(
            composer.provider_name(),
            "codex",
            "a backend the host cannot spawn must never enter the cycle"
        );
    }

    #[test]
    fn auto_returns_to_the_first_available_backend() {
        let mut composer = Composer::new();
        composer.set_available_backends(vec![BackendKind::Codex, BackendKind::Claude]);
        composer.set_auto_available(true);
        composer.default_to_auto();

        assert_eq!(composer.provider_name(), "auto");
        composer.cycle_backend();
        assert_eq!(composer.provider_name(), "codex");
    }

    /// Losing the router while Auto is selected must fall back to a real backend rather than
    /// leaving the selector pointed at something that cannot spawn.
    #[test]
    fn withdrawing_the_router_deselects_auto() {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        while !composer.is_auto() {
            composer.cycle_backend();
        }

        composer.set_auto_available(false);

        assert!(!composer.is_auto());
        assert_eq!(composer.provider_name(), "codex");
    }

    /// Auto offers one model entry and no picker: the router chooses model and effort, so the
    /// viewer has nothing to pass it.
    #[test]
    fn auto_offers_a_single_model_entry_that_shift_tab_cannot_change() {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        while !composer.is_auto() {
            composer.cycle_backend();
        }
        composer.set_auto_model();

        assert_eq!(composer.model(), "auto");
        composer.cycle_model();
        assert_eq!(composer.model(), "auto");
        composer.push_str("/model");
        assert_eq!(composer.model_suggestions(), vec!["auto".to_string()]);
    }

    #[test]
    fn concrete_providers_default_to_auto_and_cycle_through_their_catalog() {
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut composer = Composer::new();
            composer.set_auto_available(true);
            composer.select_backend(backend);
            let explicit = if backend == BackendKind::Codex {
                "gpt".to_string()
            } else {
                backend.default_model().to_string()
            };
            let models = if backend == BackendKind::Codex {
                vec![backend.default_model().to_string(), explicit.clone()]
            } else {
                vec![explicit.clone()]
            };

            composer.set_models(models, backend);

            assert!(!composer.is_auto());
            assert_eq!(composer.backend(), backend);
            assert_eq!(composer.model(), AUTO_MODEL);
            composer.cycle_model();
            assert_eq!(composer.model(), explicit);
            composer.cycle_model();
            assert_eq!(composer.model(), AUTO_MODEL);
        }
    }

    #[test]
    fn concrete_provider_catalogs_stay_unchanged_without_the_router() {
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut composer = Composer::new();
            composer.select_backend(backend);
            let explicit = backend.default_model().to_string();

            composer.set_models(vec![explicit.clone()], backend);

            assert_eq!(composer.model(), explicit);
            composer.cycle_model();
            assert_eq!(composer.model(), explicit);
        }
    }

    #[test]
    fn an_explicit_model_survives_a_same_provider_catalog_refresh() {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        let explicit = BackendKind::Claude.default_model().to_string();
        composer.set_models(
            vec![explicit.clone(), "sonnet".to_string()],
            BackendKind::Claude,
        );
        composer.cycle_model();
        assert_eq!(composer.model(), explicit);

        composer.set_models(
            vec![explicit.clone(), "sonnet".to_string(), "haiku".to_string()],
            BackendKind::Claude,
        );

        assert_eq!(composer.model(), explicit);
    }

    #[test]
    fn a_routed_codex_catalog_has_auto_without_redundant_default() {
        let mut composer = Composer::new();
        composer.set_auto_available(true);
        composer.select_backend(BackendKind::Codex);
        composer.set_models(
            vec![
                AUTO_MODEL.to_string(),
                BackendKind::Codex.default_model().to_string(),
                "gpt".to_string(),
                AUTO_MODEL.to_string(),
            ],
            BackendKind::Codex,
        );
        composer.push_str("/model");

        assert_eq!(
            composer.model_suggestions(),
            vec![AUTO_MODEL.to_string(), "gpt".to_string()]
        );
    }

    #[test]
    fn a_routerless_codex_catalog_retains_default() {
        let mut composer = Composer::new();
        composer.select_backend(BackendKind::Codex);
        composer.set_models(
            vec![
                BackendKind::Codex.default_model().to_string(),
                "gpt".to_string(),
            ],
            BackendKind::Codex,
        );
        composer.push_str("/model");

        assert_eq!(
            composer.model_suggestions(),
            vec![
                BackendKind::Codex.default_model().to_string(),
                "gpt".to_string(),
            ]
        );
    }

    #[test]
    fn auto_offers_the_owned_union_and_keeps_duplicate_slash_entries_distinct() {
        let claude = CommandEntry::claude_skill("review");
        let prompt = CommandEntry::codex_prompt("review");
        let skill = CommandEntry::codex_skill("diagnose", PathBuf::from("/skills/diagnose"));
        let mut composer = auto_composer(vec![claude.clone(), prompt.clone(), skill.clone()]);

        composer.push_str("/re");
        assert_eq!(composer.suggestions(), vec![&claude, &prompt]);
        assert_eq!(claude.owner(), Some(BackendKind::Claude));
        assert_eq!(claude.kind(), CommandKind::Skill);
        assert_eq!(claude.insertion(), "/review ");
        assert_eq!(prompt.owner(), Some(BackendKind::Codex));
        assert_eq!(prompt.kind(), CommandKind::Prompt);
        assert_eq!(prompt.insertion(), "/review ");
        assert!(composer.is_auto());

        composer.clear();
        composer.push_str("/di");
        assert!(
            composer.suggestions().is_empty(),
            "Auto keeps slash commands separate from Codex dollar skills"
        );

        composer.clear();
        composer.push_str("$di");
        assert_eq!(composer.suggestions(), vec![&skill]);
        assert_eq!(skill.owner(), Some(BackendKind::Codex));
        assert_eq!(skill.kind(), CommandKind::Skill);
        assert_eq!(skill.insertion(), "$diagnose ");
        assert_eq!(
            skill.codex_skill_path(),
            Some(Path::new("/skills/diagnose"))
        );
        assert!(composer.accept_suggestion());
        assert_eq!(composer.text(), "$diagnose ");
        assert_eq!(composer.pinned_command(), Some(&skill));
        assert!(composer.is_auto());
    }

    #[test]
    fn accepting_duplicate_slash_entries_pins_the_highlighted_owner() {
        let claude = CommandEntry::claude_skill("review");
        let prompt = CommandEntry::codex_prompt("review");
        let mut composer = auto_composer(vec![claude, prompt.clone()]);
        composer.push_str("/re");

        composer.move_suggestion(1);
        assert!(composer.accept_suggestion());

        assert_eq!(composer.text(), "/review ");
        assert_eq!(composer.pinned_command(), Some(&prompt));
        assert_eq!(
            composer.command_for_submission(),
            Some(&prompt),
            "the duplicate insertion must retain the chosen provider"
        );
    }

    #[test]
    fn late_catalog_refresh_keeps_the_highlighted_entry_identity() {
        let claude = CommandEntry::claude_skill("review");
        let prompt = CommandEntry::codex_prompt("review");
        let mut composer = auto_composer(vec![claude.clone(), prompt.clone()]);
        composer.push_str("/re");
        composer.move_suggestion(1);
        assert_eq!(
            composer.suggestions()[composer.suggestion_highlight()],
            &prompt
        );

        let earlier = CommandEntry::claude_skill("refactor");
        composer.set_commands(
            vec![earlier, claude, prompt.clone()],
            (BackendKind::Claude, None),
        );

        assert_eq!(
            composer.suggestions()[composer.suggestion_highlight()],
            &prompt,
            "a landing catalog must preserve the selected provider and command kind"
        );
        assert!(composer.accept_suggestion());
        assert_eq!(composer.pinned_command(), Some(&prompt));
    }

    #[test]
    fn target_catalog_change_drops_a_pinned_project_codex_skill() {
        let first = CommandEntry::codex_skill(
            "review",
            PathBuf::from("/projects/first/.codex/skills/review/SKILL.md"),
        );
        let second = CommandEntry::codex_skill(
            "review",
            PathBuf::from("/projects/second/.codex/skills/review/SKILL.md"),
        );
        let mut composer = auto_composer(vec![first.clone()]);
        composer.push_str("$re");
        assert!(composer.accept_suggestion());
        assert_eq!(composer.pinned_command(), Some(&first));

        composer.set_commands(
            vec![second.clone()],
            (BackendKind::Claude, Some(PathBuf::from("/projects/second"))),
        );

        assert_eq!(composer.pinned_command(), None);
        assert_eq!(composer.command_for_submission(), Some(&second));
        assert_ne!(
            composer
                .command_for_submission()
                .and_then(CommandEntry::codex_skill_path),
            first.codex_skill_path()
        );

        composer.set_commands(
            vec![first.clone()],
            (BackendKind::Claude, Some(PathBuf::from("/projects/first"))),
        );

        assert_eq!(composer.pinned_command(), None);
        assert_eq!(composer.command_for_submission(), Some(&first));
    }

    #[test]
    fn codex_filters_the_union_to_its_prompt_and_dollar_skill_entries() {
        let claude = CommandEntry::claude_skill("review");
        let prompt = CommandEntry::codex_prompt("review");
        let skill = CommandEntry::codex_skill("diagnose", PathBuf::from("/skills/diagnose"));
        let mut composer = Composer::new();
        composer.select_backend(BackendKind::Codex);
        composer.set_commands(
            vec![claude, prompt.clone(), skill.clone()],
            (BackendKind::Codex, None),
        );

        composer.push_str("/re");
        assert_eq!(composer.suggestions(), vec![&prompt]);
        composer.clear();
        composer.push_str("$di");
        assert_eq!(composer.suggestions(), vec![&skill]);
    }

    #[test]
    fn concrete_codex_slash_skill_alias_inserts_native_dollar_command_and_path() {
        let skill = CommandEntry::codex_skill(
            "diagnose",
            PathBuf::from("/projects/viewer/.codex/skills/diagnose/SKILL.md"),
        );
        let model = CommandEntry::viewer("model");
        let theme = CommandEntry::viewer("theme");
        let prompt = CommandEntry::codex_prompt("review");
        let mut commands = vec![
            skill.clone(),
            CommandEntry::codex_skill("audit", PathBuf::from("/skills/audit/SKILL.md")),
            CommandEntry::codex_skill("bootstrap", PathBuf::from("/skills/bootstrap/SKILL.md")),
            CommandEntry::codex_skill("format", PathBuf::from("/skills/format/SKILL.md")),
            CommandEntry::codex_skill("lint", PathBuf::from("/skills/lint/SKILL.md")),
            CommandEntry::codex_skill("release", PathBuf::from("/skills/release/SKILL.md")),
            CommandEntry::codex_skill("summarize", PathBuf::from("/skills/summarize/SKILL.md")),
            CommandEntry::codex_skill("test", PathBuf::from("/skills/test/SKILL.md")),
            model.clone(),
            theme.clone(),
            prompt.clone(),
        ];
        commands.sort_by_key(|command| command.display().to_string());
        let mut composer = Composer::new();
        composer.select_backend(BackendKind::Codex);
        composer.set_commands(
            commands,
            (BackendKind::Codex, Some(PathBuf::from("/projects/viewer"))),
        );

        composer.push_str("/");
        let suggestions = composer.suggestions();
        for command in [&model, &theme, &prompt, &skill] {
            assert!(
                suggestions.contains(&command),
                "bare slash should retain {command:?} in a crowded Codex catalog"
            );
        }

        composer.clear();
        composer.push_str("/di");
        assert_eq!(composer.suggestions(), vec![&skill]);
        assert!(composer.accept_suggestion());
        assert_eq!(composer.text(), "$diagnose ");
        assert_eq!(composer.pinned_command(), Some(&skill));
        assert_eq!(
            composer
                .command_for_submission()
                .and_then(CommandEntry::codex_skill_path),
            Some(Path::new(
                "/projects/viewer/.codex/skills/diagnose/SKILL.md"
            ))
        );

        composer.clear();
        composer.push_str("$di");
        assert_eq!(composer.suggestions(), vec![&skill]);
    }

    #[test]
    fn a_pin_only_survives_while_its_exact_insertion_and_backend_selection_survive() {
        let command = CommandEntry::claude_skill("implement");
        let mut composer = auto_composer(vec![command.clone()]);
        composer.push_str("/im");
        assert!(composer.accept_suggestion());
        composer.push_str("fix the bug");
        assert_eq!(composer.pinned_command(), Some(&command));

        for _ in 0.."fix the bug".len() + 1 {
            composer.backspace();
        }
        assert_eq!(composer.text(), "/implement");
        assert_eq!(composer.pinned_command(), None);

        composer.clear();
        composer.push_str("/im");
        assert!(composer.accept_suggestion());
        composer.cycle_backend();
        assert!(!composer.is_auto());
        assert_eq!(composer.pinned_command(), None);

        composer.clear();
        assert_eq!(composer.pinned_command(), None);
        assert!(composer.command_for_submission().is_none());
    }

    #[test]
    fn manual_insertion_infers_only_one_owned_command() {
        let unique = CommandEntry::claude_skill("implement");
        let mut composer = auto_composer(vec![unique.clone()]);
        composer.push_str("/implement fix the bug");

        assert_eq!(composer.pinned_command(), None);
        assert_eq!(composer.command_for_submission(), Some(&unique));

        let claude = CommandEntry::claude_skill("review");
        let codex = CommandEntry::codex_prompt("review");
        composer.clear();
        composer.set_commands(
            vec![claude, codex],
            (BackendKind::Claude, Some(PathBuf::from("/tmp/project"))),
        );
        composer.push_str("/review this change");

        assert_eq!(composer.pinned_command(), None);
        assert_eq!(
            composer.command_for_submission(),
            None,
            "the same literal insertion on two providers must remain unowned"
        );
    }

    #[test]
    fn tab_accepts_theme_suggestion_as_theme_command() {
        let mut composer = Composer::new();
        let theme = CommandEntry::viewer("theme");
        composer.set_commands(vec![theme.clone()], (BackendKind::Claude, None));
        composer.push_str("/th");

        assert!(composer.accept_suggestion());
        assert!(composer.is_theme_command());
        assert_eq!(composer.text(), "/theme ");
        assert_eq!(composer.pinned_command(), None);
        assert_eq!(composer.command_for_submission(), Some(&theme));
        assert_eq!(theme.owner(), None);
        assert_eq!(theme.kind(), CommandKind::Viewer);
    }
}

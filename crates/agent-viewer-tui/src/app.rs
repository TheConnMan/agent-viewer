use agent_viewer_core::group::project_root;
use agent_viewer_core::{BackendKind, Session, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Grouping mode for the flat list. `ByProject` is the startup default (Ctrl+S toggles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    ByState,
    ByProject,
}

/// State sections in the ByState view. Failed + Stopped fold into Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    NeedsInput,
    Working,
    Idle,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// ByState mode.
    SectionHeader { section: Section, count: usize },
    /// ByProject mode.
    ProjectHeader { root: PathBuf, count: usize },
    Session {
        backend: BackendKind,
        id: String,
        title: String,
        summary: String,
        status: Status,
        hidden: bool,
        updated_at_ms: i64,
        pr_refs: Vec<String>,
    },
    /// A blank spacer line between groups/sections. Never selectable (skipped like headers).
    Spacer,
}

/// Result of a two-stage Ctrl+X press.
#[derive(Debug, Clone, PartialEq)]
pub enum KillStage {
    Stop,
    Remove,
    Noop,
}

/// How long an armed Ctrl+X stays live before the second press re-arms instead of removing.
const KILL_ARM_WINDOW_MS: i64 = 2_000;

pub struct App {
    sessions: Vec<Session>,
    selected: usize,
    filter: String,
    show_all: bool,
    group_mode: GroupMode,
    /// Cached render model. Rebuilt only when sessions/filter/show_all/group_mode
    /// change; cursor movement leaves it untouched.
    rows: Vec<Row>,
    /// Memoized project_root(cwd). cwds are stable, so this survives refresh ticks
    /// and stops re-walking the filesystem for sessions we have already grouped.
    root_cache: HashMap<PathBuf, PathBuf>,
    /// The armed (backend, id, armed_at_ms) for the two-stage Ctrl+X.
    armed_kill: Option<(BackendKind, String, i64)>,
    /// The row expanded in place for an inline peek (keyed by session). Held in App so the
    /// rebuild path can centrally collapse it the moment the selection diverges from it
    /// (regroup / show-all / filter / cursor move), never rendering the wrong transcript.
    expanded: Option<(BackendKind, String)>,
    /// Cached `hidden_count()` value, recomputed with the rows (it depends on exactly the
    /// same inputs), so the per-frame footer never re-filters the session list.
    hidden_rows: usize,
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        let mut app = App {
            sessions,
            selected: 0,
            filter: String::new(),
            show_all: false,
            group_mode: GroupMode::ByProject,
            rows: Vec::new(),
            root_cache: HashMap::new(),
            armed_kill: None,
            expanded: None,
            hidden_rows: 0,
        };
        app.rebuild_rows();
        app.clamp_selection();
        app
    }

    /// Replace the session set, keeping the current selection anchored to the same
    /// (backend,id) session row when it survives the refresh; otherwise clamp.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let anchor = self.selected().map(|s| (s.backend, s.id.clone()));
        self.sessions = sessions;
        self.rebuild_rows();
        if let Some(anchor) = anchor
            && self.select_by_key(&anchor)
        {
            return;
        }
        self.clamp_selection();
    }

    /// The render model (borrows the cached rows; rendered fresh every frame).
    pub fn visible(&self) -> &[Row] {
        &self.rows
    }

    /// key 'a' (I-2): one show-all toggle covering companions + archived rows.
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        self.rebuild_rows();
        self.clamp_selection();
    }

    pub fn show_all(&self) -> bool {
        self.show_all
    }

    /// Rows suppressed by the default view (companion or archived, when !show_all).
    /// Cached at rebuild time — see `hidden_rows`.
    pub fn hidden_count(&self) -> usize {
        self.hidden_rows
    }

    /// key Ctrl+S — toggle ByState / ByProject grouping.
    pub fn toggle_group_mode(&mut self) {
        self.group_mode = match self.group_mode {
            GroupMode::ByState => GroupMode::ByProject,
            GroupMode::ByProject => GroupMode::ByState,
        };
        self.rebuild_rows();
        self.clamp_selection();
    }

    pub fn group_mode(&self) -> GroupMode {
        self.group_mode
    }

    /// Two-stage Ctrl+X (list page only). Selected session S, injected now_ms:
    ///  - armed for (S.backend,S.id) and now-armed <= 2_000 -> clear, Remove
    ///  - else arm (S, now): S.status in {Working, NeedsInput} -> Stop,
    ///    else Noop (armed silently; footer shows the countdown hint)
    pub fn kill_stage(&mut self, now_ms: i64) -> KillStage {
        let Some((backend, id, status)) =
            self.selected().map(|s| (s.backend, s.id.clone(), s.status))
        else {
            return KillStage::Noop;
        };

        if let Some((armed_backend, armed_id, armed_at)) = &self.armed_kill
            && *armed_backend == backend
            && armed_id == &id
            && now_ms.saturating_sub(*armed_at) <= KILL_ARM_WINDOW_MS
        {
            self.armed_kill = None;
            return KillStage::Remove;
        }

        self.armed_kill = Some((backend, id, now_ms));
        if matches!(status, Status::Working | Status::NeedsInput) {
            KillStage::Stop
        } else {
            KillStage::Noop
        }
    }

    /// Whether the selected row is currently armed for removal (footer hint).
    pub fn is_armed(&self, now_ms: i64) -> bool {
        let Some(session) = self.selected() else {
            return false;
        };
        matches!(&self.armed_kill, Some((b, id, at))
            if *b == session.backend && id == &session.id
                && now_ms.saturating_sub(*at) <= KILL_ARM_WINDOW_MS)
    }

    /// key '/'
    pub fn set_filter(&mut self, s: String) {
        self.filter = s;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// j/k/arrows — cursor only, never rebuilds the row cache. Lands on Session rows,
    /// skipping headers/spacers in the direction of travel. A single-step move (±1) off the
    /// last/first session row WRAPS to the first/last one; larger deltas clamp as before.
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            self.sync_expanded();
            return;
        }
        let step: i32 = if delta >= 0 { 1 } else { -1 };
        let target = (self.selected as i32 + delta).clamp(0, len as i32 - 1);
        // Nearest Session row from `target` in the travel direction, else the other way.
        let chosen = self
            .session_from(target, step)
            .or_else(|| self.session_from(target, -step));

        if let Some(c) = chosen {
            // A single arrow press that can't advance means we are on the first/last session
            // row — wrap to the opposite end.
            if delta == step && c == self.selected {
                let wrapped = if step > 0 {
                    self.session_from(0, 1)
                } else {
                    self.session_from(len as i32 - 1, -1)
                };
                self.selected = wrapped.unwrap_or(c);
            } else {
                self.selected = c;
            }
        }
        self.sync_expanded();
    }

    /// The first Session-row index at or beyond `start` walking in `step` direction, if any.
    fn session_from(&self, start: i32, step: i32) -> Option<usize> {
        let len = self.rows.len() as i32;
        let mut idx = start;
        while (0..len).contains(&idx) {
            if matches!(self.rows[idx as usize], Row::Session { .. }) {
                return Some(idx as usize);
            }
            idx += step;
        }
        None
    }

    pub fn selected(&self) -> Option<&Session> {
        match self.rows.get(self.selected)? {
            Row::Session { backend, id, .. } => self.find_session(*backend, id),
            _ => None,
        }
    }

    /// Index of the currently selected row (aligns 1:1 with `visible()`).
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// (backend, id) of the currently selected session row, if any.
    fn selected_key(&self) -> Option<(BackendKind, String)> {
        match self.rows.get(self.selected)? {
            Row::Session { backend, id, .. } => Some((*backend, id.clone())),
            _ => None,
        }
    }

    /// Force the selected row's inline peek open (so the ask above a reply input is visible).
    /// No-op-safe: leaves the expansion unset when nothing selectable is under the cursor.
    pub fn expand_selected(&mut self) {
        self.expanded = self.selected_key();
    }

    /// Space: toggle the inline peek expansion of the selected row (one at a time).
    pub fn toggle_expanded(&mut self) {
        let key = self.selected_key();
        self.expanded = match (key, self.expanded.take()) {
            (Some(k), Some(e)) if k == e => None, // already expanded -> collapse
            (Some(k), _) => Some(k),
            (None, _) => None,
        };
    }

    /// The currently inline-expanded row, if any.
    pub fn expanded(&self) -> Option<&(BackendKind, String)> {
        self.expanded.as_ref()
    }

    /// The session behind a (backend, id) key (for resolving expansion content by the
    /// EXPANDED key rather than the selection, which may have since diverged, and for the
    /// rename flow which must target the row by id even after a reorder).
    pub fn session_for(&self, key: &(BackendKind, String)) -> Option<&Session> {
        self.find_session(key.0, &key.1)
    }

    /// Linear lookup of a session by (backend, id).
    fn find_session(&self, backend: BackendKind, id: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.backend == backend && s.id == id)
    }

    /// Pin the selection onto the row for `key` if it is currently visible (used to keep the
    /// inline rename row from visually jumping away when the background refresh reorders).
    /// Returns true when the row was found and selected.
    pub fn select_by_key(&mut self, key: &(BackendKind, String)) -> bool {
        if let Some(idx) = self.rows.iter().position(
            |r| matches!(r, Row::Session { backend, id, .. } if *backend == key.0 && *id == key.1),
        ) {
            self.selected = idx;
            self.sync_expanded();
            true
        } else {
            false
        }
    }

    /// Collapse the expansion whenever the selected row is no longer the expanded row (a
    /// regroup / show-all / filter / cursor move landed elsewhere). Called after every
    /// selection settle so a stale expansion can never render another session's transcript.
    fn sync_expanded(&mut self) {
        if let Some(e) = &self.expanded
            && self.selected_key().as_ref() != Some(e)
        {
            self.expanded = None;
        }
    }

    /// Project root of the group the selection sits in (header or session row).
    pub fn selected_group_root(&self) -> Option<PathBuf> {
        match self.rows.get(self.selected)? {
            Row::ProjectHeader { root, .. } => Some(root.clone()),
            Row::Session { backend, id, .. } => {
                let session = self.find_session(*backend, id)?;
                Some(self.cached_root(&session.cwd))
            }
            _ => None,
        }
    }

    /// Where an inline-composer spawn lands, given the current grouping:
    ///   - ByProject: the selected session's project group root (the header's root).
    ///   - ByState: the selected session's own cwd.
    ///
    /// None when the list is empty (nothing selected).
    pub fn spawn_target(&self) -> Option<PathBuf> {
        let session = self.selected()?;
        match self.group_mode {
            GroupMode::ByProject => Some(self.cached_root(&session.cwd)),
            GroupMode::ByState => Some(session.cwd.clone()),
        }
    }

    /// `project_root(cwd)` via the memo when possible. Read-only (`&self`), so a miss is
    /// recomputed but not inserted — the ByProject rebuild is what populates the cache.
    fn cached_root(&self, cwd: &Path) -> PathBuf {
        self.root_cache
            .get(cwd)
            .cloned()
            .unwrap_or_else(|| project_root(cwd))
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Substring filter over title + cwd, case-insensitive (matches rebuild_rows).
    fn passes_filter(&self, s: &Session) -> bool {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            return true;
        }
        s.title.to_lowercase().contains(&needle)
            || s.cwd.to_string_lossy().to_lowercase().contains(&needle)
    }

    /// Memoized project_root(cwd).
    fn root_of(cache: &mut HashMap<PathBuf, PathBuf>, cwd: &Path) -> PathBuf {
        if let Some(root) = cache.get(cwd) {
            return root.clone();
        }
        let root = project_root(cwd);
        cache.insert(cwd.to_path_buf(), root.clone());
        root
    }

    /// The Session row for a session index (real summary + updated_at_ms).
    fn session_row(s: &Session) -> Row {
        Row::Session {
            backend: s.backend,
            id: s.id.clone(),
            title: s.title.clone(),
            summary: s.summary.clone(),
            status: s.status,
            hidden: s.hidden,
            updated_at_ms: s.updated_at_ms,
            pr_refs: s.pr_refs.clone(),
        }
    }

    /// Recompute the cached row model: default-view exclusion of companion/archived
    /// rows, the substring filter, then either ByState sections (fixed order, empty
    /// sections omitted, all rows uncapped) or ByProject headers.
    fn rebuild_rows(&mut self) {
        // While a filter is active, search covers hidden/companion rows too (otherwise a
        // search could never surface the archived sessions the user is looking for). The
        // hidden/companion exclusion therefore applies only when the filter is empty.
        let filtering = !self.filter.is_empty();
        let include_all = self.show_all || filtering;

        // Visible session indices (exclusion + filter), recency DESC.
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| include_all || !(s.hidden || s.companion))
            .filter(|(_, s)| self.passes_filter(s))
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|&i| std::cmp::Reverse(self.sessions[i].updated_at_ms));

        // Cache the suppressed-row count alongside the rows (same inputs, same lifetime).
        // While filtering, every match is shown, so nothing that matches is hidden.
        self.hidden_rows = if include_all {
            0
        } else {
            self.sessions
                .iter()
                .filter(|s| (s.hidden || s.companion) && self.passes_filter(s))
                .count()
        };

        self.rows = match self.group_mode {
            GroupMode::ByState => self.build_state_rows(&indices),
            GroupMode::ByProject => self.build_project_rows(&indices),
        };
    }

    /// ByState: fixed section order, empty sections omitted. Every member row renders —
    /// the list widget's ListState selection auto-scrolls the full (uncapped) list.
    fn build_state_rows(&self, indices: &[usize]) -> Vec<Row> {
        let order = [
            Section::NeedsInput,
            Section::Working,
            Section::Idle,
            Section::Done,
        ];
        let mut rows = Vec::new();
        for section in order {
            let members: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&i| section_of(self.sessions[i].status) == section)
                .collect();
            if members.is_empty() {
                continue;
            }
            // A blank spacer between sections (not before the first).
            if !rows.is_empty() {
                rows.push(Row::Spacer);
            }
            rows.push(Row::SectionHeader {
                section,
                count: members.len(),
            });
            for &i in &members {
                rows.push(Self::session_row(&self.sessions[i]));
            }
        }
        rows
    }

    /// ByProject: group by memoized project_root ACROSS backends, groups ordered by
    /// newest session DESC, sessions within a group already recency-sorted.
    fn build_project_rows(&mut self, indices: &[usize]) -> Vec<Row> {
        // Preserve incoming recency order inside each group by iterating `indices`.
        let mut order: Vec<PathBuf> = Vec::new();
        let mut by_root: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for &i in indices {
            let cwd = self.sessions[i].cwd.clone();
            let root = Self::root_of(&mut self.root_cache, &cwd);
            if !by_root.contains_key(&root) {
                order.push(root.clone());
            }
            by_root.entry(root).or_default().push(i);
        }
        // `indices` is recency DESC, so the first time we see a root is its newest
        // session — `order` is therefore already group-order (newest group first).
        let mut rows = Vec::new();
        for root in order {
            let members = &by_root[&root];
            // A blank spacer between project groups (not before the first).
            if !rows.is_empty() {
                rows.push(Row::Spacer);
            }
            rows.push(Row::ProjectHeader {
                root: root.clone(),
                count: members.len(),
            });
            for &i in members {
                rows.push(Self::session_row(&self.sessions[i]));
            }
        }
        rows
    }

    /// Clamp selection into bounds and snap it onto a Session row when possible, then sync
    /// the inline expansion (collapse it if the selection has moved off the expanded row).
    fn clamp_selection(&mut self) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            self.sync_expanded();
            return;
        }
        if self.selected >= len {
            self.selected = len - 1;
        }
        if !matches!(self.rows.get(self.selected), Some(Row::Session { .. })) {
            // Snap onto the nearest Session row: search forward from here, then backward.
            let here = self.selected as i32;
            if let Some(found) = self
                .session_from(here, 1)
                .or_else(|| self.session_from(here - 1, -1))
            {
                self.selected = found;
            }
        }
        self.sync_expanded();
    }
}

/// The ByState section a status folds into (Failed + Stopped -> Done).
fn section_of(status: Status) -> Section {
    match status {
        Status::NeedsInput => Section::NeedsInput,
        Status::Working => Section::Working,
        Status::Idle => Section::Idle,
        Status::Done | Status::Failed | Status::Stopped => Section::Done,
    }
}

/// Truncate `s` to at most `width` chars (char-, not byte-bounded).
fn truncate_to(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

/// Width math for one session row, kept pure so it is unit-testable.
///
/// The row is `<glyph><space><mark><space><name>[<2sp><detail>]<pad><right cluster>`,
/// flush-left (glyph in column 0). `mark_width` is the measured display width of the brand
/// mark (some marks are ambiguous-width — the caller measures, never assumes 1). `right_len`
/// is the measured width of the whole right-aligned cluster (`<pr> <status word> <time>`),
/// reserved FIRST plus a one-space minimum gap, so a long title truncates instead of
/// clipping the cluster off the line. `detail` is the left muted summary; any room left
/// after the name (and a two-space gap) goes to a truncated `detail`. Returns the visible
/// name, the visible detail, and the pad width.
pub fn row_layout(
    width: usize,
    mark_width: usize,
    name: &str,
    detail: &str,
    right_len: usize,
) -> (String, String, usize) {
    // Fixed left decorations before the name: glyph + space + mark + space (flush, no indent).
    let left_fixed = 3 + mark_width;
    // Reserve the right cluster plus at least one space of separation.
    let content = width.saturating_sub(left_fixed + right_len + 1);

    let name_out = truncate_to(name, content);
    let name_len = name_out.chars().count();

    let mut detail_out = String::new();
    if !detail.is_empty() && content > name_len + 2 {
        detail_out = truncate_to(detail, content - name_len - 2);
    }
    let detail_len = detail_out.chars().count();

    let used = left_fixed + name_len + if detail_len > 0 { 2 + detail_len } else { 0 };
    let pad = width.saturating_sub(used + right_len).max(1);
    (name_out, detail_out, pad)
}

/// Whether a reply may be delivered to a session at all: the backend must support reply AND
/// the session must actually be waiting for input. The sole safety gate against sending to a
/// non-blocked session. Pure.
pub fn reply_allowed(caps_reply: bool, status: Status) -> bool {
    caps_reply && matches!(status, Status::NeedsInput)
}

/// How a typed codex approval reply maps to a decision. Approve/Deny inject the single
/// approval keystroke; Freeform (anything else, including empty) attaches with focus so the
/// user finishes manually rather than guessing. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexReply {
    Approve,
    Deny,
    Freeform,
}

pub fn codex_reply_keystroke(input: &str) -> CodexReply {
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" | "approve" | "a" | "ok" => CodexReply::Approve,
        "n" | "no" | "deny" | "d" | "reject" => CodexReply::Deny,
        _ => CodexReply::Freeform,
    }
}

/// Inline spawn composer (item 8): a persistent one-line input above the footer. Holds
/// the task text plus the target backend, which Tab cycles Claude -> Codex -> Opencode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    backend: BackendKind,
    /// Index into `models_for(self.backend)` — the per-spawn model choice (Shift+Tab cycles;
    /// reset to 0 whenever the backend changes).
    model_idx: usize,
    /// Slash-command names available for the current backend/target (scanned by the caller
    /// via `set_commands`), with the (backend, target) they were scanned for so the caller
    /// only re-scans on a change.
    commands: Vec<String>,
    commands_key: Option<(BackendKind, Option<PathBuf>)>,
    /// Highlighted suggestion index, and whether the popup was Esc-dismissed for this word.
    suggest_idx: usize,
    suggest_dismissed: bool,
}

/// The per-backend model cycle. Single-entry lists make Shift+Tab a no-op there. The leading
/// entry is the default; codex/opencode expose only "default" (no model flag on spawn).
pub fn models_for(backend: BackendKind) -> &'static [&'static str] {
    match backend {
        BackendKind::Claude => &["opus[1m]", "sonnet", "fable"],
        // gpt-5.1-codex-max is deliberately omitted: it 400s on ChatGPT-account auth.
        BackendKind::Codex => &["default", "gpt-5.3-codex", "gpt-5.2-codex"],
        BackendKind::Opencode => &["default"],
    }
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
    /// Fresh composer: empty text, Claude backend, its first model.
    pub fn new() -> Composer {
        Composer {
            text: String::new(),
            backend: BackendKind::Claude,
            model_idx: 0,
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
    pub fn model(&self) -> &'static str {
        models_for(self.backend)[self.model_idx]
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

    /// Tab: Claude -> Codex -> Opencode -> Claude. Resets the model to the new backend's first.
    pub fn cycle_backend(&mut self) {
        self.backend = match self.backend {
            BackendKind::Claude => BackendKind::Codex,
            BackendKind::Codex => BackendKind::Opencode,
            BackendKind::Opencode => BackendKind::Claude,
        };
        self.model_idx = 0;
    }

    /// Shift+Tab: cycle the model for the current backend (a no-op for single-entry cycles).
    pub fn cycle_model(&mut self) {
        self.model_idx = (self.model_idx + 1) % models_for(self.backend).len();
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
        if self.suggest_dismissed {
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

    /// The clamped highlight index into the current suggestions.
    pub fn suggestion_highlight(&self) -> usize {
        self.suggest_idx
            .min(self.suggestions().len().saturating_sub(1))
    }

    /// Up/Down within the popup: wrap the highlight over the suggestions.
    pub fn move_suggestion(&mut self, delta: i32) {
        let n = self.suggestions().len();
        if n == 0 {
            return;
        }
        let cur = self.suggestion_highlight() as i32;
        self.suggest_idx = (cur + delta).rem_euclid(n as i32) as usize;
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

/// "42s" / "17m" / "3h" / "6d" (largest whole unit, no decimals; negative -> "0s").
pub fn format_elapsed(delta_ms: i64) -> String {
    if delta_ms <= 0 {
        return "0s".to_string();
    }
    let secs = delta_ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

#[cfg(test)]
mod tests {
    use super::{CodexReply, codex_reply_keystroke, reply_allowed};
    use agent_viewer_core::Status;

    #[test]
    fn reply_allowed_only_for_capable_and_blocked() {
        // Both conditions must hold: backend supports reply AND the session is blocked.
        assert!(reply_allowed(true, Status::NeedsInput));
        // Capable backend but the session is not blocked -> never.
        for s in [
            Status::Working,
            Status::Idle,
            Status::Done,
            Status::Failed,
            Status::Stopped,
        ] {
            assert!(!reply_allowed(true, s));
        }
        // Blocked session but the backend cannot reply -> never.
        assert!(!reply_allowed(false, Status::NeedsInput));
    }

    #[test]
    fn codex_reply_keystroke_maps_yes_no_and_freeform() {
        for a in ["y", "yes", "approve", "a", "ok", " YES "] {
            assert_eq!(codex_reply_keystroke(a), CodexReply::Approve);
        }
        for d in ["n", "no", "deny", "d", "reject"] {
            assert_eq!(codex_reply_keystroke(d), CodexReply::Deny);
        }
        for f in ["", "maybe", "do it later"] {
            assert_eq!(codex_reply_keystroke(f), CodexReply::Freeform);
        }
    }
}

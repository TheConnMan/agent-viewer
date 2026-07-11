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
    SectionHeader {
        section: Section,
        count: usize,
    },
    /// ByProject mode.
    ProjectHeader {
        root: PathBuf,
        count: usize,
    },
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
        if let Some((backend, id)) = anchor
            && let Some(idx) = self.rows.iter().position(|r| {
                matches!(r, Row::Session { backend: b, id: rid, .. } if *b == backend && rid == &id)
            })
        {
            self.selected = idx;
            self.sync_expanded();
            return;
        }
        self.clamp_selection();
    }

    /// The render model (a clone of the cached rows).
    pub fn visible(&self) -> Vec<Row> {
        self.rows.clone()
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
    pub fn hidden_count(&self) -> usize {
        if self.show_all {
            return 0;
        }
        self.sessions
            .iter()
            .filter(|s| (s.hidden || s.companion) && self.passes_filter(s))
            .count()
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
        let Some((backend, id, status)) = self
            .selected()
            .map(|s| (s.backend, s.id.clone(), s.status))
        else {
            return KillStage::Noop;
        };

        if let Some((armed_backend, armed_id, armed_at)) = &self.armed_kill
            && *armed_backend == backend
            && armed_id == &id
            && now_ms.saturating_sub(*armed_at) <= 2_000
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
            if *b == session.backend && id == &session.id && now_ms.saturating_sub(*at) <= 2_000)
    }

    /// key '/'
    pub fn set_filter(&mut self, s: String) {
        self.filter = s;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// j/k/arrows — cursor only, never rebuilds the row cache. Lands on Session rows,
    /// skipping headers/markers in the direction of travel.
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            self.sync_expanded();
            return;
        }
        let target = (self.selected as i32 + delta).clamp(0, len as i32 - 1);
        let step: i32 = if delta >= 0 { 1 } else { -1 };
        // Walk from the clamped target toward the travel direction for a Session row;
        // if none that way (target sat past the last session), fall back the other way.
        let mut chosen = None;
        let mut idx = target;
        while (0..len as i32).contains(&idx) {
            if matches!(self.rows[idx as usize], Row::Session { .. }) {
                chosen = Some(idx as usize);
                break;
            }
            idx += step;
        }
        if chosen.is_none() {
            let mut idx = target;
            while (0..len as i32).contains(&idx) {
                if matches!(self.rows[idx as usize], Row::Session { .. }) {
                    chosen = Some(idx as usize);
                    break;
                }
                idx -= step;
            }
        }
        if let Some(c) = chosen {
            self.selected = c;
        }
        self.sync_expanded();
    }

    pub fn selected(&self) -> Option<&Session> {
        match self.rows.get(self.selected)? {
            Row::Session { backend, id, .. } => self
                .sessions
                .iter()
                .find(|s| s.backend == *backend && &s.id == id),
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
    /// EXPANDED key rather than the selection, which may have since diverged).
    pub fn session_for(&self, key: &(BackendKind, String)) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.backend == key.0 && s.id == key.1)
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
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.backend == *backend && &s.id == id)?;
                Some(
                    self.root_cache
                        .get(&session.cwd)
                        .cloned()
                        .unwrap_or_else(|| project_root(&session.cwd)),
                )
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
            GroupMode::ByProject => Some(
                self.root_cache
                    .get(&session.cwd)
                    .cloned()
                    .unwrap_or_else(|| project_root(&session.cwd)),
            ),
            GroupMode::ByState => Some(session.cwd.clone()),
        }
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
        // Visible session indices (exclusion + filter), recency DESC.
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.show_all || !(s.hidden || s.companion))
            .filter(|(_, s)| self.passes_filter(s))
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|&i| std::cmp::Reverse(self.sessions[i].updated_at_ms));

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
            let mut found = None;
            for i in self.selected..len {
                if matches!(self.rows[i], Row::Session { .. }) {
                    found = Some(i);
                    break;
                }
            }
            if found.is_none() {
                for i in (0..self.selected).rev() {
                    if matches!(self.rows[i], Row::Session { .. }) {
                        found = Some(i);
                        break;
                    }
                }
            }
            if let Some(f) = found {
                self.selected = f;
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
/// The row is `<glyph><space><mark><space><name>[<2sp><detail>]<pad>[<pr><space>]<elapsed>`,
/// flush-left (glyph in column 0). `mark_width` is the measured display width of the brand
/// mark (some marks are ambiguous-width — the caller measures, never assumes 1). The elapsed
/// slot and, when present, a PR badge are reserved on the right FIRST (plus a one-space
/// minimum gap), so a long title truncates instead of clipping them off the line. `detail`
/// is the status-word-plus-summary field; any room left after the name (and a two-space gap)
/// goes to a truncated `detail`. Returns the visible name, the visible detail, and the pad.
pub fn row_layout(
    width: usize,
    mark_width: usize,
    name: &str,
    detail: &str,
    pr_len: usize,
    elapsed_len: usize,
) -> (String, String, usize) {
    // Fixed left decorations before the name: glyph + space + mark + space (flush, no indent).
    let left_fixed = 3 + mark_width;
    // Right reservation folded into content: PR badge (+1 gap) when present, then elapsed.
    let pr_reserve = if pr_len > 0 { pr_len + 1 } else { 0 };
    // Reserve the elapsed slot plus at least one space of separation, and the PR badge.
    let content = width.saturating_sub(left_fixed + pr_reserve + elapsed_len + 1);

    let name_out = truncate_to(name, content);
    let name_len = name_out.chars().count();

    let mut detail_out = String::new();
    if !detail.is_empty() && content > name_len + 2 {
        detail_out = truncate_to(detail, content - name_len - 2);
    }
    let detail_len = detail_out.chars().count();

    let used = left_fixed + name_len + if detail_len > 0 { 2 + detail_len } else { 0 };
    let pad = width.saturating_sub(used + pr_reserve + elapsed_len).max(1);
    (name_out, detail_out, pad)
}

/// Inline spawn composer (item 8): a persistent one-line input above the footer. Holds
/// the task text plus the target backend, which Tab cycles Claude -> Codex -> Opencode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    backend: BackendKind,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    /// Fresh composer: empty text, Claude backend.
    pub fn new() -> Composer {
        Composer {
            text: String::new(),
            backend: BackendKind::Claude,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn push_char(&mut self, c: char) {
        self.text.push(c);
    }

    /// Backspace on empty is a no-op (not a panic).
    pub fn backspace(&mut self) {
        self.text.pop();
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Tab: Claude -> Codex -> Opencode -> Claude.
    pub fn cycle_backend(&mut self) {
        self.backend = match self.backend {
            BackendKind::Claude => BackendKind::Codex,
            BackendKind::Codex => BackendKind::Opencode,
            BackendKind::Opencode => BackendKind::Claude,
        };
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

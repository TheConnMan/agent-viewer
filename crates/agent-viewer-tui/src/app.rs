use crate::shared_listing::{SpawnDirectoryMode, SpawnTarget, TargetRequest};
use agent_viewer_core::group::project_root;
use agent_viewer_core::{BackendKind, PrRef, Session, Status};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// The composer, DetachTracker, and command-scan helpers moved to `crate::composer`; re-export
// them here so `agent_viewer_tui::app::{Composer, DetachTracker, subdir_names, file_stems}`
// still resolves for every existing import site.
pub use crate::composer::{Composer, DetachTracker, file_stems, subdir_names};

/// Grouping mode for the flat list. `ByProject` is the startup default (Ctrl+S toggles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    ByState,
    ByProject,
}

/// State sections in the ByState view. Failed + Stopped fold into Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    NeedsInput,
    Working,
    Idle,
    Done,
}

/// Stable identity of a collapsible group, keyed by the active grouping. Persisted to the
/// viewer DB via its text form so a collapse survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GroupKey {
    Project(PathBuf),
    State(Section),
}

impl GroupKey {
    /// Stable text form for DB persistence. Project -> "project:<path>" (to_string_lossy),
    /// State -> "state:needs_input" | "state:working" | "state:idle" | "state:done".
    pub fn to_storage(&self) -> String {
        match self {
            GroupKey::Project(p) => format!("project:{}", p.to_string_lossy()),
            GroupKey::State(section) => format!("state:{}", section_storage(*section)),
        }
    }

    /// Inverse of `to_storage`. Unknown/malformed text yields None (never a panic).
    pub fn from_storage(s: &str) -> Option<GroupKey> {
        if let Some(path) = s.strip_prefix("project:") {
            return Some(GroupKey::Project(PathBuf::from(path)));
        }
        if let Some(name) = s.strip_prefix("state:") {
            return section_from_storage(name).map(GroupKey::State);
        }
        None
    }
}

/// The persisted token for a Section (paired with `section_from_storage`).
fn section_storage(section: Section) -> &'static str {
    match section {
        Section::NeedsInput => "needs_input",
        Section::Working => "working",
        Section::Idle => "idle",
        Section::Done => "done",
    }
}

/// The Section for a persisted token (None for unknown text).
fn section_from_storage(name: &str) -> Option<Section> {
    match name {
        "needs_input" => Some(Section::NeedsInput),
        "working" => Some(Section::Working),
        "idle" => Some(Section::Idle),
        "done" => Some(Section::Done),
        _ => None,
    }
}

fn is_hold_title(title: &str) -> bool {
    title.eq_ignore_ascii_case("hold")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// ByState mode. `collapsed` hides this section's session rows; `count` stays the group's
    /// total member count (when collapsed, that IS the hidden-row count).
    SectionHeader {
        section: Section,
        count: usize,
        collapsed: bool,
    },
    /// ByProject mode. `collapsed` hides this project's visible session rows; `count` stays
    /// the visible session row count after hold titles are excluded.
    ProjectHeader {
        root: PathBuf,
        count: usize,
        collapsed: bool,
    },
    Session {
        backend: BackendKind,
        id: String,
        title: String,
        summary: String,
        project: Option<String>,
        activity: Option<String>,
        status: Status,
        hidden: bool,
        created_at_ms: i64,
        updated_at_ms: i64,
        pr_refs: Vec<PrRef>,
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

fn sanitize_session_titles(sessions: &mut [Session]) {
    for session in sessions {
        session.title.retain(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{00AD}'
                        | '\u{0600}'..='\u{0605}'
                        | '\u{061C}'
                        | '\u{06DD}'
                        | '\u{070F}'
                        | '\u{0890}'..='\u{0891}'
                        | '\u{08E2}'
                        | '\u{180E}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{2028}'..='\u{202E}'
                        | '\u{2060}'..='\u{2064}'
                        | '\u{2066}'..='\u{206F}'
                        | '\u{FEFF}'
                        | '\u{FFF9}'..='\u{FFFB}'
                        | '\u{110BD}'
                        | '\u{110CD}'
                        | '\u{13430}'..='\u{1343F}'
                        | '\u{1BCA0}'..='\u{1BCA3}'
                        | '\u{1D173}'..='\u{1D17A}'
                        | '\u{E0001}'
                        | '\u{E0020}'..='\u{E007F}'
                )
        });
    }
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
    /// Latest background computed activity ribbon for each session.
    activity_ribbons: HashMap<(BackendKind, String), String>,
    /// Memoized project_root(cwd). cwds are stable, so this survives refresh ticks
    /// and stops re-walking the filesystem for sessions we have already grouped.
    root_cache: HashMap<PathBuf, PathBuf>,
    /// The armed (backend, id, armed_at_ms) for the two-stage Ctrl+X.
    armed_kill: Option<(BackendKind, String, i64)>,
    /// Cached `hidden_count()` value, recomputed with the rows (it depends on exactly the
    /// same inputs), so the per-frame footer never re-filters the session list.
    hidden_rows: usize,
    /// Groups the user has collapsed (keyed by the active grouping). A collapsed group still
    /// renders its header but omits its session rows. Seeded once from the DB at startup and
    /// persisted by the caller on every toggle.
    collapsed: HashSet<GroupKey>,
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        let mut sessions = sessions;
        sanitize_session_titles(&mut sessions);
        let mut app = App {
            sessions,
            selected: 0,
            filter: String::new(),
            show_all: false,
            group_mode: GroupMode::ByProject,
            rows: Vec::new(),
            activity_ribbons: HashMap::new(),
            root_cache: HashMap::new(),
            armed_kill: None,
            hidden_rows: 0,
            collapsed: HashSet::new(),
        };
        app.rebuild_rows();
        app.clamp_selection();
        app
    }

    /// Replace the session set, keeping the current selection anchored across the refresh to
    /// EITHER the same (backend,id) session row (existing behavior) OR, when a header is
    /// selected, the same group header key (so a header selection is not lost on the 1s
    /// background refresh); only when neither anchor survives does it clamp.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let session_anchor = self.selected().map(|s| (s.backend, s.id.clone()));
        let header_anchor = self.selected_header_key();
        let mut sessions = sessions;
        sanitize_session_titles(&mut sessions);
        self.sessions = sessions;
        self.activity_ribbons.retain(|(backend, id), _| {
            self.sessions
                .iter()
                .any(|session| session.backend == *backend && session.id == *id)
        });
        self.rebuild_rows();
        if let Some(anchor) = session_anchor
            && self.select_by_key(&anchor)
        {
            return;
        }
        if let Some(key) = header_anchor
            && self.select_header(&key)
        {
            return;
        }
        self.clamp_selection();
    }

    /// The render model (borrows the cached rows; rendered fresh every frame).
    pub fn visible(&self) -> &[Row] {
        &self.rows
    }

    /// Every known session, before the filter/show-all/grouping that shapes `visible()`.
    /// The triage queue is built from this so its length matches `needs_input_count`.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn set_activity_ribbon(&mut self, backend: BackendKind, id: &str, ribbon: Option<String>) {
        let key = (backend, id.to_string());
        match ribbon {
            Some(ribbon) => {
                self.activity_ribbons.insert(key, ribbon.clone());
                for row in &mut self.rows {
                    if let Row::Session {
                        backend: row_backend,
                        id: row_id,
                        activity,
                        ..
                    } = row
                        && *row_backend == backend
                        && row_id == id
                    {
                        *activity = Some(ribbon.clone());
                    }
                }
            }
            None => {
                self.activity_ribbons.remove(&key);
                for row in &mut self.rows {
                    if let Row::Session {
                        backend: row_backend,
                        id: row_id,
                        activity,
                        ..
                    } = row
                        && *row_backend == backend
                        && row_id == id
                    {
                        *activity = None;
                    }
                }
            }
        }
    }

    pub fn session_ids_for_backend(&self, backend: BackendKind) -> HashSet<String> {
        self.sessions
            .iter()
            .filter(|session| session.backend == backend)
            .map(|session| session.id.clone())
            .collect()
    }

    /// Ctrl+A (I-2): one show-all toggle covering companions + archived rows.
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

    /// Sessions actively working right now (header status summary).
    pub fn running_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| matches!(s.status, Status::Working))
            .count()
    }

    /// Sessions waiting on the user for input right now (header status summary).
    pub fn needs_input_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| matches!(s.status, Status::NeedsInput { .. }))
            .count()
    }

    /// Sessions that have finished, whether successfully or with an error.
    pub fn completed_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| match &session.status {
                Status::Done | Status::Error => true,
                Status::Working | Status::NeedsInput { .. } | Status::Idle | Status::Unknown => {
                    false
                }
            })
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

    /// Seed the collapsed set (called once at startup from the DB). Rebuilds rows + clamps.
    pub fn set_collapsed(&mut self, collapsed: HashSet<GroupKey>) {
        self.collapsed = collapsed;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// Read the collapsed set (persistence + tests).
    pub fn collapsed(&self) -> &HashSet<GroupKey> {
        &self.collapsed
    }

    /// Whether a group is currently collapsed.
    pub fn is_group_collapsed(&self, key: &GroupKey) -> bool {
        self.collapsed.contains(key)
    }

    /// If the selected row is a header, flip its collapsed state, rebuild rows, keep the
    /// cursor on that same header, and return (key, now_collapsed) for the caller to persist.
    /// None when the selected row is not a header (a session row or empty list).
    pub fn toggle_selected_group(&mut self) -> Option<(GroupKey, bool)> {
        let key = self.selected_header_key()?;
        let now_collapsed = if self.collapsed.remove(&key) {
            false
        } else {
            self.collapsed.insert(key.clone());
            true
        };
        self.rebuild_rows();
        self.select_header(&key);
        Some((key, now_collapsed))
    }

    /// The GroupKey of the currently selected header row, if the selection is on one.
    fn selected_header_key(&self) -> Option<GroupKey> {
        match self.rows.get(self.selected)? {
            Row::ProjectHeader { root, .. } => Some(GroupKey::Project(root.clone())),
            Row::SectionHeader { section, .. } => Some(GroupKey::State(*section)),
            _ => None,
        }
    }

    /// Pin the selection onto the header row for `key` if it is currently visible (keeps the
    /// cursor on a group header across a toggle/refresh). Returns true when found.
    fn select_header(&mut self, key: &GroupKey) -> bool {
        let found = self.rows.iter().position(|r| match (r, key) {
            (Row::ProjectHeader { root, .. }, GroupKey::Project(k)) => root == k,
            (Row::SectionHeader { section, .. }, GroupKey::State(k)) => section == k,
            _ => false,
        });
        if let Some(idx) = found {
            self.selected = idx;
            true
        } else {
            false
        }
    }

    /// Two-stage Ctrl+X (list page only). Selected session S, injected now_ms:
    ///  - armed for (S.backend,S.id) and now-armed <= 2_000 -> clear, Remove
    ///  - else arm (S, now): nonterminal S -> Stop,
    ///    else Noop (armed silently; footer shows the countdown hint)
    pub fn kill_stage(&mut self, now_ms: i64) -> KillStage {
        let Some((backend, id, should_stop)) = self
            .selected()
            .map(|s| (s.backend, s.id.clone(), !s.status.is_finished()))
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
        if should_stop {
            KillStage::Stop
        } else {
            KillStage::Noop
        }
    }

    pub fn disarm_kill_for(&mut self, backend: BackendKind, id: &str) {
        if self
            .armed_kill
            .as_ref()
            .is_some_and(|(armed_backend, armed_id, _)| *armed_backend == backend && armed_id == id)
        {
            self.armed_kill = None;
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

    /// Ctrl+F opens filtering.
    pub fn set_filter(&mut self, s: String) {
        self.filter = s;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// Arrow keys move the cursor only and never rebuild the row cache. Lands on selectable rows
    /// (headers AND session rows), skipping only Spacer rows in the direction of travel (a
    /// collapsed group's session rows are simply not emitted, so they need no extra skip). A
    /// single-step move (±1) off the last/first selectable row WRAPS to the first/last one;
    /// larger deltas clamp as before.
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let step: i32 = if delta >= 0 { 1 } else { -1 };
        let target = (self.selected as i32 + delta).clamp(0, len as i32 - 1);
        // Nearest selectable row from `target` in the travel direction, else the other way.
        let chosen = self
            .selectable_from(target, step)
            .or_else(|| self.selectable_from(target, -step));

        if let Some(c) = chosen {
            // A single arrow press that can't advance means we are on the first/last
            // selectable row — wrap to the opposite end.
            if delta == step && c == self.selected {
                let wrapped = if step > 0 {
                    self.selectable_from(0, 1)
                } else {
                    self.selectable_from(len as i32 - 1, -1)
                };
                self.selected = wrapped.unwrap_or(c);
            } else {
                self.selected = c;
            }
        }
    }

    /// Mouse hit-test target: select the visible row at `idx` (an index into `visible()`),
    /// if it exists and is selectable (a header or session row, never a Spacer). Returns
    /// whether the selection changed to a valid row.
    pub fn select_visible_index(&mut self, idx: usize) -> bool {
        match self.rows.get(idx) {
            Some(r) if !matches!(r, Row::Spacer) => {
                self.selected = idx;
                true
            }
            _ => false,
        }
    }

    /// The first row index at or beyond `start` (walking `step`) whose row matches `pred`.
    fn scan_from(&self, start: i32, step: i32, pred: impl Fn(&Row) -> bool) -> Option<usize> {
        let len = self.rows.len() as i32;
        let mut idx = start;
        while (0..len).contains(&idx) {
            if pred(&self.rows[idx as usize]) {
                return Some(idx as usize);
            }
            idx += step;
        }
        None
    }

    /// The first selectable-row index at or beyond `start` walking in `step` direction, if
    /// any. Selectable rows are everything but Spacer (headers + session rows).
    fn selectable_from(&self, start: i32, step: i32) -> Option<usize> {
        self.scan_from(start, step, |r| !matches!(r, Row::Spacer))
    }

    /// The first Session-row index at or beyond `start` walking in `step` direction, if any.
    fn session_from(&self, start: i32, step: i32) -> Option<usize> {
        self.scan_from(start, step, |r| matches!(r, Row::Session { .. }))
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

    /// The session behind a (backend, id) key. Used by flows that must retain their target
    /// even after a refresh reorders the list.
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
            true
        } else {
            false
        }
    }

    /// Where an inline-composer spawn lands, given the current grouping:
    ///   - ByProject: the selected session's project group root (the header's root).
    ///   - ByState: the selected session's own cwd.
    ///
    /// None when the list is empty (nothing selected).
    ///
    /// On a selected header: a ProjectHeader spawns into its own root (a sensible dir); a
    /// SectionHeader has no spawn dir (None -> the caller shows the "no target" footer notice).
    pub fn spawn_target(&self) -> Option<SpawnTarget> {
        match self.rows.get(self.selected) {
            Some(Row::ProjectHeader { root, .. }) => {
                Some(SpawnTarget::ExplicitDirectory(root.clone()))
            }
            Some(Row::Session { backend, id, .. }) => {
                let session = self.find_session(*backend, id)?;
                let (mode, displayed_directory) = match self.group_mode {
                    GroupMode::ByProject => (
                        SpawnDirectoryMode::ProjectRoot,
                        self.cached_root(&session.cwd),
                    ),
                    GroupMode::ByState => {
                        (SpawnDirectoryMode::WorkingDirectory, session.cwd.clone())
                    }
                };
                Some(SpawnTarget::Session {
                    request: TargetRequest::new(*backend, id.clone()),
                    mode,
                    displayed_directory,
                })
            }
            Some(Row::SectionHeader { .. }) | Some(Row::Spacer) | None => None,
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

    /// The Session row for a session index (real summary plus creation and update times).
    fn session_row(s: &Session, project: Option<String>) -> Row {
        Row::Session {
            backend: s.backend,
            id: s.id.clone(),
            title: s.title.clone(),
            summary: s.summary.clone(),
            project,
            activity: None,
            status: s.status.clone(),
            hidden: s.hidden,
            created_at_ms: s.created_at_ms,
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

        // Visible session indices (exclusion + filter), recency ASC.
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| include_all || !(s.hidden || s.companion))
            .filter(|(_, s)| self.passes_filter(s))
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|&i| self.sessions[i].updated_at_ms);

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
            GroupMode::ByState => {
                let visible_indices: Vec<usize> = indices
                    .iter()
                    .copied()
                    .filter(|&i| !is_hold_title(&self.sessions[i].title))
                    .collect();
                self.build_state_rows(&visible_indices)
            }
            GroupMode::ByProject => self.build_project_rows(&indices),
        };
        for row in &mut self.rows {
            if let Row::Session {
                backend,
                id,
                activity,
                ..
            } = row
            {
                *activity = self.activity_ribbons.get(&(*backend, id.clone())).cloned();
            }
        }
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
            let mut members: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&i| section_of(&self.sessions[i].status) == section)
                .collect();
            members.sort_by(|&a, &b| {
                let backend_key = |backend| match backend {
                    BackendKind::Codex => 0,
                    BackendKind::Claude => 1,
                    BackendKind::Opencode => 2,
                };
                self.sessions[a]
                    .created_at_ms
                    .cmp(&self.sessions[b].created_at_ms)
                    .then_with(|| {
                        backend_key(self.sessions[a].backend)
                            .cmp(&backend_key(self.sessions[b].backend))
                    })
                    .then_with(|| self.sessions[a].id.cmp(&self.sessions[b].id))
            });
            if members.is_empty() {
                continue;
            }
            // A blank spacer between sections (not before the first).
            if !rows.is_empty() {
                rows.push(Row::Spacer);
            }
            // A collapsed section still shows its header (count = hidden rows) but omits its
            // session rows.
            let collapsed = self.collapsed.contains(&GroupKey::State(section));
            rows.push(Row::SectionHeader {
                section,
                count: members.len(),
                collapsed,
            });
            if !collapsed {
                for &i in &members {
                    let root = self.cached_root(&self.sessions[i].cwd);
                    rows.push(Self::session_row(
                        &self.sessions[i],
                        Some(project_label(&root)),
                    ));
                }
            }
        }
        rows
    }

    /// ByProject: group by memoized project_root ACROSS backends. Groups are ordered by
    /// directory path (case-insensitive) so the list stays stable — starting or updating
    /// a session never reorders the groups. Sessions within a group are ordered by start
    /// time (created_at_ms) ASC, oldest first. Start time never changes, so existing rows
    /// don't shuffle when a session's activity updates.
    fn build_project_rows(&mut self, indices: &[usize]) -> Vec<Row> {
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
        // Stable group order: sort roots by path (case-insensitive), independent of any
        // session's recency, so a new session cannot move a group. Within each group,
        // order by start time ASC.
        order.sort_by(|a, b| {
            a.to_string_lossy()
                .to_lowercase()
                .cmp(&b.to_string_lossy().to_lowercase())
                .then_with(|| a.cmp(b))
        });
        for members in by_root.values_mut() {
            members.sort_by(|&a, &b| {
                self.sessions[a]
                    .created_at_ms
                    .cmp(&self.sessions[b].created_at_ms)
                    .then_with(|| {
                        self.sessions[b]
                            .updated_at_ms
                            .cmp(&self.sessions[a].updated_at_ms)
                    })
            });
        }
        // Roots are computed above (the only &mut self borrow, via root_of); the emit loop
        // below reads self.collapsed / self.sessions immutably.
        let mut rows = Vec::new();
        for root in order {
            let members = &by_root[&root];
            let visible_members: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&i| !is_hold_title(&self.sessions[i].title))
                .collect();
            // A blank spacer between project groups (not before the first).
            if !rows.is_empty() {
                rows.push(Row::Spacer);
            }
            // A collapsed project retains its header. Its count equals visible non hold session
            // rows, so a hold only header can show zero.
            let collapsed = self.collapsed.contains(&GroupKey::Project(root.clone()));
            rows.push(Row::ProjectHeader {
                root: root.clone(),
                count: visible_members.len(),
                collapsed,
            });
            if !collapsed {
                for i in visible_members {
                    rows.push(Self::session_row(&self.sessions[i], None));
                }
            }
        }
        rows
    }

    /// Clamp selection into bounds and snap it onto a Session row when possible.
    fn clamp_selection(&mut self) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        if self.selected >= len {
            self.selected = len - 1;
        }
        if !matches!(self.rows.get(self.selected), Some(Row::Session { .. })) {
            // Prefer a Session row (so startup + a vanished selection land on a session):
            // search forward from here, then backward. Only when no session row exists (e.g.
            // every group collapsed) fall back to the nearest selectable header.
            let here = self.selected as i32;
            if let Some(found) = self
                .session_from(here, 1)
                .or_else(|| self.session_from(here - 1, -1))
                .or_else(|| self.selectable_from(here, 1))
                .or_else(|| self.selectable_from(here - 1, -1))
            {
                self.selected = found;
            }
        }
    }
}

/// The state section a status folds into.
fn section_of(status: &Status) -> Section {
    match status {
        Status::NeedsInput { .. } => Section::NeedsInput,
        Status::Working => Section::Working,
        Status::Idle | Status::Unknown => Section::Idle,
        Status::Done | Status::Error => Section::Done,
    }
}

fn project_label(root: &Path) -> String {
    let home = agent_viewer_core::home_dir();
    if !home.as_os_str().is_empty() {
        let git_root = home.join("git");
        if let Ok(relative) = root.strip_prefix(git_root) {
            return relative.to_string_lossy().into_owned();
        }
    }
    root.to_string_lossy().into_owned()
}

/// Truncate `s` to at most `width` terminal columns. At `width == 0` this yields the empty
/// string, unlike `ui::truncate`, which treats zero as unconstrained.
fn truncate_to(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    out
}

/// Width math for one session row, kept pure so it is unit-testable.
///
/// The row is
/// `<glyph><mark><title><space><status><2sp><summary><summary_gap><pad><pr><space><elapsed>`.
/// The title keeps the supplied shared width whenever that column fits. Narrow rows may
/// reduce it enough to retain complete secondary fields. The PR badge is always atomic.
#[allow(clippy::too_many_arguments)]
pub fn row_layout(
    width: usize,
    mark_width: usize,
    title: &str,
    title_width: usize,
    status: &str,
    pr: &str,
    summary: &str,
    elapsed_width: usize,
) -> (String, String, String, String, usize) {
    // Fixed left decorations before the title: glyph + mark.
    let left_fixed = 1 + mark_width;
    let status_width = UnicodeWidthStr::width(status);
    let pr_width = UnicodeWidthStr::width(pr);
    let summary_min_width = summary
        .graphemes(true)
        .next()
        .map(UnicodeWidthStr::width)
        .unwrap_or(0);
    // One space separates title from status and one always separates elapsed when it fits.
    let required_without_secondary = left_fixed + 1 + status_width + 1 + elapsed_width;
    let unconstrained_title_capacity =
        title_width.min(width.saturating_sub(required_without_secondary));

    // Keep the full shared column whenever it fits. If the viewport itself forces that
    // column narrower, also reserve a complete PR and one summary grapheme where possible.
    let mut title_capacity = unconstrained_title_capacity;
    if !pr.is_empty() || !summary.is_empty() {
        let available_for_secondary = width.saturating_sub(required_without_secondary);
        let pr_minimum = usize::from(!pr.is_empty()) * (2 + pr_width);
        let summary_minimum = usize::from(!summary.is_empty()) * (2 + summary_min_width);
        let combined_minimum =
            2 + pr_width + usize::from(!pr.is_empty() && !summary.is_empty()) + summary_min_width;
        let minimum_secondary =
            if !pr.is_empty() && !summary.is_empty() && combined_minimum <= available_for_secondary
            {
                combined_minimum
            } else if !pr.is_empty() && pr_minimum <= available_for_secondary {
                pr_minimum
            } else if !summary.is_empty() && summary_minimum <= available_for_secondary {
                summary_minimum
            } else {
                0
            };
        title_capacity = title_capacity
            .min(width.saturating_sub(required_without_secondary + minimum_secondary));
    }

    let mut title_out = truncate_to(title, title_capacity);
    title_out.push_str(
        &" ".repeat(title_capacity.saturating_sub(UnicodeWidthStr::width(title_out.as_str()))),
    );

    let mut secondary_capacity = width.saturating_sub(required_without_secondary + title_capacity);
    let mut pr_out = String::new();
    let mut summary_out = String::new();
    if secondary_capacity >= 2 {
        secondary_capacity -= 2;
        if !pr.is_empty() && pr_width <= secondary_capacity {
            pr_out = pr.to_string();
            secondary_capacity -= pr_width;
        }
        if !summary.is_empty() {
            if !pr_out.is_empty() {
                secondary_capacity = secondary_capacity.saturating_sub(1);
            }
            summary_out = truncate_to(summary, secondary_capacity);
        }
    }

    let rendered_secondary_gap = usize::from(!pr_out.is_empty() || !summary_out.is_empty()) * 2;
    let pr_summary_gap = usize::from(!pr_out.is_empty() && !summary_out.is_empty());
    let used = left_fixed
        + title_capacity
        + 1
        + status_width
        + rendered_secondary_gap
        + UnicodeWidthStr::width(pr_out.as_str())
        + pr_summary_gap
        + UnicodeWidthStr::width(summary_out.as_str())
        + 1
        + elapsed_width;
    let pad = width.saturating_sub(used);
    (title_out, status.to_string(), pr_out, summary_out, pad)
}

/// How a typed codex approval reply maps to a decision. Approve auto-sends the stable `y`
/// approve key; Deny and Freeform (anything else, including empty) attach with focus so the
/// user finishes manually rather than guessing (the reject key is version/config specific,
/// and a free-text reply is not a yes/no decision). Pure.
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
    use super::{CodexReply, codex_reply_keystroke};

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

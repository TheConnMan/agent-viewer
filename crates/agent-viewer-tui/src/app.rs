use agent_viewer_core::group::project_root;
use agent_viewer_core::{BackendKind, Session, Status};
use std::collections::HashMap;
use std::path::PathBuf;

/// Grouping mode for the flat list. `ByState` is the startup default (Ctrl+S toggles).
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
    },
    /// DONE overflow "… N more" (I-5, cap 15).
    MoreMarker {
        hidden: usize,
    },
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
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        let mut app = App {
            sessions,
            selected: 0,
            filter: String::new(),
            show_all: false,
            group_mode: GroupMode::ByState,
            rows: Vec::new(),
            root_cache: HashMap::new(),
            armed_kill: None,
        };
        app.rebuild_rows();
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

    /// Rows suppressed by the default view (companion or hidden, when !show_all).
    pub fn hidden_count(&self) -> usize {
        // Stage 2 placeholder (deliberately wrong); Stream C counts companion + hidden.
        0
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
        // Stage 2 placeholder (deliberately wrong); Stream C implements the arming.
        let _ = (now_ms, &self.armed_kill);
        KillStage::Noop
    }

    /// key '/'
    pub fn set_filter(&mut self, s: String) {
        self.filter = s;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// j/k/arrows — cursor only, never rebuilds the row cache.
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as i32 + delta).clamp(0, len as i32 - 1);
        self.selected = next as usize;
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

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Recompute the cached row model.
    ///
    /// STAGE 2 SCAFFOLD: a flat recency-sorted list of Session rows respecting the
    /// filter and the `hidden` flag (via show_all). It deliberately omits the ByState
    /// sections, ByProject headers, companion filtering, and the DONE overflow marker,
    /// and stamps placeholder summary/updated_at_ms — so the v2 behavior tests fail
    /// while the preserved filter/anchor behavior stays green. Stream C implements the
    /// real grouped model.
    fn rebuild_rows(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut indices: Vec<usize> = Vec::new();
        for (i, s) in self.sessions.iter().enumerate() {
            if s.hidden && !self.show_all {
                continue;
            }
            if !needle.is_empty() {
                let title = s.title.to_lowercase();
                let cwd = s.cwd.to_string_lossy().to_lowercase();
                if !(title.contains(&needle) || cwd.contains(&needle)) {
                    continue;
                }
            }
            indices.push(i);
        }
        indices.sort_by_key(|&i| std::cmp::Reverse(self.sessions[i].updated_at_ms));

        let mut rows = Vec::new();
        for i in indices {
            let s = &self.sessions[i];
            rows.push(Row::Session {
                backend: s.backend,
                id: s.id.clone(),
                title: s.title.clone(),
                status: s.status,
                hidden: s.hidden,
                // Placeholder (Stream C copies from the session).
                summary: String::new(),
                updated_at_ms: 0,
            });
        }
        self.rows = rows;
    }

    fn clamp_selection(&mut self) {
        let len = self.rows.len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

/// "42s" / "17m" / "3h" / "6d" (largest whole unit, no decimals; negative -> "0s").
pub fn format_elapsed(delta_ms: i64) -> String {
    // Stage 2 placeholder (deliberately wrong); Stream C implements the buckets.
    let _ = delta_ms;
    String::new()
}

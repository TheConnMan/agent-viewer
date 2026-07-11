use agent_viewer_core::group::project_root;
use agent_viewer_core::{BackendKind, Session, Status};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    GroupHeader {
        root: PathBuf,
        collapsed: bool,
        count: usize,
    },
    Session {
        backend: BackendKind,
        id: String,
        title: String,
        status: Status,
        hidden: bool,
    },
}

pub struct App {
    sessions: Vec<Session>,
    selected: usize,
    filter: String,
    show_hidden: bool,
    collapsed: HashSet<PathBuf>,
    /// Cached render model. Rebuilt only when sessions/filter/show_hidden/collapse
    /// change; cursor movement leaves it untouched.
    rows: Vec<Row>,
    /// Memoized project_root(cwd). cwds are stable, so this survives refresh ticks
    /// and stops re-walking the filesystem for sessions we have already grouped.
    root_cache: HashMap<PathBuf, PathBuf>,
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        let mut app = App {
            sessions,
            selected: 0,
            filter: String::new(),
            show_hidden: false,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            root_cache: HashMap::new(),
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

    /// The render model (a clone of the cached rows). Rules: hidden sessions excluded
    /// unless show_hidden; filter = case-insensitive substring over title and cwd;
    /// collapsed groups emit only their GroupHeader (count = sessions in group);
    /// grouping/order matches core::group_by_project.
    pub fn visible(&self) -> Vec<Row> {
        self.rows.clone()
    }

    /// key 'a'
    pub fn toggle_show_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_rows();
        self.clamp_selection();
    }

    /// key ' '
    pub fn toggle_collapse_selected(&mut self) {
        if let Some(root) = self.selected_root() {
            if !self.collapsed.remove(&root) {
                self.collapsed.insert(root);
            }
            self.rebuild_rows();
        }
        self.clamp_selection();
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
            Row::GroupHeader { .. } => None,
        }
    }

    /// Index of the currently selected row (aligns 1:1 with `visible()`).
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Project root of the group the selection sits in (header or session row).
    pub fn selected_group_root(&self) -> Option<PathBuf> {
        self.selected_root()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Recompute the cached row model from sessions/filter/show_hidden/collapse.
    /// Grouping/ordering mirrors core::group_by_project but resolves roots through
    /// the memo so repeated cwds skip the filesystem walk.
    fn rebuild_rows(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut indices: Vec<usize> = Vec::new();
        for (i, s) in self.sessions.iter().enumerate() {
            if s.hidden && !self.show_hidden {
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

        let mut by_root: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for &i in &indices {
            let root = self.resolve_root(i);
            by_root.entry(root).or_default().push(i);
        }

        let mut groups: Vec<(PathBuf, Vec<usize>)> = by_root.into_iter().collect();
        for (_, idxs) in groups.iter_mut() {
            idxs.sort_by_key(|&i| std::cmp::Reverse(self.sessions[i].updated_at_ms));
        }
        groups.sort_by(|a, b| {
            let a_newest = a.1.iter().map(|&i| self.sessions[i].updated_at_ms).max();
            let b_newest = b.1.iter().map(|&i| self.sessions[i].updated_at_ms).max();
            b_newest.cmp(&a_newest)
        });

        let mut rows = Vec::new();
        for (root, idxs) in groups {
            let collapsed = self.collapsed.contains(&root);
            rows.push(Row::GroupHeader {
                root,
                collapsed,
                count: idxs.len(),
            });
            if collapsed {
                continue;
            }
            for i in idxs {
                let s = &self.sessions[i];
                rows.push(Row::Session {
                    backend: s.backend,
                    id: s.id.clone(),
                    title: s.title.clone(),
                    status: s.status,
                    hidden: s.hidden,
                });
            }
        }
        self.rows = rows;
    }

    fn resolve_root(&mut self, idx: usize) -> PathBuf {
        let cwd = self.sessions[idx].cwd.clone();
        if let Some(root) = self.root_cache.get(&cwd) {
            return root.clone();
        }
        let root = project_root(&cwd);
        self.root_cache.insert(cwd, root.clone());
        root
    }

    fn selected_root(&self) -> Option<PathBuf> {
        match self.rows.get(self.selected)? {
            Row::GroupHeader { root, .. } => Some(root.clone()),
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
        }
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

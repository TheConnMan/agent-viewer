use codex_viewer_core::group::{group_by_project, project_root};
use codex_viewer_core::{BackendKind, Session, Status};
use std::collections::HashSet;
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
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        App {
            sessions,
            selected: 0,
            filter: String::new(),
            show_hidden: false,
            collapsed: HashSet::new(),
        }
    }

    /// Replace the session set, keeping the current selection anchored to the same
    /// (backend,id) session row when it survives the refresh; otherwise clamp.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let anchor = self.selected().map(|s| (s.backend, s.id.clone()));
        self.sessions = sessions;
        if let Some((backend, id)) = anchor {
            let rows = self.visible();
            if let Some(idx) = rows.iter().position(|r| {
                matches!(r, Row::Session { backend: b, id: rid, .. } if *b == backend && rid == &id)
            }) {
                self.selected = idx;
                return;
            }
        }
        self.clamp_selection();
    }

    /// The render model. Rules: hidden sessions excluded unless show_hidden;
    /// filter = case-insensitive substring over title and cwd;
    /// collapsed groups emit only their GroupHeader (count = sessions in group);
    /// grouping/order via core::group_by_project.
    pub fn visible(&self) -> Vec<Row> {
        let groups = group_by_project(self.filtered_sessions());
        let mut rows = Vec::new();
        for group in groups {
            let collapsed = self.collapsed.contains(&group.root);
            rows.push(Row::GroupHeader {
                root: group.root.clone(),
                collapsed,
                count: group.sessions.len(),
            });
            if collapsed {
                continue;
            }
            for s in group.sessions {
                rows.push(Row::Session {
                    backend: s.backend,
                    id: s.id,
                    title: s.title,
                    status: s.status,
                    hidden: s.hidden,
                });
            }
        }
        rows
    }

    /// key 'a'
    pub fn toggle_show_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.clamp_selection();
    }

    /// key ' '
    pub fn toggle_collapse_selected(&mut self) {
        if let Some(root) = self.selected_root()
            && !self.collapsed.remove(&root)
        {
            self.collapsed.insert(root);
        }
        self.clamp_selection();
    }

    /// key '/'
    pub fn set_filter(&mut self, s: String) {
        self.filter = s;
        self.clamp_selection();
    }

    /// j/k/arrows
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as i32 + delta).clamp(0, len as i32 - 1);
        self.selected = next as usize;
    }

    pub fn selected(&self) -> Option<&Session> {
        let rows = self.visible();
        match rows.get(self.selected)? {
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

    fn filtered_sessions(&self) -> Vec<Session> {
        let needle = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| {
                if s.hidden && !self.show_hidden {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                let title = s.title.to_lowercase();
                let cwd = s.cwd.to_string_lossy().to_lowercase();
                title.contains(&needle) || cwd.contains(&needle)
            })
            .cloned()
            .collect()
    }

    fn selected_root(&self) -> Option<PathBuf> {
        let rows = self.visible();
        match rows.get(self.selected)? {
            Row::GroupHeader { root, .. } => Some(root.clone()),
            Row::Session { backend, id, .. } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.backend == *backend && &s.id == id)?;
                Some(project_root(&session.cwd))
            }
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

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
    _sessions: Vec<Session>,
    _selected: usize,
    _filter: String,
    _show_hidden: bool,
    _collapsed: HashSet<PathBuf>,
}

impl App {
    pub fn new(sessions: Vec<Session>) -> App {
        let _ = sessions;
        todo!()
    }
    /// keep selection by id
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let _ = sessions;
        todo!()
    }
    /// The render model. Rules: hidden sessions excluded unless show_hidden;
    /// filter = case-insensitive substring over title and cwd;
    /// collapsed groups emit only their GroupHeader (count = sessions hidden);
    /// grouping/order via core::group_by_project.
    pub fn visible(&self) -> Vec<Row> {
        todo!()
    }
    /// key 'a'
    pub fn toggle_show_hidden(&mut self) {
        todo!()
    }
    /// key ' '
    pub fn toggle_collapse_selected(&mut self) {
        todo!()
    }
    /// key '/'
    pub fn set_filter(&mut self, s: String) {
        let _ = s;
        todo!()
    }
    /// j/k/arrows
    pub fn move_selection(&mut self, delta: i32) {
        let _ = delta;
        todo!()
    }
    pub fn selected(&self) -> Option<&Session> {
        todo!()
    }
}

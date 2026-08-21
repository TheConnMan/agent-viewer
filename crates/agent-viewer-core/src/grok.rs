use crate::backend::{Backend, BackendKind, Capabilities, Session, SpawnResult};
use crate::error::{AttachRefusal, Error, Result};
use std::path::{Path, PathBuf};

/// Nonsecret health information for the official Grok runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokDiagnostics {
    pub binary: PathBuf,
    pub home: PathBuf,
    pub binary_available: bool,
    pub leader_count: usize,
    pub registered: bool,
    pub methods: Vec<String>,
}

/// The sole public entry point for Grok storage and lifecycle operations.
///
/// Phase 1 protocol behavior is added behind this surface. Keeping it separate from
/// `GrokBackend` lets other local clients reuse the official implementation without
/// reimplementing storage discovery or ACP.
pub struct GrokLifecycle {
    binary: PathBuf,
    home: PathBuf,
}

impl GrokLifecycle {
    pub fn new(binary: impl Into<PathBuf>, home: impl Into<PathBuf>) -> GrokLifecycle {
        GrokLifecycle {
            binary: binary.into(),
            home: home.into(),
        }
    }

    pub fn diagnostics(&self) -> Result<GrokDiagnostics> {
        let _ = (&self.binary, &self.home);
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        Ok(Vec::new())
    }

    pub fn spawn(&self, cwd: &Path, task: &str, model: Option<&str>) -> Result<SpawnResult> {
        let _ = (cwd, task, model);
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        let _ = session_id;
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        let _ = (session_id, title);
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn delete(&self, session_id: &str) -> Result<()> {
        let _ = session_id;
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn models(&self) -> Result<Vec<String>> {
        Ok(vec![BackendKind::Grok.default_model().to_string()])
    }
}

pub struct GrokBackend {
    lifecycle: GrokLifecycle,
}

impl GrokBackend {
    pub fn new() -> GrokBackend {
        GrokBackend {
            lifecycle: GrokLifecycle::new("grok", default_grok_home()),
        }
    }
}

impl Default for GrokBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for GrokBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Grok
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        self.lifecycle.list()
    }

    fn spawn(
        &self,
        dir: &Path,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<SpawnResult> {
        let _ = effort;
        self.lifecycle.spawn(dir, task, model)
    }

    fn available_models(&self) -> Vec<String> {
        self.lifecycle
            .models()
            .unwrap_or_else(|_| vec![BackendKind::Grok.default_model().to_string()])
    }

    fn stop(&self, session: &Session) -> Result<()> {
        self.lifecycle.cancel(&session.id)
    }

    fn remove(&self, session: &Session) -> Result<()> {
        self.lifecycle.delete(&session.id)
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        self.lifecycle.rename(&session.id, name)
    }

    fn attach_command(
        &self,
        _session: &Session,
    ) -> std::result::Result<std::process::Command, AttachRefusal> {
        Err(AttachRefusal::new(
            "grok sessions cannot be attached until lifecycle support is available",
        ))
    }
}

fn default_grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home_dir().join(".grok"))
}

use agent_viewer_core::{
    Backend, BackendKind, ListingCacheClaim, ListingCacheRead, ListingCacheScope,
    ListingCacheSnapshot, Session, ViewerDb,
    state::{LISTING_CACHE_FRESHNESS_MS, LISTING_CACHE_LEASE_MS},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshCursor {
    published_generation: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    Authoritative {
        sessions: Vec<Session>,
    },
    Shared {
        sessions: Vec<Session>,
    },
    Stale {
        sessions: Vec<Session>,
    },
    SourceError {
        sessions: Vec<Session>,
        notice: String,
    },
    CachedError {
        sessions: Vec<Session>,
        notice: String,
    },
    Waiting,
    Unchanged,
}

fn source_error(backend: BackendKind, error: agent_viewer_core::Error) -> String {
    format!("{}: {error}", backend.name())
}

fn source_only<F>(backend: BackendKind, source: F) -> (RefreshOutcome, bool)
where
    F: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
{
    match source() {
        Ok(sessions) => (RefreshOutcome::Authoritative { sessions }, false),
        Err(error) => (
            RefreshOutcome::SourceError {
                sessions: Vec::new(),
                notice: source_error(backend, error),
            },
            false,
        ),
    }
}

fn cached_fallback(
    db: &ViewerDb,
    scope: &ListingCacheScope,
    cursor: &mut RefreshCursor,
    now_ms: i64,
) -> Option<Vec<Session>> {
    match db.read_listing_snapshot(scope, now_ms).ok()? {
        ListingCacheRead::Fresh(snapshot) | ListingCacheRead::Stale(snapshot) => {
            let published_generation = snapshot.published_generation();
            cursor.published_generation = Some(published_generation);
            Some(snapshot.into_sessions())
        }
        ListingCacheRead::Miss => None,
    }
}

fn refresh_scoped_inner<F>(
    db: &ViewerDb,
    scope: Option<&ListingCacheScope>,
    backend: BackendKind,
    cursor: &mut RefreshCursor,
    now_ms: i64,
    source: F,
) -> (RefreshOutcome, bool)
where
    F: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
{
    let Some(scope) = scope else {
        return source_only(backend, source);
    };

    let claim = match db.claim_listing_refresh(
        Some(scope),
        cursor.published_generation,
        now_ms,
        LISTING_CACHE_FRESHNESS_MS,
        LISTING_CACHE_LEASE_MS,
    ) {
        Ok(claim) => claim,
        Err(_) => return source_only(backend, source),
    };

    match claim {
        ListingCacheClaim::Fresh(snapshot) => {
            let published_generation = snapshot.published_generation();
            cursor.published_generation = Some(published_generation);
            (
                RefreshOutcome::Shared {
                    sessions: snapshot.into_sessions(),
                },
                true,
            )
        }
        ListingCacheClaim::Unchanged => (RefreshOutcome::Unchanged, true),
        ListingCacheClaim::LeaseHeld => {
            if let Some(sessions) = cached_fallback(db, scope, cursor, now_ms) {
                return (RefreshOutcome::Stale { sessions }, true);
            }
            (RefreshOutcome::Waiting, false)
        }
        ListingCacheClaim::Bypass => source_only(backend, source),
        ListingCacheClaim::Claimed(lease) => {
            let guard = match db.guard_listing_refresh(&lease, LISTING_CACHE_LEASE_MS) {
                Ok(guard) => guard,
                Err(error) => {
                    let _ = db.fail_listing_refresh(&lease);
                    let fallback = cached_fallback(db, scope, cursor, now_ms);
                    let has_fallback = fallback.is_some();
                    return (
                        RefreshOutcome::SourceError {
                            sessions: fallback.unwrap_or_default(),
                            notice: source_error(backend, error),
                        },
                        has_fallback,
                    );
                }
            };
            let outcome = match source() {
                Ok(sessions) => {
                    match ListingCacheSnapshot::from_sessions(sessions.clone()) {
                        Ok(snapshot) => match db.publish_listing(&lease, snapshot, now_ms) {
                            Ok(agent_viewer_core::ListingCacheWrite::Published) => {
                                cursor.published_generation =
                                    Some(lease.next_published_generation());
                            }
                            Ok(agent_viewer_core::ListingCacheWrite::Fenced) | Err(_) => {
                                cursor.published_generation = None;
                            }
                        },
                        Err(_) => {
                            cursor.published_generation = None;
                            let _ = db.fail_listing_refresh(&lease);
                        }
                    }
                    (RefreshOutcome::Authoritative { sessions }, false)
                }
                Err(error) => {
                    let _ = db.fail_listing_refresh(&lease);
                    let fallback = cached_fallback(db, scope, cursor, now_ms);
                    let has_fallback = fallback.is_some();
                    (
                        RefreshOutcome::SourceError {
                            sessions: fallback.unwrap_or_default(),
                            notice: source_error(backend, error),
                        },
                        has_fallback,
                    )
                }
            };
            drop(guard);
            outcome
        }
    }
}

pub fn refresh_scoped<F>(
    db: &ViewerDb,
    scope: Option<&ListingCacheScope>,
    cursor: &mut RefreshCursor,
    now_ms: i64,
    source: F,
) -> RefreshOutcome
where
    F: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
{
    let backend = scope
        .map(ListingCacheScope::backend)
        .unwrap_or(BackendKind::Codex);
    refresh_scoped_inner(db, scope, backend, cursor, now_ms, source).0
}

pub fn refresh_backend(
    db: Option<&ViewerDb>,
    backend: &mut dyn Backend,
    last_good: &[Session],
    cursor: &mut RefreshCursor,
    now_ms: i64,
) -> RefreshOutcome {
    let kind = backend.kind();
    let scope = backend.listing_scope();
    let (outcome, has_shared_fallback) = match db {
        Some(db) => {
            refresh_scoped_inner(db, scope.as_ref(), kind, cursor, now_ms, || backend.list())
        }
        None => source_only(kind, || backend.list()),
    };
    match outcome {
        RefreshOutcome::SourceError { sessions, notice } if has_shared_fallback => {
            RefreshOutcome::CachedError { sessions, notice }
        }
        RefreshOutcome::SourceError { notice, .. } => RefreshOutcome::SourceError {
            sessions: last_good.to_vec(),
            notice,
        },
        other => other,
    }
}

pub fn invalidate_backend_scope(db: Option<&ViewerDb>, backend: &dyn Backend) {
    let Some(db) = db else {
        return;
    };
    let scope = backend.listing_scope();
    let _ = db.invalidate_listing_scope(scope.as_ref());
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetRequest {
    backend: BackendKind,
    id: String,
}

impl TargetRequest {
    pub fn new(backend: BackendKind, id: impl Into<String>) -> Self {
        Self {
            backend,
            id: id.into(),
        }
    }

    pub fn backend(&self) -> BackendKind {
        self.backend
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl From<&Session> for TargetRequest {
    fn from(session: &Session) -> Self {
        Self::new(session.backend, session.id.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpawnDirectoryMode {
    WorkingDirectory,
    ProjectRoot,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SpawnTarget {
    Session {
        request: TargetRequest,
        mode: SpawnDirectoryMode,
        displayed_directory: std::path::PathBuf,
    },
    ExplicitDirectory(std::path::PathBuf),
}

impl SpawnTarget {
    pub fn displayed_directory(&self) -> &std::path::Path {
        match self {
            Self::Session {
                displayed_directory,
                ..
            }
            | Self::ExplicitDirectory(displayed_directory) => displayed_directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolution {
    Permitted,
    Refused { notice: String },
    Missing { notice: String },
    SourceError { notice: String },
}

impl TargetResolution {
    pub fn permitted() -> Self {
        Self::Permitted
    }

    pub fn refused(notice: impl Into<String>) -> Self {
        Self::Refused {
            notice: notice.into(),
        }
    }

    pub fn notice(&self) -> Option<&str> {
        match self {
            Self::Permitted => None,
            Self::Refused { notice } | Self::Missing { notice } | Self::SourceError { notice } => {
                Some(notice)
            }
        }
    }
}

fn target_from_listing(
    request: &TargetRequest,
    sessions: Vec<Session>,
) -> Result<Session, TargetResolution> {
    sessions
        .into_iter()
        .find(|session| session.backend == request.backend && session.id == request.id)
        .ok_or_else(|| TargetResolution::Missing {
            notice: format!("{} session is no longer available", request.backend.name()),
        })
}

pub fn authoritative_target(
    backend: &mut dyn Backend,
    request: &TargetRequest,
) -> Result<Session, TargetResolution> {
    if backend.kind() != request.backend {
        return Err(TargetResolution::Missing {
            notice: format!("{} backend is no longer available", request.backend.name()),
        });
    }
    let sessions = backend
        .list()
        .map_err(|error| TargetResolution::SourceError {
            notice: source_error(request.backend, error),
        })?;
    target_from_listing(request, sessions)
}

pub fn resolve_target<L, A>(request: TargetRequest, list: L, action: A) -> TargetResolution
where
    L: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
    A: FnOnce(&Session) -> TargetResolution,
{
    let sessions = match list() {
        Ok(sessions) => sessions,
        Err(error) => {
            return TargetResolution::SourceError {
                notice: source_error(request.backend, error),
            };
        }
    };
    match target_from_listing(&request, sessions) {
        Ok(session) => action(&session),
        Err(resolution) => resolution,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedPaletteAction {
    Attach,
    Archive,
    Unarchive,
    Rename,
    StopOrRemove,
}

pub fn dispatch_cached_palette<F>(
    target: TargetRequest,
    action: CachedPaletteAction,
    dispatch: F,
) -> TargetResolution
where
    F: FnOnce(TargetRequest, CachedPaletteAction) -> TargetResolution,
{
    dispatch(target, action)
}

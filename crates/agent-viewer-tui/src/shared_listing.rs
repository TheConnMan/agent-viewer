use agent_viewer_core::{
    Backend, BackendKind, ListingCacheClaim, ListingCacheRead, ListingCacheScope,
    ListingCacheSnapshot, Session, ViewerDb,
};

const LISTING_FRESHNESS_MS: i64 = 1_000;
const LISTING_LEASE_MS: i64 = 2_000;

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
}

impl RefreshOutcome {
    pub fn sessions(&self) -> &[Session] {
        match self {
            Self::Authoritative { sessions }
            | Self::Shared { sessions }
            | Self::Stale { sessions }
            | Self::SourceError { sessions, .. } => sessions,
        }
    }

    pub fn into_sessions(self) -> Vec<Session> {
        match self {
            Self::Authoritative { sessions }
            | Self::Shared { sessions }
            | Self::Stale { sessions }
            | Self::SourceError { sessions, .. } => sessions,
        }
    }
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

fn cached_fallback(db: &ViewerDb, scope: &ListingCacheScope, now_ms: i64) -> Option<Vec<Session>> {
    match db.read_listing_snapshot(scope, now_ms).ok()? {
        ListingCacheRead::Fresh(snapshot) | ListingCacheRead::Stale(snapshot) => {
            Some(snapshot.sessions().to_vec())
        }
        ListingCacheRead::Miss => None,
    }
}

fn refresh_scoped_inner<F>(
    db: &ViewerDb,
    scope: Option<&ListingCacheScope>,
    backend: BackendKind,
    now_ms: i64,
    source: F,
) -> (RefreshOutcome, bool)
where
    F: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
{
    let Some(scope) = scope else {
        return source_only(backend, source);
    };

    let claim =
        match db.claim_listing_refresh(Some(scope), now_ms, LISTING_FRESHNESS_MS, LISTING_LEASE_MS)
        {
            Ok(claim) => claim,
            Err(_) => return source_only(backend, source),
        };

    match claim {
        ListingCacheClaim::Fresh(snapshot) => (
            RefreshOutcome::Shared {
                sessions: snapshot.sessions().to_vec(),
            },
            true,
        ),
        ListingCacheClaim::LeaseHeld => {
            if let Some(sessions) = cached_fallback(db, scope, now_ms) {
                return (RefreshOutcome::Stale { sessions }, true);
            }
            source_only(backend, source)
        }
        ListingCacheClaim::Bypass => source_only(backend, source),
        ListingCacheClaim::Claimed(lease) => match source() {
            Ok(sessions) => {
                match ListingCacheSnapshot::from_sessions(sessions.clone()) {
                    Ok(snapshot) => {
                        let _ = db.publish_listing(&lease, snapshot, now_ms);
                    }
                    Err(_) => {
                        let _ = db.fail_listing_refresh(&lease);
                    }
                }
                (RefreshOutcome::Authoritative { sessions }, false)
            }
            Err(error) => {
                let _ = db.fail_listing_refresh(&lease);
                let fallback = cached_fallback(db, scope, now_ms);
                let has_fallback = fallback.is_some();
                (
                    RefreshOutcome::SourceError {
                        sessions: fallback.unwrap_or_default(),
                        notice: source_error(backend, error),
                    },
                    has_fallback,
                )
            }
        },
    }
}

pub fn refresh_scoped<F>(
    db: &ViewerDb,
    scope: Option<&ListingCacheScope>,
    now_ms: i64,
    source: F,
) -> RefreshOutcome
where
    F: FnOnce() -> agent_viewer_core::Result<Vec<Session>>,
{
    let backend = scope
        .map(ListingCacheScope::backend)
        .unwrap_or(BackendKind::Codex);
    refresh_scoped_inner(db, scope, backend, now_ms, source).0
}

pub fn refresh_backend(
    db: Option<&ViewerDb>,
    backend: &mut dyn Backend,
    last_good: &[Session],
    now_ms: i64,
) -> RefreshOutcome {
    let kind = backend.kind();
    let scope = backend.listing_scope();
    let (mut outcome, has_shared_fallback) = match db {
        Some(db) => refresh_scoped_inner(db, scope.as_ref(), kind, now_ms, || backend.list()),
        None => source_only(kind, || backend.list()),
    };
    if !has_shared_fallback && let RefreshOutcome::SourceError { sessions, .. } = &mut outcome {
        *sessions = last_good.to_vec();
    }
    outcome
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

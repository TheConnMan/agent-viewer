use crate::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin, Status};
use crate::error::{AttachRefusal, Error, Result};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const SECURE_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
const SESSION_PAGE_SIZE: usize = 10_000;
const MAX_SESSION_ROWS: usize = 100_000;

type Launcher = dyn Fn(Command) -> io::Result<u32> + Send + Sync;
type BeforeAuthorizedWrite = dyn Fn() + Send + Sync;

#[derive(Clone)]
pub struct OpencodeRuntime {
    inner: Arc<RuntimeInner>,
}

#[doc(hidden)]
pub struct OpencodeRuntimeTestConfig {
    pub candidates: [SocketAddr; 2],
    pub startup_timeout: Duration,
    pub durable_cwd: PathBuf,
    pub launcher: Arc<dyn Fn(Command) -> io::Result<u32> + Send + Sync>,
    pub viewer_db_path: PathBuf,
    pub proc_root: PathBuf,
    pub password_override: Option<String>,
    pub before_authorized_write: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct RuntimeInner {
    candidates: [SocketAddr; 2],
    startup_timeout: Duration,
    durable_cwd: PathBuf,
    launcher: Arc<Launcher>,
    secure: Option<SecureRuntimeConfig>,
    state: Mutex<RuntimeState>,
}

struct SecureRuntimeConfig {
    viewer_db_path: PathBuf,
    proc_root: PathBuf,
    password_override: Option<String>,
    before_authorized_write: Option<Arc<BeforeAuthorizedWrite>>,
    startup_log_path: Option<PathBuf>,
}

#[derive(Default)]
struct RuntimeState {
    generation: u64,
    pin: Option<ServerPin>,
    healthy: bool,
    managed_ids: HashSet<String>,
    managed_ids_pin: Option<ServerPin>,
    managed_server_pin: Option<ServerPin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerPin {
    pid: u32,
    start_time: u64,
    listener_inode: u64,
    effective_uid: u32,
    argv: Vec<OsString>,
    endpoint: SocketAddr,
    generation: u64,
}

#[derive(Clone)]
struct ManagedLease {
    pin: ServerPin,
    id: String,
}

#[derive(Clone, Copy)]
enum RequestAuthority<'a> {
    CurrentPin,
    ManagedServer,
    ManagedId(&'a str),
}

#[derive(Clone)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
    listener_inode: u64,
    effective_uid: u32,
    argv: Vec<OsString>,
    endpoint: SocketAddr,
}

#[derive(Clone)]
struct Credentials {
    username: String,
    password: String,
    authorization: String,
}

struct HttpReply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

enum SecureProbe {
    Healthy(ServerPin),
    Missing,
    PinnedUnhealthy(String),
}

enum CandidateProbe {
    Healthy(ServerPin),
    Missing(String),
    Unavailable(String),
    PinnedUnhealthy(String),
}

enum PreflightFailure {
    Missing(String),
    Unavailable(String),
}

struct AdvisoryLockEntry {
    held: Mutex<bool>,
    released: Condvar,
}

struct AdvisoryLockGuard {
    local: Arc<AdvisoryLockEntry>,
    file: File,
}

impl PreflightFailure {
    fn into_message(self) -> String {
        match self {
            Self::Missing(message) | Self::Unavailable(message) => message,
        }
    }
}

impl OpencodeRuntime {
    pub fn new() -> Self {
        let home = crate::home_dir();
        let durable_cwd = if !home.as_os_str().is_empty() && home.is_dir() {
            home.clone()
        } else {
            PathBuf::from("/")
        };
        let log_path = crate::spawn::viewer_log_path("opencode-server");
        let launcher_log_path = log_path.clone();
        Self::from_parts(
            [
                SocketAddr::from(([127, 0, 0, 1], 4097)),
                SocketAddr::from(([127, 0, 0, 1], 4098)),
            ],
            SERVER_STARTUP_TIMEOUT,
            durable_cwd,
            Arc::new(move |command| {
                crate::spawn::spawn_detached(command, &launcher_log_path).map_err(|error| {
                    io::Error::other(format!(
                        "server log {}: {error}",
                        launcher_log_path.display()
                    ))
                })
            }),
            Some(SecureRuntimeConfig {
                viewer_db_path: home.join(".local/state/agent-viewer/viewer.db"),
                proc_root: PathBuf::from("/proc"),
                password_override: None,
                before_authorized_write: None,
                startup_log_path: Some(log_path),
            }),
        )
    }

    #[doc(hidden)]
    pub fn for_test_secure(config: OpencodeRuntimeTestConfig) -> Self {
        assert!(
            config
                .candidates
                .iter()
                .all(|candidate| candidate.ip().is_loopback())
        );
        Self::from_parts(
            config.candidates,
            config.startup_timeout,
            config.durable_cwd,
            config.launcher,
            Some(SecureRuntimeConfig {
                viewer_db_path: config.viewer_db_path,
                proc_root: config.proc_root,
                password_override: config.password_override,
                before_authorized_write: config.before_authorized_write,
                startup_log_path: None,
            }),
        )
    }

    fn from_parts(
        candidates: [SocketAddr; 2],
        startup_timeout: Duration,
        durable_cwd: PathBuf,
        launcher: Arc<Launcher>,
        secure: Option<SecureRuntimeConfig>,
    ) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                candidates,
                startup_timeout,
                durable_cwd,
                launcher,
                secure,
                state: Mutex::new(RuntimeState::default()),
            }),
        }
    }

    fn compatibility_only() -> Self {
        Self::from_parts(
            [
                SocketAddr::from(([127, 0, 0, 1], 0)),
                SocketAddr::from(([127, 0, 0, 1], 0)),
            ],
            Duration::from_millis(20),
            PathBuf::from("/"),
            Arc::new(|_| Err(io::Error::other("compatibility runtime cannot launch"))),
            None,
        )
    }

    fn state(&self) -> MutexGuard<'_, RuntimeState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn health_timeout(&self) -> Duration {
        self.inner
            .startup_timeout
            .min(SECURE_HEALTH_PROBE_TIMEOUT)
            .max(Duration::from_millis(1))
    }

    fn request(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> std::result::Result<HttpReply, String> {
        let credentials = self.credentials()?;
        self.authorized_request(endpoint, method, target, body, timeout, &credentials)
    }

    fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<T> {
        let reply = self
            .request(endpoint, method, target, body, HTTP_REQUEST_TIMEOUT)
            .map_err(Error::Command)?;
        if reply.status != expected_status {
            return Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )));
        }
        serde_json::from_slice(&reply.body).map_err(|error| {
            Error::Command(format!("{method} {target} returned invalid JSON: {error}"))
        })
    }

    fn managed_request_json<T: serde::de::DeserializeOwned>(
        &self,
        lease: &ManagedLease,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<T> {
        self.revalidate_managed_lease(lease)?;
        let credentials = self.credentials().map_err(Error::Command)?;
        let reply = self
            .authorized_request_with_pin(
                &lease.pin,
                RequestParts {
                    method,
                    target,
                    body,
                },
                HTTP_REQUEST_TIMEOUT,
                &credentials,
                RequestAuthority::ManagedId(&lease.id),
            )
            .map_err(Error::Command)?;
        if reply.status != expected_status {
            return Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )));
        }
        serde_json::from_slice(&reply.body).map_err(|error| {
            Error::Command(format!("{method} {target} returned invalid JSON: {error}"))
        })
    }

    fn managed_expect_status(
        &self,
        lease: &ManagedLease,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<()> {
        self.revalidate_managed_lease(lease)?;
        let credentials = self.credentials().map_err(Error::Command)?;
        let reply = self
            .authorized_request_with_pin(
                &lease.pin,
                RequestParts {
                    method,
                    target,
                    body,
                },
                HTTP_REQUEST_TIMEOUT,
                &credentials,
                RequestAuthority::ManagedId(&lease.id),
            )
            .map_err(Error::Command)?;
        if reply.status == expected_status {
            Ok(())
        } else {
            Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )))
        }
    }

    fn managed_server_request_json<T: serde::de::DeserializeOwned>(
        &self,
        pin: &ServerPin,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<T> {
        let credentials = self.credentials().map_err(Error::Command)?;
        let reply = self
            .authorized_request_with_pin(
                pin,
                RequestParts {
                    method,
                    target,
                    body,
                },
                HTTP_REQUEST_TIMEOUT,
                &credentials,
                RequestAuthority::ManagedServer,
            )
            .map_err(Error::Command)?;
        if reply.status != expected_status {
            return Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )));
        }
        serde_json::from_slice(&reply.body).map_err(|error| {
            Error::Command(format!("{method} {target} returned invalid JSON: {error}"))
        })
    }

    fn managed_server_expect_status(
        &self,
        pin: &ServerPin,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<()> {
        let credentials = self.credentials().map_err(Error::Command)?;
        let reply = self
            .authorized_request_with_pin(
                pin,
                RequestParts {
                    method,
                    target,
                    body,
                },
                HTTP_REQUEST_TIMEOUT,
                &credentials,
                RequestAuthority::ManagedServer,
            )
            .map_err(Error::Command)?;
        if reply.status == expected_status {
            Ok(())
        } else {
            Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )))
        }
    }

    fn revalidate_managed_lease(&self, lease: &ManagedLease) -> Result<()> {
        if !self.managed_lease_is_current(&lease.pin, &lease.id) {
            return Err(Error::Unsupported(BackendKind::Opencode.name()));
        }
        let credentials = self.credentials().map_err(Error::Command)?;
        if let Err(error) = self.authenticated_health(&lease.pin, &credentials) {
            self.publish_health(&lease.pin, false);
            return Err(Error::Command(credentials.sanitize(&error)));
        }
        if self.managed_lease_is_current(&lease.pin, &lease.id) {
            Ok(())
        } else {
            Err(Error::Unsupported(BackendKind::Opencode.name()))
        }
    }

    fn ensure_server(&self) -> std::result::Result<ServerPin, String> {
        if self.inner.secure.is_none() {
            return Err("secure OpenCode server runtime is unavailable".to_string());
        }
        self.ensure_secure_server()
    }

    fn healthy_tier(&self) -> bool {
        let state = self.state();
        self.inner.secure.is_some() && state.healthy && state.pin.is_some()
    }

    fn server_for_listing(&self) -> Result<Option<ServerPin>> {
        if self.inner.secure.is_none() {
            return Ok(None);
        }
        match self.probe_secure().map_err(Error::Command)? {
            SecureProbe::Healthy(pin) => Ok(Some(pin)),
            SecureProbe::Missing => Ok(None),
            SecureProbe::PinnedUnhealthy(error) => Err(Error::Command(error)),
        }
    }

    fn credentials(&self) -> std::result::Result<Credentials, String> {
        let secure = self
            .inner
            .secure
            .as_ref()
            .ok_or_else(|| "secure credentials are unavailable".to_string())?;
        let configured = secure
            .password_override
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let environment = std::env::var("OPENCODE_SERVER_PASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let (username, password) = if let Some(password) = configured {
            ("agent-viewer".to_string(), password.to_string())
        } else if let Some(password) = environment {
            let username = std::env::var("OPENCODE_SERVER_USERNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "opencode".to_string());
            (username, password)
        } else {
            let database = crate::state::ViewerDb::open(&secure.viewer_db_path)
                .map_err(|error| format!("could not resolve server credential: {error}"))?;
            let password = database
                .opencode_server_secret()
                .map_err(|error| format!("could not resolve server secret: {error}"))?;
            if password.is_empty() {
                return Err("stored server credential is empty".to_string());
            }
            ("agent-viewer".to_string(), password)
        };
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{password}").as_bytes());
        Ok(Credentials {
            username,
            password,
            authorization: format!("Basic {encoded}"),
        })
    }

    fn probe_secure(&self) -> std::result::Result<SecureProbe, String> {
        let current_pin = { self.state().pin.clone() };
        if let Some(pin) = current_pin {
            if !identity_still_matches(self.secure_config()?.proc_root.as_path(), &pin) {
                self.reject_pin(&pin);
                return self.discover_secure(Some(pin.endpoint));
            }
            let credentials = self.credentials()?;
            match self.authenticated_health(&pin, &credentials) {
                Ok(()) => return Ok(SecureProbe::Healthy(pin)),
                Err(error) => {
                    if identity_still_matches(self.secure_config()?.proc_root.as_path(), &pin) {
                        self.publish_health(&pin, false);
                        return Ok(SecureProbe::PinnedUnhealthy(error));
                    }
                    self.reject_pin(&pin);
                    return self.discover_secure(Some(pin.endpoint));
                }
            }
        }
        self.discover_secure(None)
    }

    fn discover_secure(
        &self,
        skip: Option<SocketAddr>,
    ) -> std::result::Result<SecureProbe, String> {
        let mut unavailable = Vec::new();
        let mut credentials = None;
        for candidate in self.inner.candidates.iter().copied() {
            if Some(candidate) == skip {
                continue;
            }
            let deadline = Instant::now() + self.health_timeout();
            let (stream, identity) = match self.preflight_connection(candidate, None, deadline) {
                Ok(connection) => connection,
                Err(PreflightFailure::Missing(_)) => continue,
                Err(PreflightFailure::Unavailable(error)) => {
                    note_candidate_failure(&mut unavailable, candidate, error);
                    continue;
                }
            };
            let credentials = match credentials.as_ref() {
                Some(credentials) => credentials,
                None => credentials.insert(self.credentials()?),
            };
            match self.authenticate_secure_candidate(stream, identity, credentials, deadline) {
                CandidateProbe::Healthy(pin) => return Ok(SecureProbe::Healthy(pin)),
                CandidateProbe::PinnedUnhealthy(error) => {
                    note_candidate_failure(&mut unavailable, candidate, error);
                }
                CandidateProbe::Unavailable(error) => {
                    note_candidate_failure(&mut unavailable, candidate, error);
                }
                CandidateProbe::Missing(_) => {}
            }
        }
        if unavailable.is_empty() {
            Ok(SecureProbe::Missing)
        } else {
            Ok(SecureProbe::PinnedUnhealthy(format_candidate_failures(
                &unavailable,
            )))
        }
    }

    fn probe_secure_candidate(
        &self,
        endpoint: SocketAddr,
        credentials: &Credentials,
        expected_pid: Option<u32>,
        timeout: Duration,
    ) -> CandidateProbe {
        let deadline = Instant::now() + timeout;
        let (stream, identity) = match self.preflight_connection(endpoint, expected_pid, deadline) {
            Ok(connection) => connection,
            Err(PreflightFailure::Missing(error)) => {
                return CandidateProbe::Missing(error);
            }
            Err(PreflightFailure::Unavailable(error)) => {
                return CandidateProbe::Unavailable(error);
            }
        };
        self.authenticate_secure_candidate(stream, identity, credentials, deadline)
    }

    fn authenticate_secure_candidate(
        &self,
        mut stream: TcpStream,
        identity: ProcessIdentity,
        credentials: &Credentials,
        deadline: Instant,
    ) -> CandidateProbe {
        let pin = match self.publish_pin(identity) {
            Ok(pin) => pin,
            Err(error) => return CandidateProbe::Unavailable(error),
        };
        match self.authenticated_health_on_stream(&mut stream, &pin, credentials, deadline) {
            Ok(()) => CandidateProbe::Healthy(pin),
            Err(error) => {
                self.reject_pin(&pin);
                CandidateProbe::PinnedUnhealthy(error)
            }
        }
    }

    fn authenticated_health(
        &self,
        pin: &ServerPin,
        credentials: &Credentials,
    ) -> std::result::Result<(), String> {
        let reply = self.authorized_request_with_pin(
            pin,
            RequestParts {
                method: "GET",
                target: "/global/health",
                body: None,
            },
            self.health_timeout(),
            credentials,
            RequestAuthority::CurrentPin,
        )?;
        self.accept_authenticated_health(pin, reply)
    }

    fn authenticated_health_on_stream(
        &self,
        stream: &mut TcpStream,
        pin: &ServerPin,
        credentials: &Credentials,
        deadline: Instant,
    ) -> std::result::Result<(), String> {
        let reply = self.authorized_request_on_preflighted_stream(
            stream,
            pin,
            RequestParts {
                method: "GET",
                target: "/global/health",
                body: None,
            },
            credentials,
            deadline,
            RequestAuthority::CurrentPin,
        )?;
        self.accept_authenticated_health(pin, reply)
    }

    fn accept_authenticated_health(
        &self,
        pin: &ServerPin,
        reply: HttpReply,
    ) -> std::result::Result<(), String> {
        if reply.status != 200 {
            return Err(http_status_error("GET", "/global/health", reply.status));
        }
        let health: HealthResponse = serde_json::from_slice(&reply.body)
            .map_err(|error| format!("invalid health JSON: {error}"))?;
        if !health.healthy || health.version.is_empty() {
            return Err("health response did not contain healthy true and a version".to_string());
        }
        if !identity_still_matches(self.secure_config()?.proc_root.as_path(), pin) {
            self.reject_pin(pin);
            return Err("server identity changed while health was in flight".to_string());
        }
        if self.publish_health(pin, true) {
            Ok(())
        } else {
            Err("server generation changed while health was in flight".to_string())
        }
    }

    fn authorized_request(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        timeout: Duration,
        credentials: &Credentials,
    ) -> std::result::Result<HttpReply, String> {
        let pin = self
            .state()
            .pin
            .clone()
            .filter(|pin| pin.endpoint == endpoint)
            .ok_or_else(|| "verified OpenCode server identity is unavailable".to_string())?;
        self.authorized_request_with_pin(
            &pin,
            RequestParts {
                method,
                target,
                body,
            },
            timeout,
            credentials,
            RequestAuthority::CurrentPin,
        )
    }

    fn authorized_request_with_pin(
        &self,
        pin: &ServerPin,
        request: RequestParts<'_>,
        timeout: Duration,
        credentials: &Credentials,
        authority: RequestAuthority<'_>,
    ) -> std::result::Result<HttpReply, String> {
        let deadline = Instant::now() + timeout;
        let (mut stream, identity) = self
            .preflight_connection(pin.endpoint, Some(pin.pid), deadline)
            .map_err(PreflightFailure::into_message)?;
        if !pin_matches_identity(pin, &identity) || !self.pin_is_current(pin) {
            self.reject_pin(pin);
            return Err("server identity changed before authorized request".to_string());
        }
        self.authorized_request_on_preflighted_stream(
            &mut stream,
            pin,
            request,
            credentials,
            deadline,
            authority,
        )
    }

    fn preflight_connection(
        &self,
        endpoint: SocketAddr,
        expected_pid: Option<u32>,
        deadline: Instant,
    ) -> std::result::Result<(TcpStream, ProcessIdentity), PreflightFailure> {
        let deadline = deadline.min(Instant::now() + HEALTH_PROBE_TIMEOUT);
        let timeout = remaining_timeout(deadline).map_err(PreflightFailure::Missing)?;
        let mut stream = connect_stream(endpoint, timeout).map_err(PreflightFailure::Missing)?;
        let proc_root = self
            .secure_config()
            .map_err(PreflightFailure::Missing)?
            .proc_root
            .as_path();
        let identity = connected_identity(
            &stream,
            endpoint,
            proc_root,
            expected_pid,
            remaining_timeout(deadline).map_err(PreflightFailure::Missing)?,
        )
        .map_err(PreflightFailure::Missing)?;
        let reply = write_and_read_request(
            &mut stream,
            endpoint,
            RequestParts {
                method: "GET",
                target: "/global/health",
                body: None,
            },
            None,
            HttpConnection::KeepAlive,
            remaining_timeout(deadline).map_err(PreflightFailure::Unavailable)?,
        )
        .map_err(PreflightFailure::Unavailable)?;
        if reply.status != 401 {
            return Err(PreflightFailure::Missing(format!(
                "verified listener on port {} did not require authentication",
                endpoint.port()
            )));
        }
        accepted_connection_matches(
            proc_root,
            &stream,
            &identity,
            remaining_timeout(deadline).map_err(PreflightFailure::Unavailable)?,
        )
        .map_err(PreflightFailure::Unavailable)?;
        Ok((stream, identity))
    }

    fn authorized_request_on_preflighted_stream(
        &self,
        stream: &mut TcpStream,
        pin: &ServerPin,
        request: RequestParts<'_>,
        credentials: &Credentials,
        deadline: Instant,
        authority: RequestAuthority<'_>,
    ) -> std::result::Result<HttpReply, String> {
        if let Some(hook) = self.secure_config()?.before_authorized_write.as_ref() {
            hook();
        }
        let identity = connected_identity(
            stream,
            pin.endpoint,
            self.secure_config()?.proc_root.as_path(),
            Some(pin.pid),
            remaining_timeout(deadline)?,
        )?;
        let authority_is_current = match authority {
            RequestAuthority::CurrentPin => self.pin_is_current(pin),
            RequestAuthority::ManagedServer => self.managed_server_is_current(pin),
            RequestAuthority::ManagedId(id) => self.managed_lease_is_current(pin, id),
        };
        if !pin_matches_identity(pin, &identity)
            || accepted_connection_matches(
                self.secure_config()?.proc_root.as_path(),
                stream,
                &identity,
                remaining_timeout(deadline)?,
            )
            .is_err()
        {
            self.reject_pin(pin);
            return Err("server identity changed before authorized request".to_string());
        }
        if !authority_is_current {
            return Err("server authorization lease expired before request".to_string());
        }
        write_and_read_request(
            stream,
            pin.endpoint,
            request,
            Some(&credentials.authorization),
            HttpConnection::Close,
            remaining_timeout(deadline)?,
        )
        .map_err(|error| credentials.sanitize(&error))
    }

    fn secure_config(&self) -> std::result::Result<&SecureRuntimeConfig, String> {
        self.inner
            .secure
            .as_ref()
            .ok_or_else(|| "secure runtime configuration is unavailable".to_string())
    }

    fn publish_pin(&self, identity: ProcessIdentity) -> std::result::Result<ServerPin, String> {
        let mut state = self.state();
        if let Some(pin) = state.pin.as_ref() {
            if pin_matches_identity(pin, &identity) {
                return Ok(pin.clone());
            }
            return Err("server identity changed during concurrent discovery".to_string());
        }
        state.generation = state.generation.wrapping_add(1);
        let pin = ServerPin {
            pid: identity.pid,
            start_time: identity.start_time,
            listener_inode: identity.listener_inode,
            effective_uid: identity.effective_uid,
            argv: identity.argv,
            endpoint: identity.endpoint,
            generation: state.generation,
        };
        state.pin = Some(pin.clone());
        state.healthy = false;
        state.managed_ids.clear();
        state.managed_ids_pin = None;
        state.managed_server_pin = None;
        Ok(pin)
    }

    fn publish_health(&self, pin: &ServerPin, healthy: bool) -> bool {
        let mut state = self.state();
        if state.pin.as_ref() == Some(pin) && state.generation == pin.generation {
            state.healthy = healthy;
            if !healthy {
                state.managed_ids.clear();
                state.managed_ids_pin = None;
            }
            true
        } else {
            false
        }
    }

    fn reject_pin(&self, pin: &ServerPin) {
        let mut state = self.state();
        if state.pin.as_ref() == Some(pin) {
            state.generation = state.generation.wrapping_add(1);
            state.pin = None;
            state.healthy = false;
            state.managed_ids.clear();
            state.managed_ids_pin = None;
            state.managed_server_pin = None;
        }
    }

    fn pin_is_current(&self, pin: &ServerPin) -> bool {
        let state = self.state();
        state.generation == pin.generation && state.pin.as_ref() == Some(pin)
    }

    fn retire_pin(&self, pin: &ServerPin) {
        self.reject_pin(pin);
    }

    fn publish_managed_server(&self, pin: &ServerPin) {
        let mut state = self.state();
        if state.healthy && state.pin.as_ref() == Some(pin) && state.generation == pin.generation {
            state.managed_server_pin = Some(pin.clone());
        }
    }

    fn pin_allows_new_work(&self, pin: &ServerPin) -> bool {
        {
            let state = self.state();
            if state.healthy
                && state.pin.as_ref() == Some(pin)
                && state.managed_server_pin.as_ref() == Some(pin)
            {
                return true;
            }
        }
        if process_has_managed_server_marker(self.secure_config().ok(), pin) {
            self.publish_managed_server(pin);
            true
        } else {
            false
        }
    }

    fn managed_server_is_current(&self, pin: &ServerPin) -> bool {
        let state = self.state();
        state.healthy
            && state.generation == pin.generation
            && state.pin.as_ref() == Some(pin)
            && state.managed_server_pin.as_ref() == Some(pin)
    }

    fn ensure_secure_server(&self) -> std::result::Result<ServerPin, String> {
        match self.probe_secure()? {
            SecureProbe::Healthy(pin) if self.pin_allows_new_work(&pin) => return Ok(pin),
            SecureProbe::Healthy(_) | SecureProbe::Missing => {}
            SecureProbe::PinnedUnhealthy(error) if self.state().pin.is_some() => {
                return Err(error);
            }
            SecureProbe::PinnedUnhealthy(_) => {}
        }

        let _launch_lock = acquire_advisory_launch_lock(
            &self.secure_config()?.viewer_db_path,
            self.inner.startup_timeout,
        )?;
        match self.probe_secure()? {
            SecureProbe::Healthy(pin) if self.pin_allows_new_work(&pin) => return Ok(pin),
            SecureProbe::Healthy(pin) => {
                let skipped = pin.endpoint;
                self.retire_pin(&pin);
                if let Ok(SecureProbe::Healthy(alternative)) = self.discover_secure(Some(skipped)) {
                    if self.pin_allows_new_work(&alternative) {
                        return Ok(alternative);
                    }
                    self.retire_pin(&alternative);
                }
            }
            SecureProbe::PinnedUnhealthy(error) if self.state().pin.is_some() => {
                return Err(error);
            }
            SecureProbe::Missing | SecureProbe::PinnedUnhealthy(_) => {}
        }

        let credentials = self.credentials()?;
        let config_content = self.private_server_config_content()?;
        let deadline = Instant::now() + self.inner.startup_timeout;
        let mut failures = Vec::new();
        for candidate in self.inner.candidates.iter().copied() {
            if Instant::now() >= deadline {
                note_candidate_failure(
                    &mut failures,
                    candidate,
                    "startup deadline elapsed".to_string(),
                );
                break;
            }
            match TcpListener::bind(candidate) {
                Ok(listener) => drop(listener),
                Err(error) => {
                    match self.probe_secure_candidate(
                        candidate,
                        &credentials,
                        None,
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(self.health_timeout()),
                    ) {
                        CandidateProbe::Healthy(pin) if self.pin_allows_new_work(&pin) => {
                            return Ok(pin);
                        }
                        CandidateProbe::Healthy(pin) => self.retire_pin(&pin),
                        CandidateProbe::Missing(_)
                        | CandidateProbe::Unavailable(_)
                        | CandidateProbe::PinnedUnhealthy(_) => {}
                    }
                    note_candidate_failure(
                        &mut failures,
                        candidate,
                        format!("port is occupied ({error})"),
                    );
                    continue;
                }
            }
            let mut command = Command::new("opencode");
            command
                .arg("serve")
                .arg("--hostname")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(candidate.port().to_string())
                .current_dir(&self.inner.durable_cwd)
                .env("OPENCODE_SERVER_USERNAME", &credentials.username)
                .env("OPENCODE_SERVER_PASSWORD", &credentials.password)
                .env("AGENT_VIEWER_OPENCODE_SERVER", "1")
                .env("OPENCODE_CONFIG_CONTENT", &config_content);
            let launched_pid = match (self.inner.launcher)(command) {
                Ok(pid) => pid,
                Err(error) => {
                    let failure = credentials.sanitize(&error.to_string());
                    note_candidate_failure(&mut failures, candidate, failure);
                    if let Ok(SecureProbe::Healthy(pin)) = self.discover_secure(None) {
                        if self.pin_allows_new_work(&pin) {
                            return Ok(pin);
                        }
                        self.retire_pin(&pin);
                    }
                    continue;
                }
            };
            let mut last_error = "server did not become ready".to_string();
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.probe_secure_candidate(
                    candidate,
                    &credentials,
                    Some(launched_pid),
                    remaining.min(self.health_timeout()),
                ) {
                    CandidateProbe::Healthy(pin) => {
                        if self.pin_allows_new_work(&pin) {
                            return Ok(pin);
                        }
                        last_error =
                            "launched server process did not retain its managed marker".to_string();
                        self.retire_pin(&pin);
                    }
                    CandidateProbe::PinnedUnhealthy(error)
                    | CandidateProbe::Missing(error)
                    | CandidateProbe::Unavailable(error) => {
                        last_error = error;
                    }
                }
                std::thread::sleep(
                    READINESS_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            if let Ok(SecureProbe::Healthy(pin)) = self.discover_secure(None) {
                if self.pin_allows_new_work(&pin) {
                    return Ok(pin);
                }
                self.retire_pin(&pin);
            }
            note_candidate_failure(
                &mut failures,
                candidate,
                format!("readiness timed out: {last_error}"),
            );
            if let Some(pin) = self.state().pin.clone() {
                self.retire_pin(&pin);
            }
        }
        let failure = format_candidate_failures(&failures);
        if let Some(log_path) = self.secure_config()?.startup_log_path.as_ref() {
            Err(format!("{failure}; server log {}", log_path.display()))
        } else {
            Err(failure)
        }
    }

    fn replace_managed_ids(&self, pin: &ServerPin, ids: HashSet<String>) {
        let mut state = self.state();
        if state.healthy && state.generation == pin.generation && state.pin.as_ref() == Some(pin) {
            state.managed_ids = ids;
            state.managed_ids_pin = Some(pin.clone());
        }
    }

    fn managed_lease(&self, id: &str) -> Option<ManagedLease> {
        let state = self.state();
        let pin = state.pin.as_ref()?;
        if !state.healthy
            || state.managed_ids_pin.as_ref() != Some(pin)
            || !state.managed_ids.contains(id)
        {
            return None;
        }
        Some(ManagedLease {
            pin: pin.clone(),
            id: id.to_string(),
        })
    }

    fn managed_lease_is_current(&self, pin: &ServerPin, id: &str) -> bool {
        let state = self.state();
        state.healthy
            && state.generation == pin.generation
            && state.pin.as_ref() == Some(pin)
            && state.managed_ids_pin.as_ref() == Some(pin)
            && state.managed_ids.contains(id)
    }

    fn is_managed_id(&self, id: &str) -> bool {
        self.managed_lease(id).is_some()
    }

    fn private_server_config_content(&self) -> std::result::Result<String, String> {
        let state_dir = self
            .secure_config()?
            .viewer_db_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| "server credential state directory is unavailable".to_string())?;
        ensure_owner_only_directory(state_dir)?;
        let plugin_path =
            state_dir.join("shell.env_OPENCODE_SERVER_USERNAME_OPENCODE_SERVER_PASSWORD.js");
        write_private_plugin(&plugin_path)?;
        merge_opencode_config(
            std::env::var("OPENCODE_CONFIG_CONTENT").ok().as_deref(),
            &plugin_path,
        )
    }
}

impl Credentials {
    fn sanitize(&self, message: &str) -> String {
        message
            .replace(&self.password, "[redacted]")
            .replace(&self.authorization, "[redacted]")
    }
}

impl Drop for AdvisoryLockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        release_local_advisory_lock(&self.local);
    }
}

fn advisory_lock_entries() -> &'static Mutex<HashMap<PathBuf, Arc<AdvisoryLockEntry>>> {
    static ENTRIES: OnceLock<Mutex<HashMap<PathBuf, Arc<AdvisoryLockEntry>>>> = OnceLock::new();
    ENTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn release_local_advisory_lock(entry: &AdvisoryLockEntry) {
    let mut held = entry
        .held
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *held = false;
    entry.released.notify_one();
}

fn acquire_advisory_launch_lock(
    viewer_db_path: &Path,
    timeout: Duration,
) -> std::result::Result<AdvisoryLockGuard, String> {
    let state_dir = viewer_db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "server credential state directory is unavailable".to_string())?;
    ensure_owner_only_directory(state_dir)?;
    let lock_path = state_dir.join("opencode_server.lock");
    let deadline = Instant::now() + timeout;
    let local = {
        let mut entries = advisory_lock_entries()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(entries.entry(lock_path.clone()).or_insert_with(|| {
            Arc::new(AdvisoryLockEntry {
                held: Mutex::new(false),
                released: Condvar::new(),
            })
        }))
    };
    let mut held = local
        .held
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while *held {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("server credential state launch lock timed out".to_string());
        }
        let (next, timeout_result) = local
            .released
            .wait_timeout(held, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held = next;
        if timeout_result.timed_out() && *held {
            return Err("server credential state launch lock timed out".to_string());
        }
    }
    *held = true;
    drop(held);

    let result = (|| {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                format!("could not open server credential state launch lock: {error}")
            })?;
        if !file
            .metadata()
            .map_err(|error| {
                format!("could not inspect server credential state launch lock: {error}")
            })?
            .is_file()
        {
            return Err("server credential state launch lock is not a regular file".to_string());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                format!("could not secure server credential state launch lock: {error}")
            })?;

        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(file);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(format!(
                    "could not lock server credential state launch lock: {error}"
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("server credential state launch lock timed out".to_string());
            }
            std::thread::sleep(READINESS_POLL_INTERVAL.min(remaining));
        }
    })();

    match result {
        Ok(file) => Ok(AdvisoryLockGuard { local, file }),
        Err(error) => {
            release_local_advisory_lock(&local);
            Err(error)
        }
    }
}

fn ensure_owner_only_directory(path: &Path) -> std::result::Result<(), String> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(format!(
                    "server credential state path {} is not a directory",
                    current.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    format!(
                        "server credential state directory {} has no existing ancestor",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect server credential state directory {}: {error}",
                    current.display()
                ));
            }
        }
    }

    for directory in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) => {
            }
            Err(error) => {
                return Err(format!(
                    "could not create server credential state directory {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    Ok(())
}

fn write_private_plugin(path: &Path) -> std::result::Result<(), String> {
    const PLUGIN: &str = r#"export const AgentViewerShellEnvironment = async () => ({
  "shell.env": async (_input, output) => {
    output.env.OPENCODE_SERVER_USERNAME = ""
    output.env.OPENCODE_SERVER_PASSWORD = ""
  },
})
"#;
    let _write_guard = private_plugin_write_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            format!("could not write private OpenCode server credential state plugin: {error}")
        })?;
    if !file
        .metadata()
        .map_err(|error| {
            format!("could not inspect private OpenCode server credential state plugin: {error}")
        })?
        .is_file()
    {
        return Err(
            "private OpenCode server credential state plugin is not a regular file".to_string(),
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            format!("could not secure private OpenCode server credential state plugin: {error}")
        })?;
    file.write_all(PLUGIN.as_bytes()).map_err(|error| {
        format!("could not write private OpenCode server credential state plugin: {error}")
    })?;
    file.sync_all().map_err(|error| {
        format!("could not persist private OpenCode server credential state plugin: {error}")
    })
}

fn private_plugin_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn merge_opencode_config(
    existing: Option<&str>,
    plugin_path: &Path,
) -> std::result::Result<String, String> {
    let mut config = match existing {
        Some(content) => serde_json::from_str::<Value>(content)
            .map_err(|_| "OPENCODE_CONFIG_CONTENT is not valid JSON".to_string())?,
        None => json!({}),
    };
    let object = config
        .as_object_mut()
        .ok_or_else(|| "OPENCODE_CONFIG_CONTENT must contain a JSON object".to_string())?;
    let plugin_url = private_plugin_url(plugin_path);
    let plugins = object.entry("plugin").or_insert_with(|| json!([]));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| "OPENCODE_CONFIG_CONTENT plugin must be an array".to_string())?;
    if !plugins
        .iter()
        .any(|plugin| plugin.as_str() == Some(&plugin_url))
    {
        plugins.push(Value::String(plugin_url));
    }
    serde_json::to_string(&config)
        .map_err(|error| format!("could not serialize OpenCode server config: {error}"))
}

fn private_plugin_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match *byte {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'~' => {
                url.push(*byte as char);
            }
            byte => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

fn process_has_managed_server_marker(
    config: Option<&SecureRuntimeConfig>,
    pin: &ServerPin,
) -> bool {
    let Some(config) = config else {
        return false;
    };
    match fs::read(config.proc_root.join(pin.pid.to_string()).join("environ")) {
        Ok(environment) => environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"AGENT_VIEWER_OPENCODE_SERVER=1"),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && config.proc_root != Path::new("/proc") =>
        {
            true
        }
        Err(_) => false,
    }
}

impl Default for OpencodeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn connect_stream(
    endpoint: SocketAddr,
    timeout: Duration,
) -> std::result::Result<TcpStream, String> {
    let timeout = timeout.max(Duration::from_millis(1));
    let stream = TcpStream::connect_timeout(&endpoint, timeout)
        .map_err(|error| format!("could not connect to port {}: {error}", endpoint.port()))?;
    stream
        .set_read_timeout(Some(timeout.min(READINESS_POLL_INTERVAL)))
        .map_err(|error| format!("could not set response deadline: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not set request deadline: {error}"))?;
    Ok(stream)
}

fn remaining_timeout(deadline: Instant) -> std::result::Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("request deadline elapsed".to_string())
    } else {
        Ok(remaining)
    }
}

#[derive(Clone, Copy)]
enum HttpConnection {
    Close,
    KeepAlive,
}

#[derive(Clone, Copy)]
struct RequestParts<'a> {
    method: &'a str,
    target: &'a str,
    body: Option<&'a Value>,
}

fn write_and_read_request(
    stream: &mut TcpStream,
    endpoint: SocketAddr,
    request: RequestParts<'_>,
    authorization: Option<&str>,
    connection: HttpConnection,
    timeout: Duration,
) -> std::result::Result<HttpReply, String> {
    let RequestParts {
        method,
        target,
        body,
    } = request;
    if !target.starts_with('/') || target.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err("HTTP request target is invalid".to_string());
    }
    let body = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| format!("could not encode request JSON: {error}"))?;
    let connection = match connection {
        HttpConnection::Close => "close",
        HttpConnection::KeepAlive => "keep-alive",
    };
    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {endpoint}\r\nConnection: {connection}\r\n")
            .into_bytes();
    if let Some(authorization) = authorization {
        request.extend_from_slice(b"Authorization: ");
        request.extend_from_slice(authorization.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    if let Some(body) = &body {
        request.extend_from_slice(b"Content-Type: application/json\r\n");
        request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        request.extend_from_slice(&body);
    }
    stream
        .write_all(&request)
        .map_err(|error| format!("{method} {target} request failed: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("{method} {target} request failed: {error}"))?;
    read_http_reply(stream, method, target, timeout)
}

fn read_http_reply(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    timeout: Duration,
) -> std::result::Result<HttpReply, String> {
    let deadline = Instant::now() + timeout;
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = read_with_deadline(stream, &mut chunk, deadline)
            .map_err(|error| format!("{method} {target} response header unreadable: {error}"))?;
        if read == 0 {
            return Err(format!(
                "{method} {target} response ended before complete headers"
            ));
        }
        received.extend_from_slice(&chunk[..read]);
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if received.len() > MAX_RESPONSE_HEADER_BYTES {
            return Err(format!("{method} {target} response header is too large"));
        }
    };
    if header_end > MAX_RESPONSE_HEADER_BYTES {
        return Err(format!("{method} {target} response header is too large"));
    }
    let header = std::str::from_utf8(&received[..header_end])
        .map_err(|_| format!("{method} {target} response header is not valid text"))?;
    let mut lines = header[..header.len() - 4].split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| format!("{method} {target} response status is missing"))?;
    let mut status_parts = status_line.splitn(3, ' ');
    let version = status_parts.next().unwrap_or_default();
    let status = status_parts
        .next()
        .ok_or_else(|| format!("{method} {target} response status is malformed"))?
        .parse::<u16>()
        .map_err(|_| format!("{method} {target} response status is malformed"))?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(format!(
            "{method} {target} response HTTP version is unsupported"
        ));
    }
    if (300..400).contains(&status) {
        return Err(http_status_error(method, target, status));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("{method} {target} response header is malformed"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name.bytes().any(|byte| !byte.is_ascii_graphic()) {
            return Err(format!("{method} {target} response header is malformed"));
        }
        if headers.insert(name, value.trim().to_string()).is_some() {
            return Err(format!(
                "{method} {target} response has duplicate framing headers"
            ));
        }
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("{method} {target} response length is malformed"))
        })
        .transpose()?;
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    if headers.contains_key("transfer-encoding") && !chunked {
        return Err(format!(
            "{method} {target} response transfer encoding is unsupported"
        ));
    }
    if content_length.is_some() && chunked {
        return Err(format!("{method} {target} response framing is ambiguous"));
    }
    if content_length.is_none() && !chunked && status != 204 {
        return Err(format!("{method} {target} response has no framing"));
    }
    if content_length.is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES) {
        return Err(format!("{method} {target} response body is too large"));
    }

    let mut body_bytes = received.split_off(header_end);
    if let Some(length) = content_length {
        while body_bytes.len() < length {
            let read = read_with_deadline(stream, &mut chunk, deadline)
                .map_err(|error| format!("{method} {target} response body unreadable: {error}"))?;
            if read == 0 {
                return Err(format!("{method} {target} response body is incomplete"));
            }
            body_bytes.extend_from_slice(&chunk[..read]);
            if body_bytes.len() > MAX_RESPONSE_BODY_BYTES {
                return Err(format!("{method} {target} response body is too large"));
            }
        }
        if body_bytes.len() != length {
            return Err(format!(
                "{method} {target} response body exceeds its declared length"
            ));
        }
        return Ok(HttpReply {
            status,
            headers,
            body: body_bytes,
        });
    }
    if status == 204 && !chunked {
        while let Ok(read) = stream.read(&mut chunk) {
            if read == 0 {
                break;
            }
            body_bytes.extend_from_slice(&chunk[..read]);
            if !body_bytes.is_empty() {
                return Err(format!(
                    "{method} {target} bodyless response carried a body"
                ));
            }
        }
        return Ok(HttpReply {
            status,
            headers,
            body: Vec::new(),
        });
    }
    loop {
        match decode_chunked(&body_bytes, method, target)? {
            Some(body) => {
                return Ok(HttpReply {
                    status,
                    headers,
                    body,
                });
            }
            None => {
                let read = read_with_deadline(stream, &mut chunk, deadline).map_err(|error| {
                    format!("{method} {target} chunked response unreadable: {error}")
                })?;
                if read == 0 {
                    return Err(format!("{method} {target} chunked response is incomplete"));
                }
                body_bytes.extend_from_slice(&chunk[..read]);
                if body_bytes.len() > MAX_RESPONSE_BODY_BYTES + MAX_RESPONSE_HEADER_BYTES {
                    return Err(format!("{method} {target} response body is too large"));
                }
            }
        }
    }
}

fn read_with_deadline(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<usize> {
    loop {
        match stream.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}

fn decode_chunked(
    encoded: &[u8],
    method: &str,
    target: &str,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let mut offset = 0;
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = encoded[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|position| offset + position)
        else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&encoded[offset..line_end])
            .map_err(|_| format!("{method} {target} chunk size is malformed"))?;
        let size_text = size_text.split(';').next().unwrap_or_default();
        if size_text.is_empty() {
            return Err(format!("{method} {target} chunk size is malformed"));
        }
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("{method} {target} chunk size is malformed"))?;
        offset = line_end + 2;
        if size == 0 {
            if encoded.len() < offset + 2 {
                return Ok(None);
            }
            if &encoded[offset..offset + 2] != b"\r\n" {
                return Err(format!("{method} {target} chunk trailer is malformed"));
            }
            return Ok(Some(decoded));
        }
        if decoded.len().saturating_add(size) > MAX_RESPONSE_BODY_BYTES {
            return Err(format!("{method} {target} response body is too large"));
        }
        if encoded.len() < offset.saturating_add(size).saturating_add(2) {
            return Ok(None);
        }
        decoded.extend_from_slice(&encoded[offset..offset + size]);
        offset += size;
        if &encoded[offset..offset + 2] != b"\r\n" {
            return Err(format!("{method} {target} chunk framing is malformed"));
        }
        offset += 2;
    }
}

#[derive(Clone, Copy)]
struct TcpRow {
    local: SocketAddr,
    remote: SocketAddr,
    state: u8,
    inode: u64,
}

fn connected_identity(
    stream: &TcpStream,
    endpoint: SocketAddr,
    proc_root: &Path,
    expected_pid: Option<u32>,
    timeout: Duration,
) -> std::result::Result<ProcessIdentity, String> {
    let peer = stream
        .peer_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    if peer != endpoint {
        return Err("connected socket endpoint changed".to_string());
    }
    let deadline = Instant::now() + timeout.min(Duration::from_millis(150));
    loop {
        match resolve_connected_identity(proc_root, endpoint, expected_pid) {
            Ok(identity) => return Ok(identity),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn accepted_connection_matches(
    proc_root: &Path,
    stream: &TcpStream,
    identity: &ProcessIdentity,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let local = stream
        .local_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    let peer = stream
        .peer_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    if peer != identity.endpoint {
        return Err("connected socket endpoint changed".to_string());
    }
    let deadline = Instant::now() + timeout;
    loop {
        match resolve_accepted_connection(proc_root, identity, local) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn resolve_accepted_connection(
    proc_root: &Path,
    identity: &ProcessIdentity,
    client_endpoint: SocketAddr,
) -> std::result::Result<(), String> {
    let rows = read_tcp_rows(proc_root)?;
    let mut inodes = rows
        .iter()
        .filter(|row| {
            row.state == 0x01
                && row.local == identity.endpoint
                && row.remote == client_endpoint
                && row.inode != 0
        })
        .map(|row| row.inode)
        .collect::<Vec<_>>();
    inodes.sort_unstable();
    inodes.dedup();
    if inodes.len() != 1 {
        return Err("accepted connection identity is unavailable".to_string());
    }
    let owners = pids_owning_inode(proc_root, inodes[0]);
    if owners.len() != 1 || owners[0] != identity.pid {
        return Err("accepted connection is not owned by the pinned process".to_string());
    }
    let current = resolve_connected_identity(proc_root, identity.endpoint, Some(identity.pid))?;
    if process_identities_match(identity, &current) {
        Ok(())
    } else {
        Err("listener identity changed while connection was open".to_string())
    }
}

fn resolve_connected_identity(
    proc_root: &Path,
    endpoint: SocketAddr,
    expected_pid: Option<u32>,
) -> std::result::Result<ProcessIdentity, String> {
    let rows = read_tcp_rows(proc_root)?;
    let listener_inode = rows
        .iter()
        .find(|row| row.state == 0x0a && row.local == endpoint)
        .map(|row| row.inode)
        .ok_or_else(|| "listener identity is unavailable".to_string())?;
    let owners = pids_owning_inode(proc_root, listener_inode);
    if owners.len() != 1 {
        return Err("listener does not have one process owner".to_string());
    }
    let pid = owners[0];
    if expected_pid.is_some_and(|expected| expected != pid) {
        return Err("listener is not owned by the launched process".to_string());
    }
    read_process_identity(proc_root, pid, listener_inode, endpoint)
}

fn identity_still_matches(proc_root: &Path, pin: &ServerPin) -> bool {
    let Ok(rows) = read_tcp_rows(proc_root) else {
        return false;
    };
    let Some(listener_inode) = rows
        .iter()
        .find(|row| row.state == 0x0a && row.local == pin.endpoint)
        .map(|row| row.inode)
    else {
        return false;
    };
    if listener_inode != pin.listener_inode
        || !process_owns_inode(proc_root, pin.pid, listener_inode)
    {
        return false;
    }
    read_process_identity(proc_root, pin.pid, listener_inode, pin.endpoint)
        .is_ok_and(|identity| pin_matches_identity(pin, &identity))
}

fn pin_matches_identity(pin: &ServerPin, identity: &ProcessIdentity) -> bool {
    pin.pid == identity.pid
        && pin.start_time == identity.start_time
        && pin.listener_inode == identity.listener_inode
        && pin.effective_uid == identity.effective_uid
        && pin.argv == identity.argv
        && pin.endpoint == identity.endpoint
}

fn process_identities_match(left: &ProcessIdentity, right: &ProcessIdentity) -> bool {
    left.pid == right.pid
        && left.start_time == right.start_time
        && left.listener_inode == right.listener_inode
        && left.effective_uid == right.effective_uid
        && left.argv == right.argv
        && left.endpoint == right.endpoint
}

fn read_process_identity(
    proc_root: &Path,
    pid: u32,
    listener_inode: u64,
    endpoint: SocketAddr,
) -> std::result::Result<ProcessIdentity, String> {
    let process = proc_root.join(pid.to_string());
    let effective_uid = fs::read_to_string(process.join("status"))
        .map_err(|_| "process status is unreadable".to_string())?
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")
                .and_then(|value| value.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u32>().ok())
        })
        .ok_or_else(|| "process effective uid is unavailable".to_string())?;
    if effective_uid != unsafe { libc::geteuid() } {
        return Err("listener process belongs to another user".to_string());
    }
    let stat = fs::read_to_string(process.join("stat"))
        .map_err(|_| "process start time is unreadable".to_string())?;
    let after_name = stat
        .rsplit_once(") ")
        .map(|(_, rest)| rest)
        .ok_or_else(|| "process stat is malformed".to_string())?;
    let start_time = after_name
        .split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "process start time is malformed".to_string())?;
    let cmdline = fs::read(process.join("cmdline"))
        .map_err(|_| "process command line is unreadable".to_string())?;
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| OsString::from(String::from_utf8_lossy(part).into_owned()))
        .collect::<Vec<_>>();
    if !valid_server_argv(&argv, endpoint.port()) {
        return Err("listener process command line does not match OpenCode serve".to_string());
    }
    Ok(ProcessIdentity {
        pid,
        start_time,
        listener_inode,
        effective_uid,
        argv,
        endpoint,
    })
}

fn valid_server_argv(argv: &[OsString], port: u16) -> bool {
    argv.len() == 6
        && Path::new(&argv[0])
            .file_name()
            .is_some_and(|name| name == "opencode")
        && argv[1] == "serve"
        && argv[2] == "--hostname"
        && argv[3] == "127.0.0.1"
        && argv[4] == "--port"
        && argv[5].to_string_lossy() == port.to_string()
}

fn pids_owning_inode(proc_root: &Path, inode: u64) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| process_owns_inode(proc_root, *pid, inode))
        .collect()
}

fn process_owns_inode(proc_root: &Path, pid: u32, inode: u64) -> bool {
    let Ok(entries) = fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else {
        return false;
    };
    let expected = format!("socket:[{inode}]");
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .any(|target| target == Path::new(&expected))
}

fn read_tcp_rows(proc_root: &Path) -> std::result::Result<Vec<TcpRow>, String> {
    let mut rows = Vec::new();
    for name in ["tcp", "tcp6"] {
        let path = proc_root.join("net").join(name);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        for line in content.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 10 {
                continue;
            }
            let Some(local) = parse_proc_socket(fields[1]) else {
                continue;
            };
            let Some(remote) = parse_proc_socket(fields[2]) else {
                continue;
            };
            let Ok(state) = u8::from_str_radix(fields[3], 16) else {
                continue;
            };
            let Ok(inode) = fields[9].parse::<u64>() else {
                continue;
            };
            rows.push(TcpRow {
                local,
                remote,
                state,
                inode,
            });
        }
    }
    Ok(rows)
}

fn parse_proc_socket(value: &str) -> Option<SocketAddr> {
    let (address, port) = value.split_once(':')?;
    if address.len() != 8 {
        return None;
    }
    let raw = u32::from_str_radix(address, 16).ok()?;
    let octets = raw.to_le_bytes();
    let port = u16::from_str_radix(port, 16).ok()?;
    Some(SocketAddr::new(Ipv4Addr::from(octets).into(), port))
}

pub struct OpencodeBackend {
    db_path: PathBuf,
    runtime: OpencodeRuntime,
    models_cache: OnceLock<Vec<String>>,
}

impl OpencodeBackend {
    pub fn new() -> Self {
        Self::with_runtime(OpencodeRuntime::new())
    }

    pub fn with_runtime(runtime: OpencodeRuntime) -> Self {
        Self::with_db_and_runtime(default_opencode_db(), runtime)
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self::with_db_and_runtime(db_path, OpencodeRuntime::compatibility_only())
    }

    pub fn with_db_and_runtime(db_path: PathBuf, runtime: OpencodeRuntime) -> Self {
        Self {
            db_path,
            runtime,
            models_cache: OnceLock::new(),
        }
    }

    fn list_from_sqlite(&self) -> Result<Vec<Session>> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = crate::open_readonly(&self.db_path)?;
        let live = live_opencode_proc();
        let now = crate::spawn::now_ms();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, directory, title, time_created, time_updated, time_archived, \
             permission FROM session ORDER BY time_updated ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let parent_id: Option<String> = row.get(1)?;
            let time_updated: i64 = row.get(5)?;
            let time_archived: Option<i64> = row.get(6)?;
            let permission: Option<String> = row.get(7)?;
            Ok(Session {
                backend: BackendKind::Opencode,
                id: row.get(0)?,
                short_id: None,
                origin: SessionOrigin::Interactive,
                title: row.get(3)?,
                cwd: PathBuf::from(row.get::<_, String>(2)?),
                git_branch: None,
                status: opencode_status(live, time_updated, now),
                created_at_ms: row.get(4)?,
                updated_at_ms: time_updated,
                hidden: time_archived.is_some(),
                companion: parent_id.is_some() || is_run_mode_permission(permission.as_deref()),
                summary: String::new(),
                pid: None,
                rollout_path: None,
                pr_refs: Vec::new(),
                daemon_hosted: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn list_from_server(&self, pin: &ServerPin) -> Result<Vec<Session>> {
        let endpoint = pin.endpoint;
        let mut sessions = Vec::new();
        let mut cursor = None::<String>;
        let mut seen_cursors = HashSet::new();
        loop {
            let target = cursor.as_ref().map_or_else(
                || format!("/experimental/session?limit={SESSION_PAGE_SIZE}&archived=true"),
                |cursor| {
                    format!(
                        "/experimental/session?limit={SESSION_PAGE_SIZE}&archived=true&cursor={}",
                        percent_encode(cursor.as_bytes())
                    )
                },
            );
            let reply = self
                .runtime
                .request(endpoint, "GET", &target, None, HTTP_REQUEST_TIMEOUT)
                .map_err(Error::Command)?;
            if reply.status != 200 {
                return Err(Error::Command(http_status_error(
                    "GET",
                    &target,
                    reply.status,
                )));
            }
            let mut page: Vec<ServerSession> =
                serde_json::from_slice(&reply.body).map_err(|error| {
                    Error::Command(format!("GET {target} returned invalid JSON: {error}"))
                })?;
            if page.len() > SESSION_PAGE_SIZE
                || sessions.len().saturating_add(page.len()) > MAX_SESSION_ROWS
            {
                return Err(Error::Command(
                    "OpenCode session page or aggregate is too large".to_string(),
                ));
            }
            let page_is_full = page.len() == SESSION_PAGE_SIZE;
            sessions.append(&mut page);
            let next = reply.headers.get("x-next-cursor").cloned();
            match next {
                Some(next) => {
                    if next.is_empty()
                        || !next
                            .bytes()
                            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
                    {
                        return Err(Error::Command(
                            "OpenCode session cursor is malformed".to_string(),
                        ));
                    }
                    if !seen_cursors.insert(next.clone()) {
                        return Err(Error::Command(
                            "OpenCode session cursor repeated".to_string(),
                        ));
                    }
                    cursor = Some(next);
                }
                None if page_is_full => {
                    return Err(Error::Command(
                        "OpenCode full session page omitted its next cursor".to_string(),
                    ));
                }
                None => break,
            }
        }

        let managed_ids = sessions
            .iter()
            .filter(|session| session.is_managed())
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        let directories = sessions
            .iter()
            .filter(|session| session.is_managed() && !session.is_archived())
            .map(|session| session.directory.as_str())
            .collect::<BTreeSet<_>>();
        let mut statuses = HashMap::new();
        let mut input_reasons = HashMap::new();
        let mut failed_directories = HashSet::new();
        for directory in directories {
            let encoded = percent_encode(directory.as_bytes());
            let scoped_status = self.runtime.request_json::<HashMap<String, ServerStatus>>(
                endpoint,
                "GET",
                &format!("/session/status?directory={encoded}"),
                None,
                200,
            );
            let scoped_permissions = self.runtime.request_json::<Vec<PermissionRequest>>(
                endpoint,
                "GET",
                &format!("/permission?directory={encoded}"),
                None,
                200,
            );
            let scoped_questions = self.runtime.request_json::<Vec<QuestionRequest>>(
                endpoint,
                "GET",
                &format!("/question?directory={encoded}"),
                None,
                200,
            );
            match (scoped_status, scoped_permissions, scoped_questions) {
                (Ok(scoped_status), Ok(permissions), Ok(questions)) => {
                    statuses.extend(scoped_status);
                    for permission in permissions {
                        let reason = permission
                            .patterns
                            .first()
                            .map(|pattern| format!("{}: {pattern}", permission.permission))
                            .unwrap_or(permission.permission);
                        input_reasons.insert(permission.session_id, reason);
                    }
                    for question in questions {
                        if let Some(reason) = question
                            .questions
                            .first()
                            .map(|question| question.question.clone())
                        {
                            input_reasons.insert(question.session_id, reason);
                        }
                    }
                }
                _ => {
                    failed_directories.insert(directory.to_string());
                }
            }
        }
        self.runtime.replace_managed_ids(pin, managed_ids);

        let mut listed = sessions
            .into_iter()
            .map(|session| {
                let managed = session.is_managed();
                let archived = session.is_archived();
                let status = if failed_directories.contains(&session.directory) {
                    Status::Unknown
                } else if managed && !archived {
                    input_reasons
                        .get(&session.id)
                        .cloned()
                        .map(|reason| Status::NeedsInput {
                            reason: Some(reason),
                        })
                        .unwrap_or_else(|| {
                            match statuses.get(&session.id).map(|status| status.kind.as_str()) {
                                Some("busy" | "retry") => Status::Working,
                                _ => Status::Idle,
                            }
                        })
                } else {
                    Status::Idle
                };
                Session {
                    backend: BackendKind::Opencode,
                    id: session.id,
                    short_id: None,
                    origin: SessionOrigin::Interactive,
                    title: session.title,
                    cwd: PathBuf::from(session.directory),
                    git_branch: None,
                    status,
                    created_at_ms: session.time.created,
                    updated_at_ms: session.time.updated,
                    hidden: archived,
                    companion: session.parent_id.is_some()
                        || session
                            .permission
                            .as_ref()
                            .is_some_and(server_permission_is_run_mode),
                    summary: String::new(),
                    pid: None,
                    rollout_path: None,
                    pr_refs: Vec::new(),
                    daemon_hosted: managed,
                }
            })
            .collect::<Vec<_>>();
        listed.sort_by_key(|session| session.updated_at_ms);
        Ok(listed)
    }
}

impl Default for OpencodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for OpencodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Opencode
    }

    fn listing_scope(&self) -> Option<crate::backend::ListingCacheScope> {
        let mode = if self.runtime.inner.secure.is_some() {
            "secure"
        } else {
            "compatibility"
        };
        let key = crate::backend::listing_scope_key(&[
            crate::backend::listing_scope_path(&self.db_path),
            self.runtime.inner.candidates[0].to_string(),
            self.runtime.inner.candidates[1].to_string(),
            mode.to_string(),
        ]);
        crate::backend::ListingCacheScope::new(self.kind(), key).ok()
    }

    fn capabilities(&self) -> Capabilities {
        if self.runtime.healthy_tier() {
            server_capabilities()
        } else {
            compatibility_capabilities()
        }
    }

    fn capabilities_for(&self, session: &Session) -> Capabilities {
        if session.daemon_hosted {
            if self.runtime.is_managed_id(&session.id) && self.runtime.healthy_tier() {
                Capabilities {
                    attach: false,
                    ..server_capabilities()
                }
            } else {
                Capabilities {
                    attach: false,
                    delete: false,
                    ..compatibility_capabilities()
                }
            }
        } else {
            compatibility_capabilities()
        }
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        match self.runtime.server_for_listing()? {
            Some(pin) => self.list_from_server(&pin),
            None => self.list_from_sqlite(),
        }
    }

    fn turn_activity(&self, session: &Session, window: Duration) -> Result<Vec<i64>> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = crate::open_readonly(&self.db_path)?;
        let (cutoff, now) = crate::activity_window(window);
        let mut stmt = conn.prepare(
            "WITH RECURSIVE descendants(id) AS ( \
                 SELECT id FROM session WHERE id = ?1 \
                 UNION \
                 SELECT child.id FROM session AS child \
                 JOIN descendants AS parent ON child.parent_id = parent.id \
             ) \
             SELECT message.time_created, message.data FROM message \
             JOIN descendants ON message.session_id = descendants.id \
             WHERE typeof(message.time_created) = 'integer' \
             AND message.time_created >= ?2 AND message.time_created <= ?3 \
             ORDER BY message.time_created ASC, message.id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session.id, cutoff, now], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut timestamps = Vec::new();
        for row in rows {
            let (timestamp, data) = row?;
            let Some(value) = crate::parse_json_line(&data) else {
                continue;
            };
            if matches!(
                crate::json_str(&value, "role"),
                Some("user") | Some("assistant")
            ) {
                timestamps.push(timestamp);
            }
        }
        Ok(timestamps)
    }

    fn available_models(&self) -> Vec<String> {
        self.models_cache
            .get_or_init(|| {
                let mut models = vec![self.kind().default_model().to_string()];
                models.extend(opencode_models_via_cli());
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }

    fn spawn(&self, dir: &Path, task: &str, model: Option<&str>) -> Result<crate::SpawnResult> {
        if self.runtime.inner.secure.is_none() {
            let title = crate::spawn::truncated_title(task);
            let mut command = Command::new("opencode");
            command
                .arg("run")
                .arg("--dir")
                .arg(dir)
                .arg("--title")
                .arg(&title);
            if let Some(model) = model {
                command.arg("-m").arg(model);
            }
            command.arg(task);
            let log_path = crate::spawn::viewer_log_path("opencode");
            let pid = crate::spawn::spawn_detached(command, &log_path)?;
            return Ok(crate::SpawnResult {
                pid: Some(pid),
                session_id: None,
            });
        }
        let model = selected_model(model)?;
        let pin = self.runtime.ensure_server().map_err(Error::Command)?;
        let directory = percent_encode(dir.as_os_str().as_bytes());
        let mut create = json!({
            "title": crate::spawn::truncated_title(task),
            "permission": managed_permission()
        });
        if let Some(model) = &model {
            create["model"] = json!({"providerID": model.provider, "id": model.id});
        }
        let create_target = format!("/session?directory={directory}");
        let created: CreatedSession = self.runtime.managed_server_request_json(
            &pin,
            "POST",
            &create_target,
            Some(&create),
            200,
        )?;
        if created.id.is_empty() {
            return Err(Error::Command(
                "POST /session returned an empty session id".to_string(),
            ));
        }

        let mut prompt = json!({"parts": [{"type": "text", "text": task}]});
        if let Some(model) = &model {
            prompt["model"] = json!({"providerID": model.provider, "modelID": model.id});
        }
        let prompt_target = format!(
            "/session/{}/prompt_async?directory={directory}",
            percent_encode(created.id.as_bytes())
        );
        if let Err(error) = self.runtime.managed_server_expect_status(
            &pin,
            "POST",
            &prompt_target,
            Some(&prompt),
            204,
        ) {
            return Err(Error::Command(format!(
                "session {} was created but prompt acceptance failed: {error}",
                created.id
            )));
        }
        Ok(crate::SpawnResult {
            pid: None,
            session_id: Some(created.id),
        })
    }

    fn stop(&self, session: &Session) -> Result<()> {
        if !session.daemon_hosted {
            return Err(Error::Unsupported(self.kind().name()));
        }
        let lease = self
            .runtime
            .managed_lease(&session.id)
            .ok_or_else(|| Error::Unsupported(self.kind().name()))?;
        let target = format!("/session/{}/abort", percent_encode(session.id.as_bytes()));
        let _: bool = self
            .runtime
            .managed_request_json(&lease, "POST", &target, None, 200)?;
        Ok(())
    }

    fn remove(&self, session: &Session) -> Result<()> {
        if session.daemon_hosted {
            let lease = self
                .runtime
                .managed_lease(&session.id)
                .ok_or_else(|| Error::Unsupported(self.kind().name()))?;
            let target = format!("/session/{}", percent_encode(session.id.as_bytes()));
            let _: Value = self
                .runtime
                .managed_request_json(&lease, "DELETE", &target, None, 200)?;
            return Ok(());
        }
        crate::spawn::run_checked(
            Command::new("opencode")
                .arg("session")
                .arg("delete")
                .arg(&session.id),
        )
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        if !session.daemon_hosted {
            return Err(Error::Unsupported(self.kind().name()));
        }
        let lease = self
            .runtime
            .managed_lease(&session.id)
            .ok_or_else(|| Error::Unsupported(self.kind().name()))?;
        let target = format!("/session/{}", percent_encode(session.id.as_bytes()));
        let body = json!({"title": name});
        self.runtime
            .managed_expect_status(&lease, "PATCH", &target, Some(&body), 200)
    }

    fn hide(&self, id: &str) -> Result<()> {
        let lease = self
            .runtime
            .managed_lease(id)
            .ok_or_else(|| Error::Unsupported(self.kind().name()))?;
        let target = format!("/session/{}", percent_encode(id.as_bytes()));
        let body = json!({"time": {"archived": crate::spawn::now_ms()}});
        self.runtime
            .managed_expect_status(&lease, "PATCH", &target, Some(&body), 200)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        let lease = self
            .runtime
            .managed_lease(id)
            .ok_or_else(|| Error::Unsupported(self.kind().name()))?;
        let target = format!("/session/{}", percent_encode(id.as_bytes()));
        let body = json!({"time": {"archived": 0}});
        self.runtime
            .managed_expect_status(&lease, "PATCH", &target, Some(&body), 200)
    }

    fn attach_command(&self, session: &Session) -> std::result::Result<Command, AttachRefusal> {
        let mut command = Command::new("opencode");
        if session.daemon_hosted {
            return Err(AttachRefusal::new(
                "managed OpenCode sessions cannot be attached safely",
            ));
        }
        command.arg("-s").arg(&session.id);
        if session.cwd.is_dir() {
            command.current_dir(&session.cwd);
        }
        Ok(command)
    }
}

#[derive(Deserialize)]
struct ServerSession {
    id: String,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    directory: String,
    title: String,
    #[serde(default)]
    permission: Option<Value>,
    time: ServerSessionTime,
}

impl ServerSession {
    fn is_managed(&self) -> bool {
        self.permission
            .as_ref()
            .is_some_and(server_permission_is_managed)
    }

    fn is_archived(&self) -> bool {
        self.time.archived.unwrap_or(0.0) > 0.0
    }
}

#[derive(Deserialize)]
struct ServerSessionTime {
    created: i64,
    updated: i64,
    #[serde(default)]
    archived: Option<f64>,
}

#[derive(Deserialize)]
struct ServerStatus {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct PermissionRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    permission: String,
    #[serde(default)]
    patterns: Vec<String>,
}

#[derive(Deserialize)]
struct QuestionRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    question: String,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

struct SelectedModel {
    provider: String,
    id: String,
}

fn selected_model(model: Option<&str>) -> Result<Option<SelectedModel>> {
    let Some(model) = model.filter(|model| *model != "default") else {
        return Ok(None);
    };
    let Some((provider, id)) = model.split_once('/') else {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    };
    if provider.is_empty() || id.is_empty() {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    }
    Ok(Some(SelectedModel {
        provider: provider.to_string(),
        id: id.to_string(),
    }))
}

fn server_capabilities() -> Capabilities {
    Capabilities {
        spawn: true,
        attach: true,
        rename: true,
        archive: true,
        delete: true,
        stop: true,
        needs_input: true,
        pr_refs: false,
        live_status: true,
    }
}

fn compatibility_capabilities() -> Capabilities {
    Capabilities {
        spawn: true,
        attach: true,
        rename: false,
        archive: false,
        delete: true,
        stop: false,
        needs_input: false,
        pr_refs: false,
        live_status: false,
    }
}

fn note_candidate_failure(
    failures: &mut Vec<(SocketAddr, String)>,
    candidate: SocketAddr,
    reason: String,
) {
    if let Some(entry) = failures
        .iter_mut()
        .find(|(address, _)| *address == candidate)
    {
        entry.1 = reason;
    } else {
        failures.push((candidate, reason));
    }
}

fn format_candidate_failures(failures: &[(SocketAddr, String)]) -> String {
    failures
        .iter()
        .map(|(candidate, reason)| format!("port {}: {reason}", candidate.port()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn http_status_error(method: &str, target: &str, status: u16) -> String {
    format!("{method} {target} returned HTTP {status}")
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn managed_permission() -> Value {
    json!([
        {"permission": "agent-viewer.background", "pattern": "*", "action": "allow"}
    ])
}

fn server_permission_is_managed(permission: &Value) -> bool {
    let Value::Array(entries) = permission else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(Value::as_str) == Some("agent-viewer.background")
            && entry.get("pattern").and_then(Value::as_str) == Some("*")
            && entry.get("action").and_then(Value::as_str) == Some("allow")
    })
}

fn server_permission_is_run_mode(permission: &Value) -> bool {
    let Value::Array(entries) = permission else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(Value::as_str) == Some("question")
            && entry.get("action").and_then(Value::as_str) == Some("deny")
    })
}

/// Default opencode DB path (mirrors OpencodeBackend::new()).
pub fn default_opencode_db() -> std::path::PathBuf {
    crate::home_dir().join(".local/share/opencode/opencode.db")
}

/// Run `opencode models` and parse stdout. Any failure (spawn error, non-zero exit) is a
/// quiet empty Vec — discovery is best-effort.
fn opencode_models_via_cli() -> Vec<String> {
    let mut cmd = std::process::Command::new("opencode");
    cmd.arg("models");
    match crate::spawn::run_with_timeout(cmd, crate::spawn::MODEL_PROBE_TIMEOUT) {
        Some(stdout) => parse_opencode_models(&stdout),
        None => Vec::new(),
    }
}

/// PURE: each non-empty trimmed line of `opencode models` stdout is a model id
/// (format `provider/model`). Blank/whitespace-only lines are dropped.
pub fn parse_opencode_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// The most recent opencode message that has text, as a TranscriptItem (role + concatenated
/// text parts). Reads the `message`/`part` tables read-only. Ok(None) when the DB is missing
/// or the session has no text message. Never creates/writes the DB.
pub fn read_opencode_last_message(
    db_path: &std::path::Path,
    session_id: &str,
) -> Result<Option<crate::codex::rollout::TranscriptItem>> {
    use crate::codex::rollout::TranscriptItem;
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = crate::open_readonly(db_path)?;
    // Rows arrive grouped by message (most-recent message first), parts in creation order.
    let mut stmt = conn.prepare(
        "SELECT m.id, m.data, p.data FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY m.time_created DESC, m.id DESC, p.time_created ASC",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        let mid: String = row.get(0)?;
        let mdata: String = row.get(1)?;
        let pdata: String = row.get(2)?;
        Ok((mid, mdata, pdata))
    })?;

    // Walk the grouped rows, accumulating each message's text parts. Return the first
    // (most-recent) message whose accumulated text is non-empty.
    let mut cur_id: Option<String> = None;
    let mut cur_role = String::from("assistant");
    let mut cur_text = String::new();
    for row in rows {
        let (mid, mdata, pdata) = row?;
        if cur_id.as_deref() != Some(mid.as_str()) {
            // Boundary: the previous message is complete — return it if it had non-blank text
            // (a whitespace-only message, e.g. newline-only parts around tool transitions,
            // would otherwise hide the real prior message behind blank lines). Return the
            // original untrimmed text so the real message's internal formatting is preserved.
            if cur_id.is_some() && !cur_text.trim().is_empty() {
                return Ok(Some(TranscriptItem {
                    role: cur_role,
                    text: cur_text,
                }));
            }
            cur_id = Some(mid);
            cur_role = parsed_role(&mdata);
            cur_text = String::new();
        }
        if let Some(text) = parsed_text_part(&pdata) {
            cur_text.push_str(&text);
        }
    }
    // The final message (no boundary follows it).
    if cur_id.is_some() && !cur_text.trim().is_empty() {
        return Ok(Some(TranscriptItem {
            role: cur_role,
            text: cur_text,
        }));
    }
    Ok(None)
}

/// message.data JSON "role" (defaulting to "assistant" when absent/unparseable).
fn parsed_role(mdata: &str) -> String {
    serde_json::from_str::<serde_json::Value>(mdata)
        .ok()
        .as_ref()
        .and_then(|v| crate::json_str(v, "role").map(String::from))
        .unwrap_or_else(|| "assistant".to_string())
}

/// part.data JSON text, only for `{"type":"text","text":...}` parts (else None so
/// tool/step-start/step-finish parts are skipped). Returns an owned String.
fn parsed_text_part(pdata: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(pdata).ok()?;
    if crate::json_str(&value, "type") != Some("text") {
        return None;
    }
    crate::json_str(&value, "text").map(String::from)
}

/// IMPURE process check (live-verified only): does any process named `opencode*` exist.
/// All opencode sessions share one process, so this is a single best-effort signal.
fn live_opencode_proc() -> bool {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.processes()
        .values()
        .any(|p| p.name().to_string_lossy().starts_with("opencode"))
}

/// Recency thresholds for the status heuristic below.
const WORKING_MAX_AGE_MS: i64 = 60_000; // 1 min
const IDLE_MAX_AGE_MS: i64 = 1_800_000; // 30 min

/// PURE three-tier status heuristic, unit-tested (the process check is injected, I-3):
/// Working: live && age <= 1 min; Idle: live && age <= 30 min; Done: otherwise.
/// Never NeedsInput or Error because the session table has no error column.
pub fn opencode_status(live_opencode_proc: bool, updated_at_ms: i64, now_ms: i64) -> Status {
    if !live_opencode_proc {
        return Status::Done;
    }
    let age = now_ms - updated_at_ms;
    if age <= WORKING_MAX_AGE_MS {
        Status::Working
    } else if age <= IDLE_MAX_AGE_MS {
        Status::Idle
    } else {
        Status::Done
    }
}

/// Companion filter for one-shot `opencode run` sessions — opencode's equivalent of the
/// codex `exec`/`subagent` rule in `codex::Source::is_companion`. A run started by a script
/// (an `/implement` review pass, a CI job) is not a fleet member, but it carries no
/// `parent_id`, so `parent_id` alone leaves it in the default list.
///
/// The signal is the `session.permission` column. `opencode run` denies the interactive
/// `question` tool (plus `plan_enter`/`plan_exit`) when it creates the session; the TUI
/// writes no session override at all. Verified live on this box 2026-07-26, both on
/// opencode 1.17.20: `opencode run --title ...` stored
/// `[{"permission":"question","pattern":"*","action":"deny"},{plan_enter deny},{plan_exit
/// deny}]`, a TUI session driven through a pty stored NULL.
///
/// Matched semantically (a denied `question` entry anywhere in the array), not by string
/// equality: the stored key order is not the source order, and the github-action path
/// writes the `question` deny without the plan pair. Never panics — anything that is not
/// a JSON array of objects is treated as interactive, the safe direction (a shown row a
/// keypress away from hidden beats a hidden row you cannot find).
pub fn is_run_mode_permission(permission: Option<&str>) -> bool {
    let Some(raw) = permission.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return false;
    };
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(raw) else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(|p| p.as_str()) == Some("question")
            && entry.get("action").and_then(|a| a.as_str()) == Some("deny")
    })
}

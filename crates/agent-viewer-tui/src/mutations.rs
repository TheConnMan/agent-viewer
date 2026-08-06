//! Background runners for backend work that must not block the render loop. Each operation is
//! a self contained closure returning a structured outcome; results are drained without blocking
//! through `poll()`. In flight keys deduplicate repeated submissions while work is pending.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use agent_viewer_core::BackendKind;

type BackgroundJob<T> = Box<dyn FnOnce() -> Result<T, String> + Send + 'static>;

/// The footer text for a panicked job, keeping whatever the panic itself said.
fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match detail {
        Some(detail) => format!("background operation panicked: {detail}"),
        None => "background operation panicked".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSelection {
    pub backend: BackendKind,
    pub session_id: Option<String>,
    pub job_name: Option<String>,
    pub cwd: PathBuf,
    /// The earliest instant the new session could have been created: the spawn call itself for a
    /// direct spawn, the router INVOCATION for a routed one. Together with `spawned_at_ms` it
    /// brackets the interval the cwd + creation-time fallback searches, because a routed job is
    /// created while the router runs, not when its decision lands.
    pub submitted_at_ms: i64,
    /// The latest instant it could have been created: the spawn call, or the router's return.
    pub spawned_at_ms: i64,
    pub preexisting_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOutcome {
    pub notice: String,
    pub spawned: Option<SpawnSelection>,
}

pub struct BackgroundRunner<T> {
    tx: Sender<(String, Result<T, String>)>,
    rx: Receiver<(String, Result<T, String>)>,
    in_flight: HashSet<String>,
    dependents: HashMap<String, Option<BackgroundJob<T>>>,
    release_worker: Option<ReleaseWorker>,
}

pub type MutationRunner = BackgroundRunner<MutationOutcome>;
pub type AttachRunner<T> = BackgroundRunner<T>;

impl<T: Send + 'static> Default for BackgroundRunner<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> BackgroundRunner<T> {
    pub fn new() -> BackgroundRunner<T> {
        let (tx, rx) = channel();
        BackgroundRunner {
            tx,
            rx,
            in_flight: HashSet::new(),
            dependents: HashMap::new(),
            release_worker: None,
        }
    }

    pub fn release_handle(&mut self) -> ReleaseHandle {
        self.release_worker
            .get_or_insert_with(ReleaseWorker::new)
            .handle()
    }

    pub fn release_command(&mut self, release: std::process::Command) {
        self.release_worker
            .get_or_insert_with(ReleaseWorker::new)
            .release(release);
    }

    pub fn shutdown_releases(&mut self) {
        self.release_worker.take();
    }

    /// Run `op` on a worker thread under `key`. Returns false (a no-op) when `key` is
    /// already in flight.
    pub fn submit<F>(&mut self, key: String, op: F) -> bool
    where
        F: FnOnce() -> Result<T, String> + Send + 'static,
    {
        if self.in_flight.contains(&key) {
            return false;
        }
        self.in_flight.insert(key.clone());
        self.start(key, Box::new(op));
        true
    }

    /// Allow at most one success dependent and seal `key` until the pipeline completes.
    pub fn submit_after_success<F>(&mut self, key: String, op: F) -> bool
    where
        F: FnOnce() -> Result<T, String> + Send + 'static,
    {
        if self.in_flight.contains(&key) {
            if self.dependents.contains_key(&key) {
                return false;
            }
            self.dependents.insert(key, Some(Box::new(op)));
            return true;
        }
        self.in_flight.insert(key.clone());
        self.start(key, Box::new(op));
        true
    }

    fn start(&self, key: String, op: BackgroundJob<T>) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            // A panicking op must still report: `in_flight` is cleared only when a result is
            // received, so a lost one seals its key forever and that row silently stops
            // accepting archive, rename or stop for the rest of the run.
            let result = match catch_unwind(AssertUnwindSafe(op)) {
                Ok(result) => result,
                Err(payload) => Err(panic_message(&payload)),
            };
            // A closed receiver just means the TUI is shutting down; drop the result.
            let _ = tx.send((key, result));
        });
    }

    /// Drain one completed result if ready, starting its next dependent only on success.
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok((key, result)) => {
                if result.is_ok() {
                    let next = self.dependents.get_mut(&key).and_then(Option::take);
                    if let Some(op) = next {
                        self.start(key, op);
                    } else {
                        self.dependents.remove(&key);
                        self.in_flight.remove(&key);
                    }
                } else {
                    self.dependents.remove(&key);
                    self.in_flight.remove(&key);
                }
                Some(result)
            }
            Err(_) => None,
        }
    }

    pub fn in_flight(&self, key: &str) -> bool {
        self.in_flight.contains(key)
    }
}

const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RELEASE_START_TIMEOUT: Duration = Duration::from_millis(100);

struct ReleaseJob {
    command: std::process::Command,
    started: Sender<()>,
}

#[derive(Clone, Debug)]
pub struct ReleaseHandle {
    sender: Sender<ReleaseJob>,
}

impl ReleaseHandle {
    pub fn release(&self, command: std::process::Command) {
        let (started, wait_for_start) = channel();
        if self.sender.send(ReleaseJob { command, started }).is_ok() {
            let _ = wait_for_start.recv_timeout(RELEASE_START_TIMEOUT);
        }
    }
}

struct ReleaseWorker {
    handle: Option<ReleaseHandle>,
    join: Option<thread::JoinHandle<()>>,
}

impl ReleaseWorker {
    fn new() -> ReleaseWorker {
        let (sender, receiver) = channel::<ReleaseJob>();
        let join = thread::spawn(move || {
            for release in receiver {
                run_release(release);
            }
        });
        ReleaseWorker {
            handle: Some(ReleaseHandle { sender }),
            join: Some(join),
        }
    }

    fn handle(&self) -> ReleaseHandle {
        self.handle
            .as_ref()
            .expect("release worker is running")
            .clone()
    }

    fn release(&self, release: std::process::Command) {
        self.handle().release(release);
    }
}

impl Drop for ReleaseWorker {
    fn drop(&mut self) {
        self.handle.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_release(mut release: ReleaseJob) {
    release
        .command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = release.command.spawn() else {
        let _ = release.started.send(());
        return;
    };
    thread::sleep(RELEASE_POLL_INTERVAL);
    let _ = release.started.send(());
    let deadline = std::time::Instant::now() + RELEASE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(RELEASE_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

#[cfg(test)]
mod attach_runner_tests {
    use super::AttachRunner;
    use std::sync::mpsc::channel;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn attach_runner_executes_off_thread_and_deduplicates_pending_keys() {
        let caller = thread::current().id();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let mut runner = AttachRunner::<thread::ThreadId>::new();

        assert!(runner.submit("claude:target:attach".to_string(), move || {
            let worker = thread::current().id();
            started_tx.send(worker).expect("report worker");
            release_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release worker");
            Ok(worker)
        }));

        let worker = started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("attach worker started");
        assert_ne!(worker, caller);
        assert!(!runner.submit("claude:target:attach".to_string(), || {
            panic!("duplicate attach must not start")
        }));
        assert!(runner.poll().is_none());

        release_tx.send(()).expect("release attach worker");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let completed = loop {
            if let Some(result) = runner.poll() {
                break result.expect("attach worker result");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "attach worker did not finish"
            );
            thread::yield_now();
        };
        assert_eq!(completed, worker);
        assert!(!runner.in_flight("claude:target:attach"));
    }

    /// A job that panics must still land as a result. Without that its key stays in flight
    /// forever, and `submit` returning false is a silent no-op, so the row it belongs to never
    /// accepts another archive, rename or stop for the rest of the run.
    #[test]
    fn a_panicking_job_reports_an_error_and_frees_its_key() {
        let key = "claude:sealed:archive";
        let mut runner = AttachRunner::<()>::new();

        assert!(runner.submit(key.to_string(), || panic!("backend blew up")));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let result = loop {
            if let Some(result) = runner.poll() {
                break result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a panicking job never reported, so its key is sealed"
            );
            thread::yield_now();
        };
        assert_eq!(
            result.err().as_deref(),
            Some("background operation panicked: backend blew up")
        );
        assert!(!runner.in_flight(key));
        assert!(
            runner.submit(key.to_string(), || Ok(())),
            "the row must accept work again after a panicked job"
        );
    }
}

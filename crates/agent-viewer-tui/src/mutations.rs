//! Background runners for backend work that must not block the render loop. Each operation is
//! a self contained closure returning a structured outcome; results are drained without blocking
//! through `poll()`. In flight keys deduplicate repeated submissions while work is pending.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use agent_viewer_core::BackendKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSelection {
    pub backend: BackendKind,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
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
        }
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
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = op();
            // A closed receiver just means the TUI is shutting down; drop the result.
            let _ = tx.send((key, result));
        });
        true
    }

    /// Drain one completed result if ready (non-blocking), clearing its in-flight key.
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok((key, result)) => {
                self.in_flight.remove(&key);
                Some(result)
            }
            Err(_) => None,
        }
    }

    pub fn in_flight(&self, key: &str) -> bool {
        self.in_flight.contains(key)
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
}

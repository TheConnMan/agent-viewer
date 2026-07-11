//! MutationRunner — runs backend mutations (remove/stop/rename/hide) on a worker thread so
//! the render loop never blocks. Results are drained non-blocking via poll().

use agent_viewer_tui::mutations::MutationRunner;
use std::time::{Duration, Instant};

/// Poll until a result is ready or the timeout elapses (sleep between polls to avoid a spin).
fn poll_until(runner: &mut MutationRunner, timeout: Duration) -> Option<Result<String, String>> {
    let start = Instant::now();
    loop {
        if let Some(result) = runner.poll() {
            return Some(result);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn mutation_runner_runs_op_and_drains_result() {
    let mut runner = MutationRunner::new();
    // A sleep-free op that returns immediately.
    let accepted = runner.submit("k1".to_string(), "rename".to_string(), || Ok("renamed".to_string()));
    assert!(accepted);

    let result = poll_until(&mut runner, Duration::from_secs(2)).expect("op should complete");
    assert_eq!(result, Ok("renamed".to_string()));

    // Once drained, the key is no longer in flight and there is nothing else pending.
    assert!(!runner.in_flight("k1"));
    assert!(runner.poll().is_none());
}

#[test]
fn mutation_runner_dedups_in_flight_key() {
    let mut runner = MutationRunner::new();
    // A short-sleep op keeps the key in flight long enough to observe.
    let accepted = runner.submit("k2".to_string(), "stop".to_string(), || {
        std::thread::sleep(Duration::from_millis(100));
        Ok("stopped".to_string())
    });
    assert!(accepted);
    assert!(runner.in_flight("k2"));

    // Submitting the SAME key while it is in flight is a no-op (returns false).
    let duplicate = runner.submit("k2".to_string(), "stop".to_string(), || Ok("dup".to_string()));
    assert!(!duplicate);

    let result = poll_until(&mut runner, Duration::from_secs(2)).expect("slow op completes");
    assert_eq!(result, Ok("stopped".to_string()));
    assert!(!runner.in_flight("k2"));

    // After the result has been drained, the key can be submitted again — and errors
    // propagate through poll() as Err.
    let reaccepted = runner.submit("k2".to_string(), "stop".to_string(), || Err("boom".to_string()));
    assert!(reaccepted);
    let err = poll_until(&mut runner, Duration::from_secs(2)).expect("second op completes");
    assert_eq!(err, Err("boom".to_string()));
}

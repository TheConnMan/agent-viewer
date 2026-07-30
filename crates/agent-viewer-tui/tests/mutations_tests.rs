//! MutationRunner — runs backend mutations (remove/stop/rename/hide) on a worker thread so
//! the render loop never blocks. Results are drained non-blocking via poll().

use agent_viewer_tui::mutations::{MutationOutcome, MutationRunner};
use std::sync::mpsc::{TryRecvError, channel};
use std::time::{Duration, Instant};

/// Poll until a result is ready or the timeout elapses (sleep between polls to avoid a spin).
fn poll_until(
    runner: &mut MutationRunner,
    timeout: Duration,
) -> Option<Result<MutationOutcome, String>> {
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
    let accepted = runner.submit("k1".to_string(), || {
        Ok(MutationOutcome {
            notice: "renamed".to_string(),
            spawned: None,
        })
    });
    assert!(accepted);

    let result = poll_until(&mut runner, Duration::from_secs(2)).expect("op should complete");
    assert_eq!(
        result,
        Ok(MutationOutcome {
            notice: "renamed".to_string(),
            spawned: None,
        })
    );

    // Once drained, the key is no longer in flight and there is nothing else pending.
    assert!(!runner.in_flight("k1"));
    assert!(runner.poll().is_none());
}

#[test]
fn mutation_runner_dedups_in_flight_key() {
    let mut runner = MutationRunner::new();
    // A short-sleep op keeps the key in flight long enough to observe.
    let accepted = runner.submit("k2".to_string(), || {
        std::thread::sleep(Duration::from_millis(100));
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    });
    assert!(accepted);
    assert!(runner.in_flight("k2"));

    // Submitting the SAME key while it is in flight is a no-op (returns false).
    let duplicate = runner.submit("k2".to_string(), || {
        Ok(MutationOutcome {
            notice: "dup".to_string(),
            spawned: None,
        })
    });
    assert!(!duplicate);

    let result = poll_until(&mut runner, Duration::from_secs(2)).expect("slow op completes");
    assert_eq!(
        result,
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    );
    assert!(!runner.in_flight("k2"));

    // After the result has been drained, the key can be submitted again — and errors
    // propagate through poll() as Err.
    let reaccepted = runner.submit("k2".to_string(), || Err("boom".to_string()));
    assert!(reaccepted);
    let err = poll_until(&mut runner, Duration::from_secs(2)).expect("second op completes");
    assert_eq!(err, Err("boom".to_string()));
}

#[test]
fn dependent_mutation_waits_for_success_before_starting() {
    let mut runner = MutationRunner::new();
    let key = "codex:thread:kill".to_string();
    let (first_started_tx, first_started_rx) = channel();
    let (release_first_tx, release_first_rx) = channel();
    let (second_started_tx, second_started_rx) = channel();

    assert!(runner.submit_after_success(key.clone(), move || {
        first_started_tx.send(()).expect("report first start");
        release_first_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release first mutation");
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    }));
    first_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first mutation started");

    assert!(runner.submit_after_success(key.clone(), move || {
        second_started_tx.send(()).expect("report second start");
        Ok(MutationOutcome {
            notice: "removed".to_string(),
            spawned: None,
        })
    }));
    assert_eq!(second_started_rx.try_recv(), Err(TryRecvError::Empty));

    release_first_tx.send(()).expect("finish first mutation");
    let first = poll_until(&mut runner, Duration::from_secs(2)).expect("first mutation completes");
    assert_eq!(
        first,
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    );
    assert!(runner.in_flight(&key));

    second_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dependent mutation started after success");
    let second =
        poll_until(&mut runner, Duration::from_secs(2)).expect("dependent mutation completes");
    assert_eq!(
        second,
        Ok(MutationOutcome {
            notice: "removed".to_string(),
            spawned: None,
        })
    );
    assert!(!runner.in_flight(&key));
}

#[test]
fn dependent_mutation_is_discarded_after_failure() {
    let mut runner = MutationRunner::new();
    let key = "codex:thread:kill".to_string();
    let (first_started_tx, first_started_rx) = channel();
    let (release_first_tx, release_first_rx) = channel();
    let (second_started_tx, second_started_rx) = channel();

    assert!(runner.submit_after_success(key.clone(), move || {
        first_started_tx.send(()).expect("report first start");
        release_first_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release first mutation");
        Err("stop failed".to_string())
    }));
    first_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first mutation started");

    assert!(runner.submit_after_success(key.clone(), move || {
        second_started_tx.send(()).expect("report second start");
        Ok(MutationOutcome {
            notice: "removed".to_string(),
            spawned: None,
        })
    }));
    assert_eq!(second_started_rx.try_recv(), Err(TryRecvError::Empty));

    release_first_tx.send(()).expect("finish first mutation");
    let first = poll_until(&mut runner, Duration::from_secs(2)).expect("first mutation completes");
    assert_eq!(first, Err("stop failed".to_string()));
    assert!(!runner.in_flight(&key));
    assert_eq!(
        second_started_rx.try_recv(),
        Err(TryRecvError::Disconnected)
    );
    assert!(runner.poll().is_none());
}

#[test]
fn dependent_mutation_rejects_a_third_submission_for_the_same_key() {
    let mut runner = MutationRunner::new();
    let key = "codex:thread:kill".to_string();
    let (first_started_tx, first_started_rx) = channel();
    let (release_first_tx, release_first_rx) = channel();
    let (second_started_tx, second_started_rx) = channel();
    let (release_second_tx, release_second_rx) = channel();
    let (third_started_tx, third_started_rx) = channel();

    assert!(runner.submit_after_success(key.clone(), move || {
        first_started_tx.send(()).expect("report first start");
        release_first_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release first mutation");
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    }));
    first_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first mutation started");

    assert!(runner.submit_after_success(key.clone(), move || {
        second_started_tx.send(()).expect("report second start");
        release_second_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release second mutation");
        Ok(MutationOutcome {
            notice: "removed".to_string(),
            spawned: None,
        })
    }));

    release_first_tx.send(()).expect("finish first mutation");
    let first = poll_until(&mut runner, Duration::from_secs(2)).expect("first mutation completes");
    assert_eq!(
        first,
        Ok(MutationOutcome {
            notice: "stopped".to_string(),
            spawned: None,
        })
    );
    second_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("dependent mutation started");
    assert!(!runner.submit_after_success(key.clone(), move || {
        third_started_tx.send(()).expect("report third start");
        Ok(MutationOutcome {
            notice: "unexpected".to_string(),
            spawned: None,
        })
    }));
    assert_eq!(third_started_rx.try_recv(), Err(TryRecvError::Disconnected));

    release_second_tx
        .send(())
        .expect("finish dependent mutation");
    let second =
        poll_until(&mut runner, Duration::from_secs(2)).expect("dependent mutation completes");
    assert_eq!(
        second,
        Ok(MutationOutcome {
            notice: "removed".to_string(),
            spawned: None,
        })
    );
    assert_eq!(third_started_rx.try_recv(), Err(TryRecvError::Disconnected));
}

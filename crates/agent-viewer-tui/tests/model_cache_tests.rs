//! ModelCache: the composer's model catalog, seeded from the viewer DB at startup, refreshed
//! on a worker thread so a multi-second CLI probe never blocks the render loop, and drained
//! non-blocking via `poll()`.

use agent_viewer_core::backend::BackendKind;
use agent_viewer_core::codex::app_server::parse_skills_list;
use agent_viewer_tui::composer::CommandEntry;
use agent_viewer_tui::model_cache::{ModelCache, is_stale};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn poll_until_landed(cache: &mut ModelCache, timeout: Duration) -> Vec<(BackendKind, Vec<String>)> {
    let start = Instant::now();
    loop {
        let landed = cache.poll();
        if !landed.is_empty() {
            return landed;
        }
        if start.elapsed() > timeout {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn seeded_entries_are_served_without_a_probe() {
    let mut cache = ModelCache::default();
    let models = vec!["default".to_string(), "zai/glm-5.2".to_string()];
    cache.seed(BackendKind::Claude, models.clone(), true);

    assert_eq!(cache.models(BackendKind::Claude), Some(models.as_slice()));
    assert_eq!(cache.models(BackendKind::Codex), None);
}

#[test]
fn a_fresh_seed_suppresses_the_probe_entirely() {
    // Startup seeds a fresh disk row, so tabbing to that backend must not shell out at all.
    let mut cache = ModelCache::default();
    cache.seed(
        BackendKind::Claude,
        vec!["default".to_string(), "zai/glm-5.2".to_string()],
        true,
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    cache.request_with(BackendKind::Claude, move || {
        seen.fetch_add(1, Ordering::SeqCst);
        vec!["default".to_string()]
    });

    assert!(poll_until_landed(&mut cache, Duration::from_millis(200)).is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_stale_seed_still_serves_its_list_while_it_refreshes() {
    // The old catalog is far better than an empty picker, so it stays readable during the
    // refresh and is replaced only once the new one lands.
    let mut cache = ModelCache::default();
    cache.seed(
        BackendKind::Claude,
        vec!["default".to_string(), "stale/model".to_string()],
        false,
    );
    assert_eq!(
        cache.models(BackendKind::Claude),
        Some(["default".to_string(), "stale/model".to_string()].as_slice())
    );

    cache.request_with(BackendKind::Claude, || {
        vec!["default".to_string(), "fresh/model".to_string()]
    });
    let landed = poll_until_landed(&mut cache, Duration::from_secs(5));

    assert_eq!(
        landed,
        vec![(
            BackendKind::Claude,
            vec!["default".to_string(), "fresh/model".to_string()]
        )]
    );
    assert_eq!(
        cache.models(BackendKind::Claude),
        Some(["default".to_string(), "fresh/model".to_string()].as_slice())
    );
}

#[test]
fn a_probe_result_lands_through_poll() {
    let mut cache = ModelCache::default();
    assert_eq!(cache.models(BackendKind::Codex), None);

    cache.request_with(BackendKind::Codex, || {
        vec!["default".to_string(), "gpt-5.6-sol".to_string()]
    });
    let landed = poll_until_landed(&mut cache, Duration::from_secs(5));

    assert_eq!(landed.len(), 1);
    assert_eq!(landed[0].0, BackendKind::Codex);
    assert_eq!(
        cache.models(BackendKind::Codex),
        Some(["default".to_string(), "gpt-5.6-sol".to_string()].as_slice())
    );
}

#[test]
fn parsed_codex_skills_land_in_the_visible_command_catalog() {
    let mut cache = ModelCache::default();
    let cwd = PathBuf::from("/home/user/git/proj");
    let key = (BackendKind::Codex, Some(cwd.clone()));
    let response = r#"{"jsonrpc":"2.0","id":12,"result":{"data":[{"cwd":"/home/user/git/proj","errors":[],"skills":[{"enabled":true,"name":"project_check","path":"/home/user/git/proj/.agents/skills/project_check/SKILL.md"}]}]}}"#.to_string();

    assert!(
        cache.request_commands_with_codex_discovery(key.clone(), move |target| {
            parse_skills_list(&response, 12, target)
        })
    );

    let start = Instant::now();
    while cache.commands(&key).is_none() && start.elapsed() < Duration::from_secs(2) {
        cache.poll_commands();
        std::thread::sleep(Duration::from_millis(5));
    }

    let commands = cache.commands(&key).expect("command discovery lands");
    assert!(commands.contains(&CommandEntry::codex_skill(
        "project_check",
        cwd.join(".agents/skills/project_check/SKILL.md"),
    )));
}

#[test]
fn a_backend_is_probed_at_most_once_per_session() {
    // Every keystroke in the composer calls into the cache; a 4s shell-out per keystroke
    // would be a process storm.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cache = ModelCache::default();

    for _ in 0..5 {
        let seen = Arc::clone(&calls);
        cache.request_with(BackendKind::Claude, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            vec!["default".to_string(), "zai/glm-5.2".to_string()]
        });
    }
    poll_until_landed(&mut cache, Duration::from_secs(5));
    // Requests after the result has landed must not re-probe either.
    let seen = Arc::clone(&calls);
    cache.request_with(BackendKind::Claude, move || {
        seen.fetch_add(1, Ordering::SeqCst);
        vec!["default".to_string()]
    });

    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_probe_that_discovered_nothing_is_not_cached() {
    // A failed CLI probe degrades to just the default. Storing that would persist the
    // failure to disk and pin an empty picker for the whole TTL.
    let mut cache = ModelCache::default();
    cache.request_with(BackendKind::Claude, || vec!["default".to_string()]);

    assert!(poll_until_landed(&mut cache, Duration::from_millis(500)).is_empty());
    assert_eq!(cache.models(BackendKind::Claude), None);
}

#[test]
fn is_stale_is_a_pure_ttl_boundary() {
    let day = 24 * 60 * 60 * 1000;
    assert!(!is_stale(0, 0));
    assert!(!is_stale(0, day - 1));
    assert!(is_stale(0, day));
    // A clock that moved backwards must not mark a future stamp stale forever.
    assert!(!is_stale(5_000, 1_000));
}

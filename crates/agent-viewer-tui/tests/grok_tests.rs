use agent_viewer_core::BackendKind;
use agent_viewer_tui::app::{Composer, SpawnRoute};

#[test]
fn grok_identity_is_available_to_the_public_composer_surface() {
    let mut composer = Composer::new();
    composer.set_available_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Grok,
    ]);
    composer.select_backend(BackendKind::Grok);
    composer.set_models(
        vec!["default".to_string(), "grok-4".to_string()],
        BackendKind::Grok,
    );
    composer.cycle_model();

    assert_eq!(composer.backend(), BackendKind::Grok);
    assert_eq!(composer.provider_name(), "grok");
    assert_eq!(composer.model(), "grok-4");
    assert_eq!(BackendKind::Grok.tag(), "[gx]");
}

#[test]
fn pinned_grok_spawns_directly_while_auto_and_existing_pins_remain_routed() {
    let mut composer = Composer::new();
    composer.set_available_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Grok,
    ]);
    composer.set_auto_available(true);

    composer.select_backend(BackendKind::Grok);
    assert_eq!(composer.spawn_route(false), SpawnRoute::DirectBackend);

    composer.select_backend(BackendKind::Codex);
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);
    composer.select_backend(BackendKind::Claude);
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);

    composer.default_to_auto();
    assert!(composer.is_auto());
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);
}

mod common;

use agent_viewer_core::codex::parse_codex_catalog;

#[test]
fn parse_codex_catalog_keeps_list_only_in_priority_order() {
    let json = common::read_fixture("codex_debug_models.json");
    let got = parse_codex_catalog(&json);
    // visibility=="hide" (gpt-5.6-internal) is excluded; the three list entries come back
    // sorted by priority ascending, with the missing-priority entry (mini) last.
    assert_eq!(got, vec!["gpt-5.6-pro", "gpt-5.6-sol", "gpt-5.6-mini"]);
    assert!(!got.iter().any(|m| m == "gpt-5.6-internal"));
}

#[test]
fn parse_codex_catalog_malformed_is_empty() {
    assert!(parse_codex_catalog("not json").is_empty());
    assert!(parse_codex_catalog("{}").is_empty());
    assert!(parse_codex_catalog(r#"{"models":"nope"}"#).is_empty());
}

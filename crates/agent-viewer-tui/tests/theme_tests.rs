use agent_viewer_tui::ui::ThemeState;
use ratatui::style::Color;
use std::collections::HashSet;
use std::fs;
use std::time::Duration;

#[test]
fn builtins_resolve_distinct_core_tokens() {
    let (themes, notices) = ThemeState::load(false, None, std::path::Path::new("/missing"));
    assert!(notices.is_empty());
    assert_eq!(themes.themes().len(), 11);

    let accents = themes
        .themes()
        .iter()
        .map(|theme| theme.accent)
        .collect::<HashSet<Color>>();
    let backgrounds = themes
        .themes()
        .iter()
        .map(|theme| theme.bg)
        .collect::<HashSet<Color>>();
    let warnings = themes
        .themes()
        .iter()
        .map(|theme| theme.warn)
        .collect::<HashSet<Color>>();

    assert_eq!(accents.len(), 11);
    assert_eq!(backgrounds.len(), 11);
    assert_eq!(warnings.len(), 11);
}

#[test]
fn builtin_catalog_includes_the_seven_polish_themes() {
    let (themes, notices) = ThemeState::load(false, None, std::path::Path::new("/missing"));
    assert!(notices.is_empty());
    let ids = themes
        .themes()
        .iter()
        .map(|theme| theme.id.as_str())
        .collect::<HashSet<_>>();

    for expected in [
        "aubergine",
        "hoth",
        "catppuccin",
        "tokyonight",
        "gruvbox",
        "rosepine",
        "nord",
    ] {
        assert!(ids.contains(expected), "missing builtin theme {expected}");
    }
}

#[test]
fn partial_user_theme_overrides_accent_and_inherits_amber_background() {
    let directory = tempfile::tempdir().expect("theme directory");
    fs::write(directory.path().join("ocean.theme"), "accent = #123456\n").expect("write theme");

    let (themes, notices) = ThemeState::load(false, Some("ocean"), directory.path());
    assert!(notices.is_empty());
    assert_eq!(themes.active().id, "ocean");
    assert_eq!(themes.active().accent, Color::Rgb(0x12, 0x34, 0x56));
    assert_eq!(themes.active().bg, Color::Rgb(0x16, 0x13, 0x0d));
}

#[test]
fn malformed_line_keeps_valid_tokens_and_reports_notice() {
    let directory = tempfile::tempdir().expect("theme directory");
    fs::write(
        directory.path().join("mixed.theme"),
        "accent = #654321\nthis is malformed\nwarn = nope\n",
    )
    .expect("write theme");

    let (themes, notices) = ThemeState::load(false, Some("mixed"), directory.path());
    assert_eq!(themes.active().accent, Color::Rgb(0x65, 0x43, 0x21));
    assert_eq!(themes.active().warn, Color::Rgb(0xd9, 0xa9, 0x3f));
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("lines 2, 3"));
}

#[test]
fn active_user_theme_reloads_after_its_mtime_changes() {
    let directory = tempfile::tempdir().expect("theme directory");
    let path = directory.path().join("live.theme");
    fs::write(&path, "accent = #123456\n").expect("write first theme");
    let (mut themes, notices) = ThemeState::load(false, Some("live"), directory.path());
    assert!(notices.is_empty());
    assert_eq!(themes.active().accent, Color::Rgb(0x12, 0x34, 0x56));

    std::thread::sleep(Duration::from_millis(10));
    fs::write(&path, "accent = #abcdef\n").expect("write reloaded theme");
    assert!(themes.reload_active().is_some());
    assert_eq!(themes.active().accent, Color::Rgb(0xab, 0xcd, 0xef));
}

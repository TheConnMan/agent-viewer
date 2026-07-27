use agent_viewer_core::state::ViewerDb;
use ratatui::style::Color;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const STORAGE_PREFIX: &str = "theme:";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkSet {
    Text,
    Glyph,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowDensity {
    Normal,
    Compact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub tag: String,
    pub bg: Color,
    pub surface: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub selbg: Color,
    pub selfg: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub merged: Color,
    pub cc: Color,
    pub cx: Color,
    pub oc: Color,
    pub pulse_start: Color,
    pub pulse_end: Color,
    pub mark_set: MarkSet,
    pub row_density: RowDensity,
    pub animation: bool,
    source_path: Option<PathBuf>,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct ThemeState {
    themes: Vec<Theme>,
    active: usize,
    picker_origin: Option<usize>,
}

impl Default for ThemeState {
    fn default() -> Self {
        ThemeState {
            themes: builtins(false),
            active: 0,
            picker_origin: None,
        }
    }
}

impl ThemeState {
    pub fn load(
        glyph_marks: bool,
        persisted: Option<&str>,
        directory: &Path,
    ) -> (ThemeState, Vec<String>) {
        let mut themes = builtins(glyph_marks);
        let mut notices = Vec::new();
        if let Ok(entries) = fs::read_dir(directory) {
            let mut paths = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("theme"))
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                match load_user_theme(&path, glyph_marks) {
                    Ok((theme, notice)) => {
                        themes.push(theme);
                        if let Some(notice) = notice {
                            notices.push(notice);
                        }
                    }
                    Err(error) => notices.push(error),
                }
            }
        }
        let active = persisted
            .and_then(|id| themes.iter().position(|theme| theme.id == id))
            .unwrap_or(0);
        (
            ThemeState {
                themes,
                active,
                picker_origin: None,
            },
            notices,
        )
    }

    pub fn active(&self) -> &Theme {
        &self.themes[self.active]
    }

    pub fn themes(&self) -> &[Theme] {
        &self.themes
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn picker_open(&self) -> bool {
        self.picker_origin.is_some()
    }

    pub fn open_picker(&mut self) {
        if self.picker_origin.is_none() {
            self.picker_origin = Some(self.active);
        }
    }

    pub fn move_preview(&mut self, delta: i32) {
        if self.themes.is_empty() {
            return;
        }
        let current = self.active as i32;
        self.active = (current + delta).rem_euclid(self.themes.len() as i32) as usize;
    }

    pub fn cancel_picker(&mut self) {
        if let Some(origin) = self.picker_origin.take() {
            self.active = origin;
        }
    }

    pub fn commit_picker(&mut self) -> &str {
        self.picker_origin = None;
        &self.active().id
    }

    pub fn reload_active(&mut self) -> Option<String> {
        let path = self.active().source_path.clone()?;
        let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        if modified == self.active().modified {
            return None;
        }
        match load_user_theme(&path, self.active().mark_set == MarkSet::Glyph) {
            Ok((theme, notice)) => {
                self.themes[self.active] = theme;
                notice.or_else(|| Some(format!("reloaded theme {}", self.active().name)))
            }
            Err(error) => Some(error),
        }
    }
}

pub fn theme_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/agent-viewer/themes")
}

pub fn persisted_theme(db: &ViewerDb) -> Option<String> {
    db.collapsed_groups().ok()?.into_iter().find_map(|key| {
        key.strip_prefix(STORAGE_PREFIX)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    })
}

pub fn persist_theme(db: &ViewerDb, id: &str) -> anyhow::Result<()> {
    for key in db.collapsed_groups()? {
        if key.starts_with(STORAGE_PREFIX) {
            db.set_group_collapsed(&key, false)?;
        }
    }
    db.set_group_collapsed(&format!("{STORAGE_PREFIX}{id}"), true)?;
    Ok(())
}

pub fn builtins(glyph_marks: bool) -> Vec<Theme> {
    vec![
        amber(glyph_marks),
        terminal(glyph_marks),
        paper(glyph_marks),
        mono16(glyph_marks),
    ]
}

pub fn amber(glyph_marks: bool) -> Theme {
    Theme {
        id: "amber".into(),
        name: "analog amber".into(),
        tag: "default · warm".into(),
        bg: rgb(0x16, 0x13, 0x0d),
        surface: rgb(0x1d, 0x1a, 0x12),
        border: rgb(0x3a, 0x34, 0x27),
        text: rgb(0xe6, 0xdf, 0xcc),
        muted: rgb(0x8d, 0x85, 0x70),
        faint: rgb(0x5c, 0x56, 0x3f),
        accent: rgb(0xdf, 0xa6, 0x49),
        selbg: rgb(0x2e, 0x28, 0x17),
        selfg: rgb(0xf4, 0xee, 0xda),
        ok: rgb(0x7f, 0xae, 0x5e),
        warn: rgb(0xd9, 0xa9, 0x3f),
        err: rgb(0xcf, 0x6a, 0x52),
        merged: rgb(0xb0, 0x8a, 0xc9),
        cc: rgb(0xd9, 0x77, 0x57),
        cx: rgb(0x74, 0xaa, 0x9c),
        oc: rgb(0x9e, 0xcb, 0x6a),
        pulse_start: rgb(0xb5, 0x95, 0x50),
        pulse_end: rgb(0xf4, 0xc0, 0x72),
        mark_set: mark_set(glyph_marks),
        row_density: RowDensity::Normal,
        animation: true,
        source_path: None,
        modified: None,
    }
}

pub fn terminal(glyph_marks: bool) -> Theme {
    Theme {
        id: "terminal".into(),
        name: "terminal match".into(),
        tag: "derived from host".into(),
        bg: Color::Reset,
        surface: Color::Black,
        border: Color::DarkGray,
        text: Color::Reset,
        muted: Color::Gray,
        faint: Color::DarkGray,
        accent: Color::LightBlue,
        selbg: Color::DarkGray,
        selfg: Color::White,
        ok: Color::LightGreen,
        warn: Color::Yellow,
        err: Color::LightRed,
        merged: Color::LightMagenta,
        cc: Color::LightRed,
        cx: Color::LightCyan,
        oc: Color::LightGreen,
        pulse_start: Color::Yellow,
        pulse_end: Color::LightYellow,
        mark_set: mark_set(glyph_marks),
        row_density: RowDensity::Normal,
        animation: true,
        source_path: None,
        modified: None,
    }
}

pub fn paper(glyph_marks: bool) -> Theme {
    Theme {
        id: "paper".into(),
        name: "paper light".into(),
        tag: "light · print-safe".into(),
        bg: rgb(0xf4, 0xf1, 0xe8),
        surface: rgb(0xea, 0xe5, 0xd8),
        border: rgb(0xcf, 0xc7, 0xb3),
        text: rgb(0x2a, 0x26, 0x20),
        muted: rgb(0x6e, 0x68, 0x57),
        faint: rgb(0x9a, 0x93, 0x7f),
        accent: rgb(0xa1, 0x65, 0x2a),
        selbg: rgb(0xe2, 0xd9, 0xc1),
        selfg: rgb(0x1c, 0x19, 0x13),
        ok: rgb(0x4f, 0x7a, 0x3a),
        warn: rgb(0x8a, 0x64, 0x10),
        err: rgb(0xa8, 0x40, 0x2c),
        merged: rgb(0x7a, 0x4f, 0x96),
        cc: rgb(0xb0, 0x54, 0x36),
        cx: rgb(0x3f, 0x7a, 0x6d),
        oc: rgb(0x5b, 0x8a, 0x3c),
        pulse_start: rgb(0x8a, 0x64, 0x10),
        pulse_end: rgb(0xa1, 0x65, 0x2a),
        mark_set: mark_set(glyph_marks),
        row_density: RowDensity::Normal,
        animation: true,
        source_path: None,
        modified: None,
    }
}

pub fn mono16(glyph_marks: bool) -> Theme {
    Theme {
        id: "mono16".into(),
        name: "mono 16".into(),
        tag: "no color signal".into(),
        bg: rgb(0x00, 0x00, 0x00),
        surface: rgb(0x0b, 0x0b, 0x0b),
        border: rgb(0x4d, 0x4d, 0x4d),
        text: rgb(0xc6, 0xc6, 0xc6),
        muted: rgb(0x8a, 0x8a, 0x8a),
        faint: rgb(0x55, 0x55, 0x55),
        accent: rgb(0xff, 0xff, 0xff),
        selbg: rgb(0x2f, 0x2f, 0x2f),
        selfg: rgb(0xff, 0xff, 0xff),
        ok: rgb(0xc6, 0xc6, 0xc6),
        warn: rgb(0xff, 0xff, 0xff),
        err: rgb(0xc6, 0xc6, 0xc6),
        merged: rgb(0x8a, 0x8a, 0x8a),
        cc: rgb(0xc6, 0xc6, 0xc6),
        cx: rgb(0x8a, 0x8a, 0x8a),
        oc: rgb(0xc6, 0xc6, 0xc6),
        pulse_start: rgb(0x8a, 0x8a, 0x8a),
        pulse_end: rgb(0xff, 0xff, 0xff),
        mark_set: mark_set(glyph_marks),
        row_density: RowDensity::Normal,
        animation: false,
        source_path: None,
        modified: None,
    }
}

fn load_user_theme(path: &Path, glyph_marks: bool) -> Result<(Theme, Option<String>), String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("could not load theme {}: {error}", path.display()))?;
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| format!("invalid theme filename {}", path.display()))?
        .to_string();
    let mut theme = amber(glyph_marks);
    theme.id = id.clone();
    theme.name = id;
    theme.tag = "user".into();
    theme.source_path = Some(path.to_path_buf());
    theme.modified = fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let mut malformed = Vec::new();

    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((token, value)) = line.split_once('=') else {
            malformed.push(index + 1);
            continue;
        };
        let Some(color) = parse_color(value.trim()) else {
            malformed.push(index + 1);
            continue;
        };
        if !set_color(&mut theme, token.trim(), color) {
            malformed.push(index + 1);
        }
    }

    let notice = (!malformed.is_empty()).then(|| {
        format!(
            "theme {} skipped malformed line{} {}",
            path.display(),
            if malformed.len() == 1 { "" } else { "s" },
            malformed
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    Ok((theme, notice))
}

fn parse_color(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(rgb(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

fn set_color(theme: &mut Theme, token: &str, color: Color) -> bool {
    let target = match token {
        "bg" => &mut theme.bg,
        "surface" => &mut theme.surface,
        "border" => &mut theme.border,
        "text" => &mut theme.text,
        "muted" => &mut theme.muted,
        "faint" => &mut theme.faint,
        "accent" => &mut theme.accent,
        "selbg" => &mut theme.selbg,
        "selfg" => &mut theme.selfg,
        "ok" => &mut theme.ok,
        "warn" => &mut theme.warn,
        "err" => &mut theme.err,
        "merged" => &mut theme.merged,
        "cc" => &mut theme.cc,
        "cx" => &mut theme.cx,
        "oc" => &mut theme.oc,
        "pulse_start" => &mut theme.pulse_start,
        "pulse_end" => &mut theme.pulse_end,
        _ => return false,
    };
    *target = color;
    true
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

fn mark_set(glyph_marks: bool) -> MarkSet {
    if glyph_marks {
        MarkSet::Glyph
    } else {
        MarkSet::Text
    }
}

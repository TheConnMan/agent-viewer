use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::overlay::centered_rect;
use super::sprite::SpriteKind;
use super::{Theme, fg};
use agent_viewer_core::BackendKind;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PaletteGroup {
    Actions,
    Sessions,
    Models,
    Commands,
    // Last on purpose: group order outranks match score, so a decoration row must never beat a
    // session or model the query actually meant.
    Sprites,
}

impl PaletteGroup {
    fn label(self) -> &'static str {
        match self {
            Self::Actions => "ACTIONS",
            Self::Sessions => "SESSIONS",
            Self::Models => "MODELS",
            Self::Commands => "COMMANDS",
            Self::Sprites => "HEADER SPRITE",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaletteAction {
    Attach,
    Archive,
    Unarchive,
    Rename,
    Reply,
    StopOrRemove,
    ShowAll,
    Group,
    Filter,
    AgeRamp,
    TailPane,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaletteTarget {
    Action(PaletteAction),
    Session { backend: BackendKind, id: String },
    Model { backend: BackendKind, name: String },
    Sprite(SpriteKind),
    Command(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteItem {
    pub group: PaletteGroup,
    pub icon: String,
    pub name: String,
    pub detail: String,
    pub key_hint: Option<String>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub target: PaletteTarget,
}

impl PaletteItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        group: PaletteGroup,
        icon: impl Into<String>,
        name: impl Into<String>,
        detail: impl Into<String>,
        key_hint: Option<&str>,
        enabled: bool,
        disabled_reason: Option<String>,
        target: PaletteTarget,
    ) -> Self {
        Self {
            group,
            icon: icon.into(),
            name: name.into(),
            detail: detail.into(),
            key_hint: key_hint.map(str::to_string),
            enabled,
            disabled_reason,
            target,
        }
    }

    fn shown_detail(&self) -> &str {
        self.disabled_reason
            .as_deref()
            .unwrap_or(self.detail.as_str())
    }
}

#[derive(Debug)]
pub struct PaletteState {
    query: String,
    scope: Option<PaletteGroup>,
    items: Vec<PaletteItem>,
    results: Vec<usize>,
    highlight: usize,
}

impl PaletteState {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        let mut state = Self {
            query: String::new(),
            scope: None,
            items,
            results: Vec::new(),
            highlight: 0,
        };
        state.rank();
        state
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn scope(&self) -> Option<PaletteGroup> {
        self.scope
    }

    pub fn results(&self) -> impl Iterator<Item = &PaletteItem> {
        self.results.iter().map(|index| &self.items[*index])
    }

    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    pub fn highlighted(&self) -> Option<&PaletteItem> {
        let index = *self.results.get(self.highlight)?;
        self.items.get(index)
    }

    pub fn push(&mut self, character: char) {
        self.query.push(character);
        self.highlight = 0;
        self.rank();
    }

    pub fn push_str(&mut self, text: &str) {
        self.query.push_str(text);
        self.highlight = 0;
        self.rank();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.highlight = 0;
        self.rank();
    }

    pub fn move_highlight(&mut self, delta: i32) {
        let count = self.results.len();
        if count == 0 {
            return;
        }
        self.highlight = (self.highlight as i32 + delta).rem_euclid(count as i32) as usize;
    }

    pub fn scope_highlighted(&mut self) {
        let Some(group) = self.highlighted().map(|item| item.group) else {
            return;
        };
        self.scope = Some(group);
        self.highlight = 0;
        self.rank();
    }

    pub fn escape(&mut self) -> bool {
        if self.scope.take().is_some() {
            self.highlight = 0;
            self.rank();
            false
        } else {
            true
        }
    }

    fn rank(&mut self) {
        let query = self.query.trim();
        let mut ranked = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.scope.is_none_or(|scope| item.group == scope))
            .filter_map(|(index, item)| {
                if query.is_empty() {
                    let default = (item.group == PaletteGroup::Actions && item.enabled)
                        || item.group == PaletteGroup::Sprites
                        || item.group == PaletteGroup::Sessions;
                    return default.then_some((index, 0));
                }
                let name_score = fuzzy_score(query, &item.name);
                let detail_score = fuzzy_score(query, item.shown_detail()).map(|score| score - 250);
                name_score
                    .into_iter()
                    .chain(detail_score)
                    .max()
                    .map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
            let left = &self.items[*left_index];
            let right = &self.items[*right_index];
            left.group
                .cmp(&right.group)
                .then_with(|| right_score.cmp(left_score))
                .then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                })
                .then_with(|| left.name.cmp(&right.name))
        });
        self.results = ranked.into_iter().map(|(index, _)| index).collect();
        self.highlight = self.highlight.min(self.results.len().saturating_sub(1));
    }
}

pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let candidate = candidate
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let mut positions = Vec::with_capacity(query.len());
    let mut cursor = 0;
    for wanted in query.iter().copied() {
        let offset = candidate
            .iter()
            .skip(cursor)
            .position(|character| *character == wanted)?;
        let position = cursor + offset;
        positions.push(position);
        cursor = position + 1;
    }

    let first = positions[0];
    let mut score = 0_i64;
    if first == 0 {
        score += 1_000;
    }
    score -= first as i64 * 5;
    for (index, position) in positions.iter().copied().enumerate() {
        let boundary = position == 0 || !candidate[position - 1].is_alphanumeric();
        if boundary {
            score += 80;
        }
        if index > 0 && position == positions[index - 1] + 1 {
            score += 120;
        }
    }
    Some(score)
}

enum RenderRow<'a> {
    Group(PaletteGroup),
    Item {
        result_index: usize,
        item: &'a PaletteItem,
    },
}

pub(super) fn draw(frame: &mut Frame, palette: &PaletteState, now_ms: i64, theme: &Theme) {
    let render_rows = grouped_rows(palette);
    let frame_area = frame.area();
    let available_width = frame_area.width.saturating_sub(4);
    let desired_width = 80.min(available_width);
    let available_height = frame_area.height.saturating_sub(3);
    let desired_height = (render_rows.len() as u16 + 4).min(available_height);
    if desired_width < 8 || desired_height < 4 {
        return;
    }
    let width_percent =
        ((u32::from(desired_width) * 100 / u32::from(frame_area.width)) as u16).max(1);
    let height_percent =
        ((u32::from(desired_height) * 100 / u32::from(frame_area.height)) as u16).max(1);
    let mut area = centered_rect(width_percent, height_percent, frame_area);
    area.width = desired_width;
    area.height = desired_height;
    area.x = frame_area.x + frame_area.width.saturating_sub(area.width) / 2;
    area.y = frame_area.y + 2.min(frame_area.height.saturating_sub(area.height));

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(fg(theme.accent))
        .style(Style::default().bg(theme.surface).fg(theme.text));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height < 2 {
        return;
    }

    draw_header(frame, palette, now_ms, theme, inner);
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(fg(theme.border)),
        Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        },
    );

    let rows_area = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };
    draw_rows(frame, palette, &render_rows, theme, rows_area);
}

fn grouped_rows(palette: &PaletteState) -> Vec<RenderRow<'_>> {
    let mut rows = Vec::new();
    let mut previous = None;
    for (result_index, item) in palette.results().enumerate() {
        if previous != Some(item.group) {
            rows.push(RenderRow::Group(item.group));
            previous = Some(item.group);
        }
        rows.push(RenderRow::Item { result_index, item });
    }
    rows
}

fn draw_header(frame: &mut Frame, palette: &PaletteState, now_ms: i64, theme: &Theme, area: Rect) {
    let right = format!("{} results · ⏎ run · ⇥ into scope", palette.result_count());
    let caret = if (now_ms / 500).rem_euclid(2) == 0 {
        "▏"
    } else {
        " "
    };
    let scope = palette
        .scope()
        .map(|group| format!("{} ", group.label()))
        .unwrap_or_default();
    let left_text = format!("{scope}{}{caret}", palette.query());
    let left_width = area
        .width
        .saturating_sub(3)
        .saturating_sub(right.width() as u16) as usize;
    let left = field(&left_text, left_width);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(field("⌕", 3), fg(theme.accent)),
            Span::styled(left, fg(theme.text)),
            Span::styled(right, fg(theme.faint)),
        ])),
        Rect { height: 1, ..area },
    );
}

fn draw_rows(
    frame: &mut Frame,
    palette: &PaletteState,
    rows: &[RenderRow<'_>],
    theme: &Theme,
    area: Rect,
) {
    let visible = area.height as usize;
    if visible == 0 {
        return;
    }
    let selected_row = rows
        .iter()
        .position(|row| {
            matches!(
                row,
                RenderRow::Item { result_index, .. } if *result_index == palette.highlight
            )
        })
        .unwrap_or(0);
    let mut start = selected_row
        .saturating_sub(visible / 2)
        .min(rows.len().saturating_sub(visible));
    if start > 0 && matches!(rows.get(start - 1), Some(RenderRow::Group(_))) {
        start -= 1;
    }

    for (line, row) in rows.iter().skip(start).take(visible).enumerate() {
        let row_area = Rect {
            y: area.y + line as u16,
            height: 1,
            ..area
        };
        match row {
            RenderRow::Group(group) => frame.render_widget(
                Paragraph::new(group.label()).style(fg(theme.faint)),
                row_area,
            ),
            RenderRow::Item { result_index, item } => {
                let selected = *result_index == palette.highlight;
                frame.render_widget(
                    Paragraph::new(item_line(item, area.width, selected, theme)).style(
                        if selected {
                            Style::default().bg(theme.selbg)
                        } else {
                            Style::default()
                        },
                    ),
                    row_area,
                );
            }
        }
    }
}

fn item_line(item: &PaletteItem, width: u16, selected: bool, theme: &Theme) -> Line<'static> {
    let width = width as usize;
    let hint_width = item
        .key_hint
        .as_deref()
        .map(UnicodeWidthStr::width)
        .unwrap_or(0);
    let hint_field_width = hint_width.min(width.saturating_sub(3));
    let content_width = width.saturating_sub(3 + hint_field_width);
    let name_width = 34.min(content_width);
    let detail_width = content_width.saturating_sub(name_width);
    let primary = if selected {
        let style = fg(theme.selfg);
        if item.enabled {
            style
        } else {
            style.add_modifier(Modifier::CROSSED_OUT)
        }
    } else if item.enabled {
        fg(theme.text)
    } else {
        fg(theme.faint).add_modifier(Modifier::CROSSED_OUT)
    };
    let secondary = if selected {
        let style = fg(theme.selfg);
        if item.enabled {
            style
        } else {
            style.add_modifier(Modifier::CROSSED_OUT)
        }
    } else if item.enabled {
        fg(theme.faint)
    } else {
        fg(theme.faint).add_modifier(Modifier::CROSSED_OUT)
    };
    Line::from(vec![
        Span::styled(field(&item.icon, 3), primary),
        Span::styled(field(&item.name, name_width), primary),
        Span::styled(field(item.shown_detail(), detail_width), secondary),
        Span::styled(
            field(
                item.key_hint.as_deref().unwrap_or_default(),
                hint_field_width,
            ),
            secondary,
        ),
    ])
}

fn field(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = ellipsize(value, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let available = width - 1;
    for grapheme in value.graphemes(true) {
        if result.width() + grapheme.width() > available {
            break;
        }
        result.push_str(grapheme);
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::{PaletteGroup, PaletteItem, PaletteState, PaletteTarget, fuzzy_score};
    use agent_viewer_core::BackendKind;

    fn item(group: PaletteGroup, name: &str, detail: &str) -> PaletteItem {
        PaletteItem::new(
            group,
            "▸",
            name,
            detail,
            None,
            true,
            None,
            PaletteTarget::Command(name.to_string()),
        )
    }

    #[test]
    fn prefix_match_beats_mid_word_match() {
        assert!(
            fuzzy_score("arch", "Archive").unwrap()
                > fuzzy_score("arch", "Search archive").unwrap()
        );
    }

    #[test]
    fn contiguous_match_beats_scattered_subsequence() {
        assert!(
            fuzzy_score("abc", "zz abc").unwrap() > fuzzy_score("abc", "zz a x b x c").unwrap()
        );
    }

    #[test]
    fn representative_queries_rank_the_expected_type_first() {
        let archive = item(PaletteGroup::Actions, "Archive session", "hide selected");
        let model = PaletteItem::new(
            PaletteGroup::Models,
            "◇",
            "opus[1m]",
            "claude model",
            None,
            true,
            None,
            PaletteTarget::Model {
                backend: BackendKind::Claude,
                name: "opus[1m]".to_string(),
            },
        );
        let session = PaletteItem::new(
            PaletteGroup::Sessions,
            "✽",
            "RS-123 palette",
            "working",
            None,
            true,
            None,
            PaletteTarget::Session {
                backend: BackendKind::Claude,
                id: "session".to_string(),
            },
        );
        let distractors = [
            item(PaletteGroup::Actions, "Search archive history", "arch"),
            item(PaletteGroup::Models, "not opus profile", "model"),
            item(PaletteGroup::Sessions, "Read status one", "rs 1"),
        ];

        for (query, expected) in [
            ("arch", "Archive session"),
            ("opus", "opus[1m]"),
            ("rs-123", "RS-123 palette"),
        ] {
            let mut palette = PaletteState::new(
                distractors
                    .iter()
                    .cloned()
                    .chain([archive.clone(), model.clone(), session.clone()])
                    .collect(),
            );
            for character in query.chars() {
                palette.push(character);
            }
            assert_eq!(palette.highlighted().unwrap().name, expected);
        }
    }

    #[test]
    fn disabled_action_stays_in_matching_results_with_a_reason() {
        let mut palette = PaletteState::new(vec![PaletteItem::new(
            PaletteGroup::Actions,
            "▸",
            "Archive session",
            "hide selected",
            Some("⌃D"),
            false,
            Some("unavailable · backend does not support archive".to_string()),
            PaletteTarget::Command("archive".to_string()),
        )]);
        for character in "arch".chars() {
            palette.push(character);
        }
        let result = palette.highlighted().expect("disabled result remains");
        assert!(!result.enabled);
        assert!(!result.disabled_reason.as_deref().unwrap().is_empty());
    }
}

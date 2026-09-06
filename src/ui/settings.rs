//! The settings form screen.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;

use super::truncate;
use crate::app::App;
use crate::config::ThemeName;
use crate::settings::{prompt_layout, IMMICH_KEY, LLM_KEY, PROMPT, THEME};
use crate::theme::Theme;

const LABEL_WIDTH: usize = 19;
const PROMPT_HEIGHT: usize = 4;

pub fn render(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();
    let form = &app.settings;
    let width = area.width.saturating_sub(2).clamp(40, 78);
    let height = (form.fields.len() as u16 + 9 + (PROMPT_HEIGHT as u16 - 1)).min(area.height);
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [boxed] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(Span::styled(" settings ", theme.title));
    let inner = block.inner(boxed);
    frame.render_widget(block, boxed);

    let value_width = (inner.width as usize).saturating_sub(2 + LABEL_WIDTH + 13);
    let prompt_width = value_width.saturating_sub(1).max(1);
    form.set_prompt_width(prompt_width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focused_line = 0;
    for (i, field) in form.fields.iter().enumerate() {
        if i == PROMPT {
            let layout = prompt_layout(&field.value, form.prompt_cursor, prompt_width);
            let prompt_start = if form.focused == PROMPT {
                layout
                    .cursor_row
                    .saturating_sub(PROMPT_HEIGHT.saturating_sub(1))
            } else {
                0
            };
            if form.focused == PROMPT {
                focused_line = lines.len() + layout.cursor_row.saturating_sub(prompt_start);
            }
            for row in 0..PROMPT_HEIGHT {
                let row_index = prompt_start + row;
                let mut value = layout.rows.get(row_index).cloned().unwrap_or_default();
                if form.focused == PROMPT && row_index == layout.cursor_row {
                    value = insert_cursor(&value, layout.cursor_column);
                }
                let label = if row == 0 { field.label } else { "" };
                let value_style = if form.focused == PROMPT {
                    theme.accent
                } else {
                    theme.value
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if form.focused == PROMPT && row == 0 {
                            "▸ "
                        } else {
                            "  "
                        },
                        theme.accent,
                    ),
                    Span::styled(format!("{label:<width$}", width = LABEL_WIDTH), theme.label),
                    Span::styled(
                        format!("{value:<width$}", width = value_width + 1),
                        value_style,
                    ),
                ]));
            }
            continue;
        }
        let focused = i == form.focused;
        if focused {
            focused_line = lines.len();
        }
        let mut value = truncate(&form.display_value(i), value_width);
        if focused {
            value.push('▏');
        }
        let value_style = if focused { theme.accent } else { theme.value };
        let mut spans = vec![
            Span::styled(if focused { "▸ " } else { "  " }, theme.accent),
            Span::styled(
                format!("{:<width$}", field.label, width = LABEL_WIDTH),
                theme.label,
            ),
            Span::styled(
                format!("{value:<width$}", width = value_width + 1),
                value_style,
            ),
        ];
        if i == IMMICH_KEY || i == LLM_KEY {
            let hint = if form.show_secrets {
                "ctrl-r hide"
            } else {
                "ctrl-r show"
            };
            spans.push(Span::styled(hint, theme.dim));
        }
        lines.push(Line::from(spans));
    }
    if form.focused == THEME {
        focused_line = lines.len();
    }
    lines.push(theme_line(form, theme));
    lines.push(Line::default());
    lines.push(test_line(app, theme));
    lines.push(match &form.message {
        Some(msg) => Line::from(Span::styled(format!("  {msg}"), theme.err)),
        None => Line::default(),
    });
    let content_height = inner.height.saturating_sub(1);
    let max_scroll = lines.len().saturating_sub(content_height as usize);
    let visible_focus_offset = content_height.saturating_sub(1) as usize;
    let mut scroll = focused_line.saturating_sub(visible_focus_offset);
    scroll = scroll.min(max_scroll);
    let content = Rect {
        height: content_height,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), content);

    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let key = |k: &str, label: &str| {
        vec![
            Span::styled(format!(" {k} "), theme.accent),
            Span::styled(format!("{label}   "), theme.label),
        ]
    };
    let mut spans = key("ctrl-s", "save");
    spans.extend(key("ctrl-t", "test"));
    spans.extend(key("← →", "theme"));
    spans.extend(key("ctrl-u", "clear"));
    spans.extend(key("esc", "back"));
    frame.render_widget(Paragraph::new(Line::from(spans)), footer);
}

fn insert_cursor(value: &str, column: usize) -> String {
    let byte_index = value
        .grapheme_indices(true)
        .nth(column)
        .map_or(value.len(), |(index, _)| index);
    let mut output = value.to_string();
    output.insert(byte_index, '▏');
    output
}

fn theme_line(form: &crate::settings::SettingsForm, theme: &Theme) -> Line<'static> {
    let focused = form.focused == THEME;
    let selected = |name: ThemeName| {
        if form.theme == name {
            if focused {
                theme.accent
            } else {
                theme.value
            }
        } else {
            theme.dim
        }
    };
    let option = |name: ThemeName| {
        let marker = if form.theme == name { "●" } else { " " };
        Span::styled(format!("({marker}) {}", name.label()), selected(name))
    };
    Line::from(vec![
        Span::styled(if focused { "▸ " } else { "  " }, theme.accent),
        Span::styled(
            format!("{:<width$}", "theme", width = LABEL_WIDTH),
            theme.label,
        ),
        option(ThemeName::Btop),
        Span::styled("   ", theme.dim),
        option(ThemeName::Mono),
        if focused {
            Span::styled("   ← →", theme.dim)
        } else {
            Span::raw("")
        },
    ])
}

fn test_line(app: &App, theme: &Theme) -> Line<'static> {
    let form = &app.settings;
    let mut spans = vec![
        Span::styled("  ctrl-t ", theme.accent),
        Span::styled("test connections   ", theme.label),
    ];
    if form.testing {
        spans.push(Span::styled("testing…", theme.warn));
        return Line::from(spans);
    }
    if let Some((immich, llm)) = &form.test_result {
        for (label, result) in [("immich", immich), ("llm", llm)] {
            spans.push(Span::styled(format!("{label} "), theme.label));
            match result {
                Ok(text) => {
                    spans.push(Span::styled("✓ ", theme.ok));
                    spans.push(Span::styled(
                        format!("{}   ", truncate(text, 20)),
                        theme.value,
                    ));
                }
                Err(text) => {
                    spans.push(Span::styled("✗ ", theme.err));
                    spans.push(Span::styled(
                        format!("{}   ", truncate(text, 28)),
                        theme.err,
                    ));
                }
            }
        }
    }
    Line::from(spans)
}

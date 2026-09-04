//! The settings form screen.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::truncate;
use crate::app::App;
use crate::settings::{IMMICH_KEY, LLM_KEY};
use crate::theme::Theme;

pub fn render(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();
    let form = &app.settings;
    let width = area.width.saturating_sub(2).clamp(40, 78);
    let height = (form.fields.len() as u16 + 8).min(area.height);
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

    let value_width = (inner.width as usize).saturating_sub(19 + 13);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focused;
        let mut value = truncate(&form.display_value(i), value_width);
        if focused {
            value.push('▏');
        }
        let value_style = if focused { theme.accent } else { theme.value };
        let mut spans = vec![
            Span::styled(if focused { "▸ " } else { "  " }, theme.accent),
            Span::styled(format!("{:<17}", field.label), theme.label),
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
    lines.push(Line::default());
    lines.push(test_line(app, theme));
    lines.push(match &form.message {
        Some(msg) => Line::from(Span::styled(format!("  {msg}"), theme.err)),
        None => Line::default(),
    });
    frame.render_widget(Paragraph::new(lines), inner);

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
    spans.extend(key("esc", "back"));
    frame.render_widget(Paragraph::new(Line::from(spans)), footer);
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

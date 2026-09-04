//! The run screen: header, progress, counters, in-flight, log, popup, footer.

use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::{fmt_clock, fmt_count, fmt_secs, truncate};
use crate::app::{App, LogRow, RunState};
use crate::theme::Theme;

pub fn render(frame: &mut Frame, app: &App, now: Instant, theme: &Theme) {
    let area = frame.area();
    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(header_left(app, theme))
        .title(header_right(app, theme).right_aligned());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.height <= 8 {
        render_tiny(frame, inner, app, now, theme);
        return;
    }

    let stacked = area.width < 80;
    let show_in_flight = area.height >= 24;
    let top_height = if stacked { 9 } else { 5 };
    let in_flight_height = if show_in_flight {
        app.config.run.workers.max(1) as u16 + 2
    } else {
        0
    };
    let [top, in_flight, log, footer] = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Length(in_flight_height),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);

    if stacked {
        let [p, c] = Layout::vertical([Constraint::Length(4), Constraint::Length(5)]).areas(top);
        render_progress(frame, p, app, now, theme);
        render_counters(frame, c, app, theme);
    } else {
        let [p, c] = if area.width < 100 {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top)
        } else {
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top)
        };
        render_progress(frame, p, app, now, theme);
        render_counters(frame, c, app, theme);
    }
    if show_in_flight {
        render_in_flight(frame, in_flight, app, now, theme);
    }
    render_log(frame, log, app, theme);
    render_footer(frame, footer, app, theme);
    if app.log_expanded {
        render_popup(frame, area, app, theme);
    }
}

fn header_left(app: &App, theme: &Theme) -> Line<'static> {
    let workers = app.config.run.workers;
    Line::from(vec![
        Span::styled(" immich-alt-text ", theme.title),
        Span::styled("─ ", theme.border),
        Span::styled(app.immich_host(), theme.value),
        Span::styled(" ─ ", theme.border),
        Span::styled(app.config.llm.model.clone(), theme.value),
        Span::styled(" @ ", theme.dim),
        Span::styled(app.llm_host(), theme.value),
        Span::styled(" ─ ", theme.border),
        Span::styled(
            format!("{workers} worker{} ", if workers == 1 { "" } else { "s" }),
            theme.value,
        ),
    ])
}

fn header_right(app: &App, theme: &Theme) -> Line<'static> {
    let label = app.state_label();
    let mut spans = vec![Span::styled(format!(" {label} "), theme.state_style(label))];
    if let RunState::Error(msg) = &app.run_state {
        spans.insert(
            0,
            Span::styled(format!(" {} ", truncate(msg, 40)), theme.err),
        );
    }
    Line::from(spans)
}

fn boxed<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(Span::styled(format!(" {title} "), theme.title))
}

fn bar(width: usize, ratio: f64, theme: &Theme) -> Vec<Span<'static>> {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    (0..width)
        .map(|i| {
            if i < filled {
                let t = i as f64 / width.max(1) as f64;
                Span::styled("█", Style::default().fg(theme.bar_color(t)))
            } else {
                Span::styled("░", theme.bar_empty)
            }
        })
        .collect()
}

fn render_progress(frame: &mut Frame, area: Rect, app: &App, now: Instant, theme: &Theme) {
    let block = boxed("progress", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let counts = format!(
        " {} / {}",
        fmt_count(app.done + app.failed),
        fmt_count(app.queued)
    );
    let bar_width = (inner.width as usize).saturating_sub(counts.chars().count() + 1);
    let mut line1 = bar(bar_width, app.progress_ratio(), theme);
    line1.push(Span::styled(counts, theme.value));
    let line2 = if inner.width < 43 {
        let rate = app
            .rate_per_min(now)
            .map(|r| format!("{r:.1}/m"))
            .unwrap_or_else(|| "--/m".into());
        let eta = app
            .eta(now)
            .map(fmt_eta_compact)
            .unwrap_or_else(|| "--:--".into());
        Line::from(vec![
            Span::styled("el ", theme.label),
            Span::styled(fmt_clock(app.elapsed(now)), theme.value),
            Span::styled(" rt ", theme.label),
            Span::styled(rate, theme.value),
            Span::styled(" eta ", theme.label),
            Span::styled(eta, theme.value),
        ])
    } else {
        let rate = app
            .rate_per_min(now)
            .map(|r| format!("{r:.1}/min"))
            .unwrap_or_else(|| "--".into());
        let eta = app
            .eta(now)
            .map(fmt_clock)
            .unwrap_or_else(|| "--:--:--".into());
        Line::from(vec![
            Span::styled("elapsed ", theme.label),
            Span::styled(fmt_clock(app.elapsed(now)), theme.value),
            Span::styled("   rate ", theme.label),
            Span::styled(rate, theme.value),
            Span::styled("   eta ", theme.label),
            Span::styled(eta, theme.value),
        ])
    };
    frame.render_widget(Paragraph::new(vec![Line::from(line1), line2]), inner);
}

fn fmt_eta_compact(d: Duration) -> String {
    let total_minutes = (d.as_secs() + 30) / 60;
    format!("{:02}:{:02}", total_minutes / 60, total_minutes % 60)
}

fn render_counters(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = boxed("counters", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let pair = |l1: &str, v1: String, l2: &str, v2: String, s2: Style| {
        Line::from(vec![
            Span::styled(format!("{l1:<9}"), theme.label),
            Span::styled(format!("{v1:>8}"), theme.value),
            Span::styled(format!("   {l2:<9}"), theme.label),
            Span::styled(format!("{v2:>8}"), s2),
        ])
    };
    let avg = |d: Option<Duration>| d.map(fmt_secs).unwrap_or_else(|| "--".into());
    let failed_style = if app.failed > 0 {
        theme.err
    } else {
        theme.value
    };
    let lines = vec![
        pair(
            "scanned",
            fmt_count(app.scanned),
            "done",
            fmt_count(app.done),
            theme.value,
        ),
        pair(
            "queued",
            fmt_count(app.queued),
            "failed",
            fmt_count(app.failed),
            failed_style,
        ),
        pair(
            "avg llm",
            avg(app.avg_llm()),
            "avg total",
            avg(app.avg_total()),
            theme.value,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_in_flight(frame: &mut Frame, area: Rect, app: &App, now: Instant, theme: &Theme) {
    let block = boxed("in flight", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line<'static>> = app
        .in_flight
        .iter()
        .map(|f| {
            Line::from(vec![
                Span::styled("● ", theme.accent),
                Span::styled(format!("{:<20}", truncate(&f.name, 20)), theme.name),
                Span::styled(
                    format!("{:<14}", format!("{}…", f.stage.label())),
                    theme.info,
                ),
                Span::styled(
                    fmt_secs(now.saturating_duration_since(f.started_at)),
                    theme.duration,
                ),
            ])
        })
        .collect();
    if lines.is_empty() {
        let text = match app.run_state {
            RunState::Idle => "press s to start",
            RunState::Paused => "paused",
            RunState::Finished => "finished",
            RunState::Error(_) => "stopped",
            RunState::Running => "waiting for the first asset…",
        };
        lines.push(Line::from(Span::styled(text, theme.dim)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn log_line(row: &LogRow, width: usize, theme: &Theme) -> Line<'static> {
    // Fixed columns: time(8) + 2 + mark(1) + 1 + name(16) + 2 + took(6) + 2 = 38.
    let text_width = width.saturating_sub(38);
    match row {
        LogRow::Done {
            at,
            name,
            took,
            description,
        } => Line::from(vec![
            Span::styled(at.clone(), theme.dim),
            Span::styled("  ✓ ", theme.ok),
            Span::styled(format!("{:<16}", truncate(name, 16)), theme.name),
            Span::styled(format!("  {:>6}", fmt_secs(*took)), theme.duration),
            Span::styled(
                format!("  {}", truncate(description, text_width)),
                theme.value,
            ),
        ]),
        LogRow::Failed { at, name, error } => Line::from(vec![
            Span::styled(at.clone(), theme.dim),
            Span::styled("  ✗ ", theme.err),
            Span::styled(format!("{:<16}", truncate(name, 16)), theme.name),
            Span::styled("        ", theme.dim),
            Span::styled(format!("  {}", truncate(error, text_width)), theme.err),
        ]),
    }
}

fn render_log(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = boxed("log", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let items: Vec<ListItem<'static>> = app
        .log
        .iter()
        .map(|row| ListItem::new(log_line(row, inner.width as usize, theme)))
        .collect();
    let mut state = ListState::default();
    if !app.log.is_empty() {
        state.select(Some(app.log_selected.min(app.log.len() - 1)));
    }
    let list = List::new(items).highlight_style(theme.highlight);
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let can_start = matches!(
        app.run_state,
        RunState::Idle | RunState::Finished | RunState::Error(_)
    );
    let pause_label = if app.run_state == RunState::Paused {
        "resume"
    } else {
        "pause"
    };
    let can_pause = matches!(app.run_state, RunState::Running | RunState::Paused);
    let key = |k: &str, label: &str, enabled: bool| {
        let (ks, ls) = if enabled {
            (theme.accent, theme.label)
        } else {
            (theme.dim, theme.dim)
        };
        vec![
            Span::styled(format!(" {k} "), ks),
            Span::styled(format!("{label}   "), ls),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(key("s", "start", can_start));
    spans.extend(key("p", pause_label, can_pause));
    spans.extend(key("↑↓", "scroll log", !app.log.is_empty()));
    spans.extend(key("enter", "expand", !app.log.is_empty()));
    spans.extend(key("c", "settings", true));
    spans.extend(key("q", "quit", true));
    if let Some(msg) = &app.footer_message {
        spans.push(Span::styled(format!("  {msg}"), theme.info));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_popup(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(row) = app.log.get(app.log_selected) else {
        return;
    };
    let [v] = Layout::vertical([Constraint::Percentage(50)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(v);
    let (title, body, style) = match row {
        LogRow::Done {
            name, description, ..
        } => (name.clone(), description.clone(), theme.value),
        LogRow::Failed { name, error, .. } => (name.clone(), error.clone(), theme.err),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.accent)
        .title(Span::styled(format!(" {title} "), theme.title))
        .title(Span::styled(" esc close ", theme.dim).into_right_aligned_line());
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(Span::styled(body, style)).wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_tiny(frame: &mut Frame, area: Rect, app: &App, _now: Instant, theme: &Theme) {
    let [bar_area, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let counts = format!(
        " {} / {}",
        fmt_count(app.done + app.failed),
        fmt_count(app.queued)
    );
    let mut spans = bar(
        (bar_area.width as usize).saturating_sub(counts.len() + 1),
        app.progress_ratio(),
        theme,
    );
    spans.push(Span::styled(counts, theme.value));
    frame.render_widget(Paragraph::new(Line::from(spans)), bar_area);
    render_footer(frame, footer, app, theme);
}

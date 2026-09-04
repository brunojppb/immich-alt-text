//! Draws `App`. Render only, no state changes.

mod run;
mod settings;

use std::time::{Duration, Instant};

use ratatui::Frame;

use crate::app::{App, Screen};
use crate::theme::Theme;

/// Draws the current screen into the whole frame.
pub fn render(frame: &mut Frame, app: &App, now: Instant, theme: &Theme) {
    match app.screen {
        Screen::Run => run::render(frame, app, now, theme),
        Screen::Settings => settings::render(frame, app, theme),
    }
}

/// `HH:MM:SS`.
pub fn fmt_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// `4.3 s`.
pub fn fmt_secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

/// `1 284`, groups of three separated by a space.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Cuts `s` to `max` cells and appends `…` when it had to cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_clock(Duration::from_secs(6_137)), "01:42:17");
        assert_eq!(fmt_secs(Duration::from_millis(4_321)), "4.3 s");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_284), "1 284");
        assert_eq!(fmt_count(14_920), "14 920");
        assert_eq!(fmt_count(1_000_000), "1 000 000");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
    }
}

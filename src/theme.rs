//! Colors in one place. `btop` is the default look, `mono` turns color off.

use ratatui::style::{Color, Modifier, Style};

use crate::config::ThemeName;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub border: Style,
    pub title: Style,
    pub label: Style,
    pub value: Style,
    pub ok: Style,
    pub err: Style,
    pub warn: Style,
    pub info: Style,
    pub accent: Style,
    pub name: Style,
    pub duration: Style,
    pub dim: Style,
    pub highlight: Style,
    pub bar_empty: Style,
    gradient: bool,
}

impl Theme {
    pub fn from_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Btop => Self::btop(),
            ThemeName::Mono => Self::mono(),
        }
    }

    pub fn btop() -> Self {
        let dim = Style::default().fg(Color::Indexed(243));
        Self {
            border: Style::default().fg(Color::Indexed(240)),
            title: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            label: dim,
            value: Style::default().fg(Color::White),
            ok: Style::default().fg(Color::Indexed(114)),
            err: Style::default().fg(Color::Indexed(203)),
            warn: Style::default().fg(Color::Indexed(221)),
            info: Style::default().fg(Color::Indexed(81)),
            accent: Style::default().fg(Color::Indexed(176)),
            name: Style::default().fg(Color::Indexed(81)),
            duration: Style::default().fg(Color::Indexed(221)),
            dim,
            highlight: Style::default().add_modifier(Modifier::REVERSED),
            bar_empty: Style::default().fg(Color::Indexed(238)),
            gradient: true,
        }
    }

    pub fn mono() -> Self {
        let plain = Style::default();
        Self {
            border: plain,
            title: plain.add_modifier(Modifier::BOLD),
            label: plain,
            value: plain,
            ok: plain,
            err: plain,
            warn: plain,
            info: plain,
            accent: plain.add_modifier(Modifier::BOLD),
            name: plain,
            duration: plain,
            dim: plain.add_modifier(Modifier::DIM),
            highlight: plain.add_modifier(Modifier::REVERSED),
            bar_empty: plain,
            gradient: false,
        }
    }

    /// Style for the run-state word in the header.
    pub fn state_style(&self, label: &str) -> Style {
        match label {
            "RUNNING" => self.ok,
            "PAUSED" => self.warn,
            "ERROR" => self.err,
            _ => self.info,
        }
        .add_modifier(Modifier::BOLD)
    }

    /// Color of one bar cell at position `t` in 0.0..=1.0, green through yellow to red.
    pub fn bar_color(&self, t: f64) -> Color {
        if !self.gradient {
            return Color::Reset;
        }
        const STOPS: [u8; 10] = [46, 82, 118, 154, 190, 226, 220, 214, 208, 196];
        let i = (t.clamp(0.0, 1.0) * (STOPS.len() - 1) as f64).round() as usize;
        Color::Indexed(STOPS[i])
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_selects_the_expected_variant() {
        let btop = Theme::from_name(ThemeName::Btop);
        assert_eq!(btop.border, Theme::btop().border);
        assert_eq!(btop.ok, Theme::btop().ok);

        let mono = Theme::from_name(ThemeName::Mono);
        assert_eq!(mono.border, Theme::mono().border);
        assert_eq!(mono.accent, Theme::mono().accent);
    }

    #[test]
    fn mono_turns_colors_off() {
        let theme = Theme::mono();
        let plain = Style::default();

        assert_eq!(theme.border, plain);
        assert_eq!(theme.label, plain);
        assert_eq!(theme.value, plain);
        assert_eq!(theme.ok, plain);
        assert_eq!(theme.err, plain);
        assert_eq!(theme.warn, plain);
        assert_eq!(theme.info, plain);
        assert_eq!(theme.name, plain);
        assert_eq!(theme.duration, plain);
        assert_eq!(theme.bar_empty, plain);
        assert_eq!(theme.title, plain.add_modifier(Modifier::BOLD));
        assert_eq!(theme.accent, plain.add_modifier(Modifier::BOLD));
        assert_eq!(theme.dim, plain.add_modifier(Modifier::DIM));
        assert_eq!(theme.highlight, plain.add_modifier(Modifier::REVERSED));
        assert_eq!(theme.bar_color(0.5), Color::Reset);
    }

    #[test]
    fn state_style_maps_status_words_to_expected_styles() {
        let theme = Theme::btop();

        assert_eq!(
            theme.state_style("RUNNING"),
            theme.ok.add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            theme.state_style("PAUSED"),
            theme.warn.add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            theme.state_style("ERROR"),
            theme.err.add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            theme.state_style("IDLE"),
            theme.info.add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            theme.state_style("FINISHED"),
            theme.info.add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn bar_color_uses_gradient_stops_and_clamps_input() {
        let theme = Theme::btop();

        assert_eq!(theme.bar_color(-1.0), Color::Indexed(46));
        assert_eq!(theme.bar_color(0.0), Color::Indexed(46));
        assert_eq!(theme.bar_color(0.5), Color::Indexed(226));
        assert_eq!(theme.bar_color(1.0), Color::Indexed(196));
        assert_eq!(theme.bar_color(2.0), Color::Indexed(196));
    }
}

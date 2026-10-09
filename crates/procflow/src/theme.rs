//! Colour themes for the interactive view: the four Catppuccin flavours.
//!
//! A theme names colours by the role they play, and every role is held to a
//! contrast floor against the theme's background (see the test below). To
//! add a theme, add an entry to [`THEMES`].

use ratatui::style::Color;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// The background. Painted behind everything unless the view is
    /// transparent, and the text colour on the selection bar.
    pub base: Color,
    pub text: Color,
    /// Secondary columns, labels, table headers.
    pub subtext: Color,
    /// Section headings, hints, placeholders, idle rows.
    pub muted: Color,
    pub border: Color,
    /// Border of the focused pane.
    pub focus: Color,
    /// Titles, key names, and the selection bar.
    pub accent: Color,
    pub ingress: Color,
    pub egress: Color,
    /// The table's sparklines and share gauges.
    pub trend: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Catppuccin palette v1.8.0 (https://catppuccin.com/palette), first one is
/// the default. Which palette colour fills a role differs per flavour where
/// the usual choice would miss the contrast floor: Frappé's Overlay 0 is too
/// dim for a border, and most of Latte's bright hues are too pale on its
/// light background, so Latte leans on Mauve, Blue, Maroon and Teal.
pub const THEMES: [Theme; 4] = [
    Theme {
        name: "mocha",
        base: rgb(0x1e1e2e),
        text: rgb(0xcdd6f4),
        subtext: rgb(0xa6adc8), // Subtext 0
        muted: rgb(0x9399b2),   // Overlay 2
        border: rgb(0x6c7086),  // Overlay 0
        focus: rgb(0xb4befe),   // Lavender
        accent: rgb(0xcba6f7),  // Mauve
        ingress: rgb(0x89b4fa), // Blue
        egress: rgb(0xfab387),  // Peach
        trend: rgb(0x94e2d5),   // Teal
        good: rgb(0xa6e3a1),    // Green
        warn: rgb(0xf9e2af),    // Yellow
        bad: rgb(0xf38ba8),     // Red
    },
    Theme {
        name: "macchiato",
        base: rgb(0x24273a),
        text: rgb(0xcad3f5),
        subtext: rgb(0xa5adcb), // Subtext 0
        muted: rgb(0x939ab7),   // Overlay 2
        border: rgb(0x6e738d),  // Overlay 0
        focus: rgb(0xb7bdf8),   // Lavender
        accent: rgb(0xc6a0f6),  // Mauve
        ingress: rgb(0x8aadf4), // Blue
        egress: rgb(0xf5a97f),  // Peach
        trend: rgb(0x8bd5ca),   // Teal
        good: rgb(0xa6da95),    // Green
        warn: rgb(0xeed49f),    // Yellow
        bad: rgb(0xed8796),     // Red
    },
    Theme {
        name: "frappe",
        base: rgb(0x303446),
        text: rgb(0xc6d0f5),
        subtext: rgb(0xa5adce), // Subtext 0
        muted: rgb(0x949cbb),   // Overlay 2
        border: rgb(0x838ba7),  // Overlay 1
        focus: rgb(0xbabbf1),   // Lavender
        accent: rgb(0xca9ee6),  // Mauve
        ingress: rgb(0x8caaee), // Blue
        egress: rgb(0xef9f76),  // Peach
        trend: rgb(0x81c8be),   // Teal
        good: rgb(0xa6d189),    // Green
        warn: rgb(0xe5c890),    // Yellow
        bad: rgb(0xe78284),     // Red
    },
    Theme {
        name: "latte",
        base: rgb(0xeff1f5),
        text: rgb(0x4c4f69),
        subtext: rgb(0x5c5f77), // Subtext 1
        muted: rgb(0x5c5f77),   // Subtext 1
        border: rgb(0x7c7f93),  // Overlay 2
        focus: rgb(0x8839ef),   // Mauve
        accent: rgb(0x8839ef),  // Mauve
        ingress: rgb(0x1e66f5), // Blue
        egress: rgb(0xe64553),  // Maroon
        trend: rgb(0x179299),   // Teal
        good: rgb(0x179299),    // Teal
        warn: rgb(0xe64553),    // Maroon
        bad: rgb(0xd20f39),     // Red
    },
];

/// The names of [`THEMES`], for help and error text.
pub const NAMES: &str = "mocha|macchiato|frappe|latte";

/// Position in [`THEMES`] of the theme called `name`.
pub fn index(name: &str) -> Option<usize> {
    THEMES.iter().position(|theme| theme.name == name)
}

impl Theme {
    /// The theme for a view that leaves the terminal's own background in
    /// place. Whatever shows through is unknown and often lighter than
    /// `base`, so every neutral moves one step brighter. On a busy
    /// wallpaper that still falls short of the painted background.
    pub fn see_through(self) -> Theme {
        Theme {
            subtext: self.text,
            muted: self.subtext,
            border: self.muted,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2 relative luminance.
    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("themes use RGB colours")
        };
        let linear = |channel: u8| {
            let c = channel as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    /// WCAG 2 contrast ratio, from 1 (none) to 21.
    fn contrast(a: Color, b: Color) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn every_role_is_legible_on_its_background() {
        for theme in THEMES {
            // WCAG AA asks 4.5 of body text, and 3 of large text and of
            // non-text marks such as borders and chart bars.
            let floors = [
                ("text", theme.text, 7.0),
                ("subtext", theme.subtext, 4.5),
                ("muted", theme.muted, 4.5),
                ("border", theme.border, 3.0),
                ("focus", theme.focus, 3.0),
                ("accent", theme.accent, 4.5),
                ("ingress", theme.ingress, 3.0),
                ("egress", theme.egress, 3.0),
                ("trend", theme.trend, 3.0),
                ("good", theme.good, 3.0),
                ("warn", theme.warn, 3.0),
                ("bad", theme.bad, 3.0),
            ];
            for (role, color, floor) in floors {
                let ratio = contrast(color, theme.base);
                assert!(
                    ratio >= floor,
                    "{} {role}: {ratio:.2} is below {floor}",
                    theme.name
                );
            }
            // Only brighter, never dimmer, without the background.
            let lifted = theme.see_through();
            for (role, before, after) in [
                ("subtext", theme.subtext, lifted.subtext),
                ("muted", theme.muted, lifted.muted),
                ("border", theme.border, lifted.border),
            ] {
                assert!(
                    contrast(after, theme.base) >= contrast(before, theme.base),
                    "{} {role} got dimmer",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn themes_are_found_by_name() {
        assert_eq!(index("mocha"), Some(0));
        assert_eq!(index("latte"), Some(3));
        assert_eq!(index("solarized"), None);
        let names: Vec<&str> = THEMES.iter().map(|theme| theme.name).collect();
        assert_eq!(names.join("|"), NAMES);
    }
}

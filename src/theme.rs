//! Terminal colors resolved once, without changing the user's terminal palette.

use ratatui::style::Color;
use std::fmt;
use std::str::FromStr;

/// A configured ANSI, indexed, or RGB accent color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeColor(Color);

impl Default for ThemeColor {
    fn default() -> Self {
        Self(Color::Cyan)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseColorError;

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a terminal color name, an index from 0 to 255, or #RRGGBB")
    }
}

impl std::error::Error for ParseColorError {}

impl FromStr for ThemeColor {
    type Err = ParseColorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.starts_with('#') {
            let bytes = value.as_bytes();
            if bytes.len() != 7 {
                return Err(ParseColorError);
            }
            let mut channels = [0; 3];
            for (channel, pair) in channels.iter_mut().zip(bytes[1..].as_chunks::<2>().0) {
                *channel = hex_digit(pair[0])? * 16 + hex_digit(pair[1])?;
            }
            return Ok(Self(Color::Rgb(channels[0], channels[1], channels[2])));
        }
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            return value
                .parse::<u8>()
                .map(|index| Self(Color::Indexed(index)))
                .map_err(|_| ParseColorError);
        }
        // Accept common spelling variants without allocating a normalized string.
        let aliases = [
            ("black", Color::Black),
            ("red", Color::Red),
            ("green", Color::Green),
            ("yellow", Color::Yellow),
            ("blue", Color::Blue),
            ("magenta", Color::Magenta),
            ("cyan", Color::Cyan),
            ("gray", Color::Gray),
            ("grey", Color::Gray),
            ("darkgray", Color::DarkGray),
            ("darkgrey", Color::DarkGray),
            ("brightblack", Color::DarkGray),
            ("brightred", Color::LightRed),
            ("brightgreen", Color::LightGreen),
            ("brightyellow", Color::LightYellow),
            ("brightblue", Color::LightBlue),
            ("brightmagenta", Color::LightMagenta),
            ("brightcyan", Color::LightCyan),
            ("brightwhite", Color::White),
            ("lightred", Color::LightRed),
            ("lightgreen", Color::LightGreen),
            ("lightyellow", Color::LightYellow),
            ("lightblue", Color::LightBlue),
            ("lightmagenta", Color::LightMagenta),
            ("lightcyan", Color::LightCyan),
            ("white", Color::White),
        ];
        aliases
            .iter()
            .find(|(name, _)| {
                value
                    .bytes()
                    .filter(|byte| !matches!(*byte, b'-' | b'_'))
                    .map(|byte| byte.to_ascii_lowercase())
                    .eq(name.bytes())
            })
            .map(|(_, color)| Self(*color))
            .ok_or(ParseColorError)
    }
}

fn hex_digit(byte: u8) -> Result<u8, ParseColorError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ParseColorError),
    }
}

/// Resolved colors used by the browser and guarded deletion prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub accent: Color,
    pub warning_background: Color,
    pub warning_dialog_background: Color,
    pub warning_foreground: Color,
}

impl Theme {
    pub fn detect(accent: ThemeColor) -> Self {
        let colorterm = std::env::var("COLORTERM").ok();
        let term = std::env::var("TERM").ok();
        Self::with_support(
            accent,
            ColorSupport::detect(colorterm.as_deref(), term.as_deref()),
        )
    }

    fn with_support(accent: ThemeColor, support: ColorSupport) -> Self {
        // Use explicit fallback reds: rounding a dim RGB red to a palette can
        // select black, hiding the warning altogether.
        let (warning_background, warning_dialog_background) = match support {
            ColorSupport::TrueColor => (Color::Rgb(36, 12, 12), Color::Rgb(64, 16, 16)),
            ColorSupport::Indexed => (Color::Indexed(52), Color::Indexed(88)),
            ColorSupport::Ansi => (Color::Red, Color::Red),
        };
        Self {
            accent: support.resolve(accent.0),
            warning_background,
            warning_dialog_background,
            warning_foreground: Color::White,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorSupport {
    Ansi,
    Indexed,
    TrueColor,
}

impl ColorSupport {
    fn detect(colorterm: Option<&str>, term: Option<&str>) -> Self {
        let colorterm = colorterm.unwrap_or_default().trim();
        let term = term.unwrap_or_default().trim();
        let explicit_rgb =
            colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit");
        let direct_term = ["-direct", "-truecolor", "-24bit"].iter().any(|suffix| {
            term.get(term.len().saturating_sub(suffix.len())..)
                .is_some_and(|end| end.eq_ignore_ascii_case(suffix))
        });
        let known_rgb = [
            "xterm-kitty",
            "xterm-ghostty",
            "alacritty",
            "foot",
            "foot-extra",
            "wezterm",
        ]
        .iter()
        .any(|known| term.eq_ignore_ascii_case(known));
        if explicit_rgb || direct_term || known_rgb {
            Self::TrueColor
        } else if term
            .as_bytes()
            .windows(b"256color".len())
            .any(|part| part.eq_ignore_ascii_case(b"256color"))
        {
            Self::Indexed
        } else {
            Self::Ansi
        }
    }

    fn resolve(self, color: Color) -> Color {
        match (self, color) {
            (Self::TrueColor, _) => color,
            (Self::Indexed, Color::Rgb(red, green, blue)) => {
                // The first 16 entries are user's theme colors; the remaining
                // xterm cube and grayscale entries have standardized RGB values.
                Color::Indexed(nearest((red, green, blue), 16, 255))
            }
            (Self::Ansi, Color::Rgb(red, green, blue)) => {
                ANSI_COLORS[usize::from(nearest((red, green, blue), 0, 15))]
            }
            (Self::Ansi, Color::Indexed(index)) => {
                let index = if index < 16 {
                    index
                } else {
                    nearest(palette_rgb(index), 0, 15)
                };
                ANSI_COLORS[usize::from(index)]
            }
            _ => color,
        }
    }
}

const ANSI_COLORS: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

// Reference ANSI colors. A terminal may customize these entries; without
// changing or querying its global palette this is necessarily an approximation.
const ANSI_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn palette_rgb(index: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => ANSI_RGB[usize::from(index)],
        16..=231 => {
            let cube = usize::from(index - 16);
            (LEVELS[cube / 36], LEVELS[cube / 6 % 6], LEVELS[cube % 6])
        }
        _ => {
            let gray = 8 + 10 * (index - 232);
            (gray, gray, gray)
        }
    }
}

fn nearest(rgb: (u8, u8, u8), start: u8, end: u8) -> u8 {
    let mut result = start;
    let mut best = u32::MAX;
    for index in start..=end {
        let candidate = palette_rgb(index);
        let distance = u32::from(rgb.0.abs_diff(candidate.0)).pow(2)
            + u32::from(rgb.1.abs_diff(candidate.1)).pow(2)
            + u32::from(rgb.2.abs_diff(candidate.2)).pow(2);
        if distance < best {
            best = distance;
            result = index;
            if distance == 0 {
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_names_indices_and_hex_without_ambiguity() {
        for (text, expected) in [
            ("cyan", Color::Cyan),
            (" BRIGHT-RED ", Color::LightRed),
            ("light_blue", Color::LightBlue),
            ("dark-grey", Color::DarkGray),
            ("gray", Color::Gray),
            ("white", Color::White),
            ("0", Color::Indexed(0)),
            ("255", Color::Indexed(255)),
            ("#aB019F", Color::Rgb(171, 1, 159)),
        ] {
            assert_eq!(
                text.parse::<ThemeColor>(),
                Ok(ThemeColor(expected)),
                "{text}"
            );
        }
        assert_eq!(ThemeColor::default(), ThemeColor(Color::Cyan));
    }

    #[test]
    fn rejects_invalid_colors_including_non_ascii_hex_without_panicking() {
        for text in [
            "",
            "default",
            "256",
            "-1",
            "+1",
            "1.0",
            "#fff",
            "#1234567",
            "#gg0000",
            "#é0000",
            "#000é0",
            "#0000é",
            "rgb(1,2,3)",
            "cyan red",
        ] {
            assert_eq!(text.parse::<ThemeColor>(), Err(ParseColorError), "{text}");
        }
    }

    #[test]
    fn recognizes_explicit_and_known_truecolor_terminals() {
        for (colorterm, term) in [
            (Some("truecolor"), Some("xterm-256color")),
            (Some("24BIT"), None),
            (None, Some("xterm-direct")),
            (None, Some("tmux-direct")),
            (None, Some("xterm-truecolor")),
            (None, Some("xterm-kitty")),
            (None, Some("xterm-ghostty")),
            (None, Some("foot")),
            (None, Some("alacritty")),
            (None, Some("wezterm")),
        ] {
            assert_eq!(
                ColorSupport::detect(colorterm, term),
                ColorSupport::TrueColor
            );
        }
    }

    #[test]
    fn unknown_terminal_capabilities_use_bounded_palette_fallbacks() {
        assert_eq!(ColorSupport::detect(None, None), ColorSupport::Ansi);
        assert_eq!(
            ColorSupport::detect(Some("yes"), Some("xterm")),
            ColorSupport::Ansi
        );
        assert_eq!(
            ColorSupport::detect(None, Some("screen")),
            ColorSupport::Ansi
        );
        assert_eq!(ColorSupport::detect(None, Some("é")), ColorSupport::Ansi);
        assert_eq!(
            ColorSupport::detect(None, Some("xterm-256color")),
            ColorSupport::Indexed
        );
        assert_eq!(
            ColorSupport::detect(None, Some("SCREEN-256COLOR")),
            ColorSupport::Indexed
        );
    }

    #[test]
    fn rgb_uses_truecolor_or_the_nearest_supported_palette() {
        for (rgb, indexed, ansi) in [
            (Color::Rgb(255, 0, 0), Color::Indexed(196), Color::LightRed),
            (
                Color::Rgb(128, 128, 128),
                Color::Indexed(244),
                Color::DarkGray,
            ),
            (Color::Rgb(0, 0, 0), Color::Indexed(16), Color::Black),
            (Color::Rgb(255, 255, 255), Color::Indexed(231), Color::White),
        ] {
            assert_eq!(ColorSupport::TrueColor.resolve(rgb), rgb);
            assert_eq!(ColorSupport::Indexed.resolve(rgb), indexed);
            assert_eq!(ColorSupport::Ansi.resolve(rgb), ansi);
        }
    }

    #[test]
    fn ansi_names_preserve_terminal_palette_and_indices_fall_back_on_ansi() {
        for support in [
            ColorSupport::Ansi,
            ColorSupport::Indexed,
            ColorSupport::TrueColor,
        ] {
            assert_eq!(support.resolve(Color::Cyan), Color::Cyan);
            assert_eq!(support.resolve(Color::LightRed), Color::LightRed);
        }
        for (index, color) in ANSI_COLORS.iter().enumerate() {
            assert_eq!(
                ColorSupport::Ansi.resolve(Color::Indexed(index as u8)),
                *color
            );
        }
        assert_eq!(
            ColorSupport::Indexed.resolve(Color::Indexed(197)),
            Color::Indexed(197)
        );
        assert_eq!(
            ColorSupport::TrueColor.resolve(Color::Indexed(197)),
            Color::Indexed(197)
        );
        assert_eq!(
            ColorSupport::Ansi.resolve(Color::Indexed(197)),
            Color::LightRed
        );
    }

    #[test]
    fn warning_stays_red_and_readable_even_when_dim_rgb_would_round_to_black() {
        for (support, background, dialog) in [
            (ColorSupport::Ansi, Color::Red, Color::Red),
            (
                ColorSupport::Indexed,
                Color::Indexed(52),
                Color::Indexed(88),
            ),
            (
                ColorSupport::TrueColor,
                Color::Rgb(36, 12, 12),
                Color::Rgb(64, 16, 16),
            ),
        ] {
            let theme = Theme::with_support(ThemeColor::default(), support);
            assert_eq!(theme.warning_background, background);
            assert_eq!(theme.warning_dialog_background, dialog);
            assert_eq!(theme.warning_foreground, Color::White);
            assert_ne!(theme.warning_background, theme.warning_foreground);
            assert_ne!(theme.warning_dialog_background, theme.warning_foreground);
        }
    }
}

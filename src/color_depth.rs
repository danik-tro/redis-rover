//! What the terminal can show, and the one place a finished frame's colours
//! are converted to it.

use ratatui::{buffer::Buffer, style::Color};

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit colour: the palette as written.
    #[default]
    TrueColor,
    /// The xterm 256-colour palette.
    Ansi256,
    /// The 16 ANSI colours.
    Ansi16,
    /// No colour (`NO_COLOR`).
    Monochrome,
}

/// The 16 ANSI colours with their usual xterm values.
const ANSI: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 0, 0)),
    (Color::Green, (0, 205, 0)),
    (Color::Yellow, (205, 205, 0)),
    (Color::Blue, (0, 0, 238)),
    (Color::Magenta, (205, 0, 205)),
    (Color::Cyan, (0, 205, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (127, 127, 127)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (92, 92, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

/// Channel values of the 6×6×6 cube at indices 16..=231.
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let channel = |x: u8, y: u8| u32::from(x.abs_diff(y)).pow(2);
    channel(a.0, b.0) + channel(a.1, b.1) + channel(a.2, b.2)
}

/// The RGB value of a 256-palette index.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI[usize::from(index)].1,
        16..=231 => {
            let i = usize::from(index - 16);
            (CUBE[i / 36], CUBE[i / 6 % 6], CUBE[i % 6])
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232);
            (level, level, level)
        }
    }
}

/// The closest 256-palette index, from the cube and the grey ramp (not from
/// 0..=15, which themes and terminals redefine).
fn nearest_256(rgb: (u8, u8, u8)) -> u8 {
    let level = |channel: u8| -> u8 {
        (0..6u8)
            .min_by_key(|&i| CUBE[usize::from(i)].abs_diff(channel))
            .unwrap_or(0)
    };
    let cube = 16 + 36 * level(rgb.0) + 6 * level(rgb.1) + level(rgb.2);
    let average = (u16::from(rgb.0) + u16::from(rgb.1) + u16::from(rgb.2)) / 3;
    let step = (average.saturating_sub(3) / 10).min(23);
    let grey = 232 + u8::try_from(step).unwrap_or(23);
    if distance(rgb, indexed_rgb(grey)) < distance(rgb, indexed_rgb(cube)) {
        grey
    } else {
        cube
    }
}

fn nearest_ansi(rgb: (u8, u8, u8)) -> Color {
    ANSI.iter()
        .min_by_key(|(_, value)| distance(rgb, *value))
        .map_or(Color::Reset, |(color, _)| *color)
}

impl ColorDepth {
    /// Detect the depth from environment variables, read through `var`:
    /// a non-empty `NO_COLOR` wins, then `COLORTERM`, then `TERM`.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Self {
        if var("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return Self::Monochrome;
        }
        let colorterm = var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" {
            return Self::TrueColor;
        }
        if var("TERM").is_some_and(|term| term.contains("256")) {
            return Self::Ansi256;
        }
        Self::Ansi16
    }

    /// `color` as this depth shows it.
    #[must_use]
    pub fn map(self, color: Color) -> Color {
        match (self, color) {
            (Self::TrueColor, _) | (_, Color::Reset) => color,
            (Self::Monochrome, _) => Color::Reset,
            (Self::Ansi256, Color::Rgb(r, g, b)) => Color::Indexed(nearest_256((r, g, b))),
            (Self::Ansi16, Color::Rgb(r, g, b)) => nearest_ansi((r, g, b)),
            (Self::Ansi16, Color::Indexed(index)) => nearest_ansi(indexed_rgb(index)),
            (Self::Ansi256 | Self::Ansi16, named) => named,
        }
    }

    /// Convert every cell of a finished frame. Costs nothing on truecolor.
    pub fn apply(self, buf: &mut Buffer) {
        if self == Self::TrueColor {
            return;
        }
        for cell in &mut buf.content {
            cell.fg = self.map(cell.fg);
            cell.bg = self.map(cell.bg);
            cell.underline_color = self.map(cell.underline_color);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{
        backend::{Backend, CrosstermBackend},
        layout::Rect,
        style::Style,
    };

    use super::*;

    fn detect(vars: &[(&str, &str)]) -> ColorDepth {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        ColorDepth::detect(|name| vars.get(name).map(|v| (*v).to_owned()))
    }

    #[test]
    fn detects_the_depth_from_the_environment() {
        for (vars, expected) in [
            (vec![("COLORTERM", "truecolor")], ColorDepth::TrueColor),
            (vec![("COLORTERM", "24bit")], ColorDepth::TrueColor),
            (vec![("COLORTERM", "TrueColor")], ColorDepth::TrueColor),
            (vec![("TERM", "xterm-256color")], ColorDepth::Ansi256),
            (vec![("TERM", "screen-256color")], ColorDepth::Ansi256),
            (vec![("TERM", "xterm")], ColorDepth::Ansi16),
            (vec![("TERM", "dumb")], ColorDepth::Ansi16),
            (vec![], ColorDepth::Ansi16),
            (
                vec![("COLORTERM", "truecolor"), ("TERM", "xterm-256color")],
                ColorDepth::TrueColor,
            ),
            (
                vec![("NO_COLOR", "1"), ("COLORTERM", "truecolor")],
                ColorDepth::Monochrome,
            ),
            (
                vec![("NO_COLOR", ""), ("TERM", "xterm-256color")],
                ColorDepth::Ansi256,
            ),
        ] {
            assert_eq!(detect(&vars), expected, "{vars:?}");
        }
    }

    #[test]
    fn maps_to_the_nearest_palette_entry() {
        assert_eq!(
            ColorDepth::Ansi256.map(Color::Rgb(255, 0, 0)),
            Color::Indexed(196)
        );
        assert_eq!(
            ColorDepth::Ansi256.map(Color::Rgb(128, 128, 128)),
            Color::Indexed(244)
        );
        assert_eq!(
            ColorDepth::Ansi16.map(Color::Rgb(250, 5, 5)),
            Color::LightRed
        );
        assert_eq!(ColorDepth::Ansi16.map(Color::Indexed(196)), Color::LightRed);
        assert_eq!(ColorDepth::Ansi16.map(Color::Rgb(20, 20, 30)), Color::Black);
    }

    /// The fast lookup agrees with trying all 240 entries.
    #[test]
    fn the_256_lookup_finds_the_closest_entry() {
        for r in (0..=255u8).step_by(15) {
            for g in (0..=255u8).step_by(15) {
                for b in (0..=255u8).step_by(15) {
                    let rgb = (r, g, b);
                    let best = (16..=255u8)
                        .map(|index| distance(rgb, indexed_rgb(index)))
                        .min()
                        .unwrap();
                    assert_eq!(
                        distance(rgb, indexed_rgb(nearest_256(rgb))),
                        best,
                        "{rgb:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn keeps_what_the_depth_can_already_show() {
        let rgb = Color::Rgb(1, 2, 3);
        assert_eq!(ColorDepth::TrueColor.map(rgb), rgb);
        assert_eq!(
            ColorDepth::Ansi256.map(Color::Indexed(42)),
            Color::Indexed(42)
        );
        assert_eq!(ColorDepth::Ansi256.map(Color::Red), Color::Red);
        assert_eq!(ColorDepth::Ansi16.map(Color::Red), Color::Red);
        assert_eq!(ColorDepth::Ansi16.map(Color::Reset), Color::Reset);
    }

    #[test]
    fn monochrome_drops_every_colour() {
        for color in [Color::Rgb(9, 9, 9), Color::Indexed(9), Color::Red] {
            assert_eq!(ColorDepth::Monochrome.map(color), Color::Reset);
        }
    }

    #[test]
    fn neighbouring_cells_keep_their_own_colours() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].set_fg(Color::Rgb(255, 0, 0));
        buf[(1, 0)].set_fg(Color::Rgb(0, 255, 0));
        buf[(2, 0)].set_fg(Color::Rgb(255, 0, 0));

        ColorDepth::Ansi16.apply(&mut buf);

        assert_eq!(buf[(0, 0)].fg, Color::LightRed);
        assert_eq!(buf[(1, 0)].fg, Color::LightGreen);
        assert_eq!(buf[(2, 0)].fg, Color::LightRed);
    }

    /// AC3: on 256 colours the terminal gets no truecolor escape sequence.
    #[test]
    fn a_256_colour_frame_has_no_truecolor_escapes() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(
            0,
            0,
            "text",
            Style::new()
                .fg(Color::Rgb(233, 233, 244))
                .bg(Color::Rgb(40, 41, 54)),
        );

        ColorDepth::Ansi256.apply(&mut buf);
        let mut output = Vec::new();
        let mut backend = CrosstermBackend::new(&mut output);
        let cells = buf
            .content
            .iter()
            .enumerate()
            .map(|(i, cell)| (u16::try_from(i).unwrap(), 0, cell));
        backend.draw(cells).unwrap();
        Backend::flush(&mut backend).unwrap();

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("38;5;"), "{output:?}");
        assert!(!output.contains("38;2;"), "{output:?}");
        assert!(!output.contains("48;2;"), "{output:?}");
    }
}

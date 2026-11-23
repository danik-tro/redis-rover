//! `ui.theme`: a built-in palette or a theme file. A token a theme leaves
//! out keeps its `dark` value.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use ratatui::style::Color;

use crate::config::{Base16Palette, KeyspaceConfig};

/// The theme that needs no changes: the bundled palette.
pub const DARK: &str = "dark";
/// The theme that also turns colour off.
pub const MONOCHROME: &str = "monochrome";

const LIGHT_TOKENS: &[(&str, &str)] = &[
    ("base00", "#fafafa"),
    ("base01", "#f0f0f1"),
    ("base02", "#d4d4d8"),
    ("base03", "#62666d"),
    ("base04", "#4f5666"),
    ("base05", "#24292f"),
    ("base06", "#1f2328"),
    ("base07", "#0d1117"),
    ("base08", "#b3124f"),
    ("base09", "#8a3fa0"),
    ("base0a", "#0f7b3f"),
    ("base0b", "#6f5300"),
    ("base0c", "#0b7285"),
    ("base0d", "#0b62c4"),
    ("base0e", "#7b3fb5"),
    ("base0f", "#0f7b3f"),
    ("env_local", "#0f7b3f"),
    ("env_staging", "#8a5d00"),
    ("env_prod", "#c62828"),
    ("env_unknown", "#a85400"),
    ("string", "#0f7b3f"),
    ("hash", "#0b62c4"),
    ("list", "#a85400"),
    ("zset", "#b3124f"),
    ("set", "#6f5300"),
    ("json", "#7b3fb5"),
    ("stream", "#0b7285"),
    ("unknown", "#c62828"),
];

const HIGH_CONTRAST_TOKENS: &[(&str, &str)] = &[
    ("base00", "#000000"),
    ("base01", "#1a1a1a"),
    ("base02", "#3a3a3a"),
    ("base03", "#c0c0c0"),
    ("base04", "#ffffff"),
    ("base05", "#ffffff"),
    ("base06", "#ffffff"),
    ("base07", "#ffffff"),
    ("base08", "#ff5f5f"),
    ("base09", "#ff87ff"),
    ("base0a", "#00ff5f"),
    ("base0b", "#ffff00"),
    ("base0c", "#00ffff"),
    ("base0d", "#5fd7ff"),
    ("base0e", "#ff87ff"),
    ("base0f", "#00ff5f"),
    ("env_local", "#00ff5f"),
    ("env_staging", "#ffff00"),
    ("env_prod", "#ff5f5f"),
    ("env_unknown", "#ffaf00"),
    ("string", "#00ff5f"),
    ("hash", "#5fd7ff"),
    ("list", "#ffaf00"),
    ("zset", "#ff87ff"),
    ("set", "#ffff00"),
    ("json", "#ff87ff"),
    ("stream", "#00ffff"),
    ("unknown", "#ff5f5f"),
];

/// Greys only. The terminal gets no colour at all with this theme
/// (`ColorDepth::Monochrome`); the steps keep highlights apart from the
/// background, which is what turns them into reverse video.
const MONOCHROME_TOKENS: &[(&str, &str)] = &[
    ("base00", "#000000"),
    ("base01", "#262626"),
    ("base02", "#444444"),
    ("base03", "#808080"),
    ("base04", "#c0c0c0"),
    ("base05", "#ffffff"),
    ("base06", "#ffffff"),
    ("base07", "#ffffff"),
    ("base08", "#ffffff"),
    ("base09", "#ffffff"),
    ("base0a", "#ffffff"),
    ("base0b", "#ffffff"),
    ("base0c", "#ffffff"),
    ("base0d", "#ffffff"),
    ("base0e", "#ffffff"),
    ("base0f", "#ffffff"),
    ("env_local", "#ffffff"),
    ("env_staging", "#ffffff"),
    ("env_prod", "#ffffff"),
    ("env_unknown", "#ffffff"),
    ("string", "#ffffff"),
    ("hash", "#ffffff"),
    ("list", "#ffffff"),
    ("zset", "#ffffff"),
    ("set", "#ffffff"),
    ("json", "#ffffff"),
    ("stream", "#ffffff"),
    ("unknown", "#ffffff"),
];

/// The colour a token names, in the palette or the key type colours.
fn slot<'a>(
    token: &str,
    colors: &'a mut Base16Palette,
    keyspace: &'a mut KeyspaceConfig,
) -> Option<&'a mut Color> {
    Some(match token {
        "base00" => &mut colors.base00,
        "base01" => &mut colors.base01,
        "base02" => &mut colors.base02,
        "base03" => &mut colors.base03,
        "base04" => &mut colors.base04,
        "base05" => &mut colors.base05,
        "base06" => &mut colors.base06,
        "base07" => &mut colors.base07,
        "base08" => &mut colors.base08,
        "base09" => &mut colors.base09,
        "base0a" => &mut colors.base0a,
        "base0b" => &mut colors.base0b,
        "base0c" => &mut colors.base0c,
        "base0d" => &mut colors.base0d,
        "base0e" => &mut colors.base0e,
        "base0f" => &mut colors.base0f,
        "env_local" => &mut colors.env_local,
        "env_staging" => &mut colors.env_staging,
        "env_prod" => &mut colors.env_prod,
        "env_unknown" => &mut colors.env_unknown,
        "string" => &mut keyspace.string,
        "hash" => &mut keyspace.hash,
        "list" => &mut keyspace.list,
        "zset" => &mut keyspace.zset,
        "set" => &mut keyspace.set,
        "json" => &mut keyspace.json,
        "stream" => &mut keyspace.stream,
        "unknown" => &mut keyspace.unknown,
        _ => return None,
    })
}

fn set_token(
    token: &str,
    value: &str,
    colors: &mut Base16Palette,
    keyspace: &mut KeyspaceConfig,
) -> Result<(), String> {
    let slot = slot(token, colors, keyspace).ok_or_else(|| format!("unknown token `{token}`"))?;
    *slot = Color::from_str(value).map_err(|e| format!("token `{token}` = `{value}`: {e}"))?;
    Ok(())
}

/// A theme file's path: as written if absolute, else under the config dir.
fn file_path(theme: &str, config_dir: &Path) -> PathBuf {
    let path = Path::new(theme);
    if path.is_absolute() {
        path.to_owned()
    } else {
        config_dir.join(path)
    }
}

/// Whether `theme` names a file rather than a built-in theme.
fn is_path(theme: &str) -> bool {
    theme.contains(['/', '\\'])
        || Path::new(theme).extension().is_some_and(|ext| {
            ["json5", "json", "yaml", "yml", "toml"]
                .iter()
                .any(|e| ext == *e)
        })
}

fn read_file(path: &Path) -> Result<HashMap<String, String>, String> {
    if !path.is_file() {
        return Err(format!("no theme file at {}", path.display()));
    }
    config::Config::builder()
        .add_source(config::File::from(path))
        .build()
        .and_then(config::Config::try_deserialize)
        .map_err(|e| format!("theme file {}: {e}", path.display()))
}

/// Apply `theme` over the `dark` values in `colors` and `keyspace`.
///
/// # Errors
///
/// An unknown theme, an unreadable theme file, or a bad token in it.
pub fn apply(
    theme: &str,
    config_dir: &Path,
    colors: &mut Base16Palette,
    keyspace: &mut KeyspaceConfig,
) -> Result<(), String> {
    let tokens = match theme {
        DARK => return Ok(()),
        "light" => LIGHT_TOKENS,
        "high-contrast" => HIGH_CONTRAST_TOKENS,
        MONOCHROME => MONOCHROME_TOKENS,
        file if is_path(file) => {
            for (token, value) in read_file(&file_path(file, config_dir))? {
                set_token(&token, &value, colors, keyspace)
                    .map_err(|e| format!("theme `{file}`: {e}"))?;
            }
            return Ok(());
        }
        other => {
            return Err(format!(
                "unknown theme `{other}`: use dark, light, high-contrast, monochrome or a file path"
            ));
        }
    };
    for (token, value) in tokens {
        set_token(token, value, colors, keyspace)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> (Base16Palette, KeyspaceConfig) {
        (Base16Palette::default(), KeyspaceConfig::default())
    }

    fn rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("{other:?} is not RGB"),
        }
    }

    /// WCAG relative luminance.
    fn luminance(color: Color) -> f64 {
        let (red, green, blue) = rgb(color);
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }

    /// WCAG contrast ratio.
    fn contrast(first: Color, second: Color) -> f64 {
        let (first, second) = (luminance(first), luminance(second));
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    fn themed(name: &str) -> (Base16Palette, KeyspaceConfig) {
        let (mut colors, mut keyspace) = dark();
        apply(name, Path::new("."), &mut colors, &mut keyspace).unwrap();
        (colors, keyspace)
    }

    #[test]
    fn built_in_themes_define_every_token() {
        for tokens in [LIGHT_TOKENS, HIGH_CONTRAST_TOKENS, MONOCHROME_TOKENS] {
            assert_eq!(tokens.len(), 28);
            let (mut colors, mut keyspace) = dark();
            for (token, value) in tokens {
                set_token(token, value, &mut colors, &mut keyspace).unwrap();
            }
        }
    }

    #[test]
    fn text_is_readable_in_every_built_in_theme() {
        for (name, body) in [("light", 4.5), ("high-contrast", 7.0), ("monochrome", 7.0)] {
            let (colors, keyspace) = themed(name);
            let bg = colors.base00;
            assert!(contrast(colors.base05, bg) >= body, "{name} text");
            assert!(contrast(colors.base03, bg) >= 4.5, "{name} dim text");
            assert!(
                contrast(colors.base05, colors.base02) >= 4.5,
                "{name} selection"
            );
            for color in [
                keyspace.string,
                keyspace.hash,
                keyspace.list,
                keyspace.zset,
                keyspace.set,
                keyspace.json,
                keyspace.stream,
                keyspace.unknown,
                colors.env_local,
                colors.env_staging,
                colors.env_prod,
                colors.env_unknown,
            ] {
                let ratio = contrast(color, bg);
                assert!(ratio >= 4.5, "{name}: {color:?} is {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn dark_changes_nothing() {
        let (colors, keyspace) = themed(DARK);
        let (want_colors, want_keyspace) = dark();
        assert_eq!(colors.base00, want_colors.base00);
        assert_eq!(keyspace.hash, want_keyspace.hash);
    }

    #[test]
    fn a_theme_file_falls_back_to_dark_for_omitted_tokens() {
        let dir = std::env::temp_dir().join(format!("rrover-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mine.json5");
        std::fs::write(&file, r##"{ base00: "#010203", hash: "#040506" }"##).unwrap();

        let (mut colors, mut keyspace) = dark();
        let (want_colors, want_keyspace) = dark();
        apply("mine.json5", &dir, &mut colors, &mut keyspace).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(colors.base00, Color::Rgb(1, 2, 3));
        assert_eq!(keyspace.hash, Color::Rgb(4, 5, 6));
        assert_eq!(colors.base05, want_colors.base05);
        assert_eq!(keyspace.list, want_keyspace.list);
    }

    #[test]
    fn bad_themes_are_errors() {
        let (mut colors, mut keyspace) = dark();
        let dir = Path::new("/nonexistent");

        let unknown = apply("solarized", dir, &mut colors, &mut keyspace).unwrap_err();
        assert!(unknown.contains("unknown theme"), "{unknown}");
        let missing = apply("nope.json5", dir, &mut colors, &mut keyspace).unwrap_err();
        assert!(missing.contains("no theme file"), "{missing}");
        let token = set_token("base99", "#000000", &mut colors, &mut keyspace).unwrap_err();
        assert!(token.contains("unknown token"), "{token}");
        let value = set_token("base00", "nope!", &mut colors, &mut keyspace).unwrap_err();
        assert!(value.contains("base00"), "{value}");
    }
}

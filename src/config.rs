use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::OnceLock, time::Duration};

use color_eyre::eyre::Result;
use derive_deref::{Deref, DerefMut};
use ratatui::style::{Color, Modifier, Style};
use serde::{de::Deserializer, Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::{keybindings::KeyBindings, mode::Mode};

const CONFIG_PATH: &str = include_str!("../.config/config.json5");
static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get_or_init(|| Config::new().expect("Configuration file is not set."))
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub _data_dir: PathBuf,
    #[serde(default)]
    pub _config_dir: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default, flatten)]
    #[allow(dead_code, clippy::struct_field_names)]
    pub config: AppConfig,
    #[serde(default)]
    pub keybindings: KeyBindings,
    #[serde(default)]
    pub styles: Styles,
    #[serde(default)]
    pub colors: Base16Palette,
    #[serde(default)]
    pub keyspace: KeyspaceConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub pubsub: PubSubConfig,
}

/// Buffer limits of each Pub/Sub subscription.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PubSubConfig {
    /// Messages kept per subscription; see [`PubSubConfig::buffer_messages`].
    pub buffer_messages: usize,
    /// Payload bytes kept per subscription; see [`PubSubConfig::buffer_bytes`].
    pub buffer_bytes: usize,
    /// Payload bytes stored per message; see [`PubSubConfig::max_payload`].
    pub max_payload: usize,
    /// Notification thresholds.
    pub notify: NotifyConfig,
}

/// Thresholds of the Pub/Sub notifications.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Quiet time before the next toast; see [`NotifyConfig::cooldown`].
    pub cooldown_secs: u64,
    /// msg/s above which a subscription is noisy; see [`NotifyConfig::mute_rate`].
    pub mute_rate: u64,
    /// Time above `mute_rate` before muting; see [`NotifyConfig::mute_after`].
    pub mute_after_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 30,
            mute_rate: 10,
            mute_after_secs: 10,
        }
    }
}

impl NotifyConfig {
    /// Quiet time before the next toast, clamped to 1 s..=1 h.
    pub fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_secs.clamp(1, 3600))
    }

    /// Noisy rate in msg/s, clamped to 1..=100,000.
    pub fn mute_rate(&self) -> u64 {
        self.mute_rate.clamp(1, 100_000)
    }

    /// Time above the rate before muting, clamped to 1 s..=10 min.
    pub fn mute_after(&self) -> Duration {
        Duration::from_secs(self.mute_after_secs.clamp(1, 600))
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn policy(&self) -> crate::pubsub_notify::NotifyPolicy {
        crate::pubsub_notify::NotifyPolicy {
            cooldown: self.cooldown(),
            mute_rate: self.mute_rate() as f64,
            mute_after: self.mute_after(),
        }
    }
}

impl Default for PubSubConfig {
    fn default() -> Self {
        Self {
            buffer_messages: 1000,
            buffer_bytes: 8 * 1024 * 1024,
            max_payload: 64 * 1024,
            notify: NotifyConfig::default(),
        }
    }
}

impl PubSubConfig {
    /// Messages per subscription, clamped to 10..=100,000.
    pub fn buffer_messages(&self) -> usize {
        self.buffer_messages.clamp(10, 100_000)
    }

    /// Bytes per subscription, clamped to 64 KiB..=256 MiB.
    pub fn buffer_bytes(&self) -> usize {
        self.buffer_bytes.clamp(64 * 1024, 256 * 1024 * 1024)
    }

    /// Bytes stored per message, clamped to 1 KiB..=[`Self::buffer_bytes`], so
    /// one message always fits in the buffer.
    pub fn max_payload(&self) -> usize {
        self.max_payload.clamp(1024, self.buffer_bytes())
    }

    pub fn limits(&self) -> crate::redis_client::pubsub_store::BufferLimits {
        crate::redis_client::pubsub_store::BufferLimits {
            messages: self.buffer_messages(),
            bytes: self.buffer_bytes(),
            payload: self.max_payload(),
        }
    }
}

/// Settings for the live-metrics dashboard.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    /// Sampling interval in milliseconds; see [`DashboardConfig::interval`].
    pub interval_ms: u64,
    /// Number of samples kept per series; see [`DashboardConfig::history`].
    pub history: usize,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            history: 300,
        }
    }
}

impl DashboardConfig {
    /// Sampling interval, clamped to 250 ms..=10 s.
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.clamp(250, 10_000))
    }

    /// History length in samples, clamped to 30..=3600.
    pub fn history(&self) -> usize {
        self.history.clamp(30, 3600)
    }
}

impl Config {
    /// Load the application configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the user's configuration file cannot be
    /// read or deserialized into [`Config`].
    pub fn new() -> Result<Self, config::ConfigError> {
        let default_config: Config = json5::from_str(CONFIG_PATH).unwrap();
        let data_dir = crate::utils::get_data_dir();
        let config_dir = crate::utils::get_config_dir();
        let mut builder = config::Config::builder()
            .set_default("_data_dir", data_dir.to_str().unwrap())?
            .set_default("_config_dir", config_dir.to_str().unwrap())?;

        let config_files = [
            ("config.json5", config::FileFormat::Json5),
            ("config.json", config::FileFormat::Json),
            ("config.yaml", config::FileFormat::Yaml),
            ("config.toml", config::FileFormat::Toml),
            ("config.ini", config::FileFormat::Ini),
        ];
        let mut found_config = false;
        for (file, format) in &config_files {
            builder = builder.add_source(
                config::File::from(config_dir.join(file))
                    .format(*format)
                    .required(false),
            );
            if config_dir.join(file).exists() {
                found_config = true;
            }
        }
        if !found_config {
            log::error!("No configuration file found. Application may not behave as expected");
        }

        let mut cfg: Self = builder.build()?.try_deserialize()?;

        for (mode, default_bindings) in &default_config.keybindings.0 {
            let user_bindings = cfg.keybindings.entry(*mode).or_default();
            for (key, cmd) in default_bindings {
                user_bindings.entry(key.clone()).or_insert(*cmd);
            }
        }
        for (mode, default_styles) in &default_config.styles.0 {
            let user_styles = cfg.styles.entry(*mode).or_default();
            for (style_key, style) in default_styles {
                user_styles.entry(style_key.clone()).or_insert(*style);
            }
        }

        cfg.colors = default_config.colors;

        Ok(cfg)
    }
}

#[serde_as]
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Base16Palette {
    /// Default Background
    #[serde_as(as = "DisplayFromStr")]
    pub base00: Color,

    /// Lighter Background (Used for status bars, line number and folding marks)
    #[serde_as(as = "DisplayFromStr")]
    pub base01: Color,

    /// Selection Background (Settings where you need to highlight text, such as find results)
    #[serde_as(as = "DisplayFromStr")]
    pub base02: Color,

    /// Comments, Invisibles, Line Highlighting
    #[serde_as(as = "DisplayFromStr")]
    pub base03: Color,

    /// Dark Foreground (Used for status bars)
    #[serde_as(as = "DisplayFromStr")]
    pub base04: Color,

    /// Default Foreground, Caret, Delimiters, Operators
    #[serde_as(as = "DisplayFromStr")]
    pub base05: Color,

    /// Light Foreground (Not often used, could be used for hover states or dividers)
    #[serde_as(as = "DisplayFromStr")]
    pub base06: Color,

    /// Light Background (Probably at most for cursor line background color)
    #[serde_as(as = "DisplayFromStr")]
    pub base07: Color,

    /// Variables, XML Tags, Markup Link Text, Markup Lists, Diff Deleted
    #[serde_as(as = "DisplayFromStr")]
    pub base08: Color,

    /// Integers, Boolean, Constants, XML Attributes, Markup Link Url
    #[serde_as(as = "DisplayFromStr")]
    pub base09: Color,

    /// Classes, Keywords, Storage, Selector, Markup Italic, Diff Changed
    #[serde_as(as = "DisplayFromStr")]
    pub base0a: Color,

    /// Strings, Inherited Class, Markup Code, Diff Inserted
    #[serde_as(as = "DisplayFromStr")]
    pub base0b: Color,

    /// Support, Regular Expressions, Escape Characters, Markup Quotes
    #[serde_as(as = "DisplayFromStr")]
    pub base0c: Color,

    /// Functions, Methods, Attribute IDs, Headings
    #[serde_as(as = "DisplayFromStr")]
    pub base0d: Color,

    /// Keywords, Storage, Selector, Markup Bold, Diff Renamed
    #[serde_as(as = "DisplayFromStr")]
    pub base0e: Color,

    /// Deprecated, Opening/Closing Embedded Language Tags e.g., `<? ?>`
    #[serde_as(as = "DisplayFromStr")]
    pub base0f: Color,
}

impl Default for Base16Palette {
    fn default() -> Self {
        Self {
            base00: Color::from_str("#191724").unwrap(),
            base01: Color::from_str("#1f1d2e").unwrap(),
            base02: Color::from_str("#26233a").unwrap(),
            base03: Color::from_str("#6e6a86").unwrap(),
            base04: Color::from_str("#908caa").unwrap(),
            base05: Color::from_str("#e0def4").unwrap(),
            base06: Color::from_str("#e0def4").unwrap(),
            base07: Color::from_str("#524f67").unwrap(),
            base08: Color::from_str("#eb6f92").unwrap(),
            base09: Color::from_str("#f6c177").unwrap(),
            base0a: Color::from_str("#ebbcba").unwrap(),
            base0b: Color::from_str("#31748f").unwrap(),
            base0c: Color::from_str("#9ccfd8").unwrap(),
            base0d: Color::from_str("#c4a7e7").unwrap(),
            base0e: Color::from_str("#f6c177").unwrap(),
            base0f: Color::from_str("#524f67").unwrap(),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct KeyspaceConfig {
    #[serde_as(as = "DisplayFromStr")]
    pub string: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub set: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub zset: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub hash: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub json: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub list: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub stream: Color,

    #[serde_as(as = "DisplayFromStr")]
    pub unknown: Color,
}

impl Default for KeyspaceConfig {
    fn default() -> Self {
        Self {
            string: Color::from_str("#00f769").unwrap(),
            set: Color::from_str("#ebff87").unwrap(),
            zset: Color::from_str("#ea51b2").unwrap(),
            json: Color::from_str("#b45bcf").unwrap(),
            hash: Color::from_str("#62d6e8").unwrap(),
            list: Color::from_str("#ffb86c").unwrap(),
            stream: Color::from_str("#8be9fd").unwrap(),
            unknown: Color::from_str("#f71638").unwrap(),
        }
    }
}

#[derive(Clone, Debug, Default, Deref, DerefMut)]
pub struct Styles(pub HashMap<Mode, HashMap<String, Style>>);

impl<'de> Deserialize<'de> for Styles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let parsed_map = HashMap::<Mode, HashMap<String, String>>::deserialize(deserializer)?;

        let styles = parsed_map
            .into_iter()
            .map(|(mode, inner_map)| {
                let converted_inner_map = inner_map
                    .into_iter()
                    .map(|(str, style)| (str, parse_style(&style)))
                    .collect();
                (mode, converted_inner_map)
            })
            .collect();

        Ok(Styles(styles))
    }
}

pub fn parse_style(line: &str) -> Style {
    let (foreground, background) =
        line.split_at(line.to_lowercase().find("on ").unwrap_or(line.len()));
    let foreground = process_color_string(foreground);
    let background = process_color_string(&background.replace("on ", ""));

    let mut style = Style::default();
    if let Some(fg) = parse_color(&foreground.0) {
        style = style.fg(fg);
    }
    if let Some(bg) = parse_color(&background.0) {
        style = style.bg(bg);
    }
    style = style.add_modifier(foreground.1 | background.1);
    style
}

fn process_color_string(color_str: &str) -> (String, Modifier) {
    let color = color_str
        .replace("grey", "gray")
        .replace("bright ", "")
        .replace("bold ", "")
        .replace("underline ", "")
        .replace("inverse ", "");

    let mut modifiers = Modifier::empty();
    if color_str.contains("underline") {
        modifiers |= Modifier::UNDERLINED;
    }
    if color_str.contains("bold") {
        modifiers |= Modifier::BOLD;
    }
    if color_str.contains("inverse") {
        modifiers |= Modifier::REVERSED;
    }

    (color, modifiers)
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim_start();
    let s = s.trim_end();
    if s.contains("bright color") {
        let s = s.trim_start_matches("bright ");
        let c = s
            .trim_start_matches("color")
            .parse::<u8>()
            .unwrap_or_default();
        Some(Color::Indexed(c.wrapping_shl(8)))
    } else if s.contains("color") {
        let c = s
            .trim_start_matches("color")
            .parse::<u8>()
            .unwrap_or_default();
        Some(Color::Indexed(c))
    } else if s.contains("gray") {
        let c = 232
            + s.trim_start_matches("gray")
                .parse::<u8>()
                .unwrap_or_default();
        Some(Color::Indexed(c))
    } else if s.contains("rgb") {
        let red = u8::try_from((s.as_bytes()[3] as char).to_digit(10).unwrap_or_default())
            .unwrap_or_default();
        let green = u8::try_from((s.as_bytes()[4] as char).to_digit(10).unwrap_or_default())
            .unwrap_or_default();
        let blue = u8::try_from((s.as_bytes()[5] as char).to_digit(10).unwrap_or_default())
            .unwrap_or_default();
        let c = 16 + red * 36 + green * 6 + blue;
        Some(Color::Indexed(c))
    } else if s == "bold black" {
        Some(Color::Indexed(8))
    } else if s == "bold red" {
        Some(Color::Indexed(9))
    } else if s == "bold green" {
        Some(Color::Indexed(10))
    } else if s == "bold yellow" {
        Some(Color::Indexed(11))
    } else if s == "bold blue" {
        Some(Color::Indexed(12))
    } else if s == "bold magenta" {
        Some(Color::Indexed(13))
    } else if s == "bold cyan" {
        Some(Color::Indexed(14))
    } else if s == "bold white" {
        Some(Color::Indexed(15))
    } else if s == "black" {
        Some(Color::Indexed(0))
    } else if s == "red" {
        Some(Color::Indexed(1))
    } else if s == "green" {
        Some(Color::Indexed(2))
    } else if s == "yellow" {
        Some(Color::Indexed(3))
    } else if s == "blue" {
        Some(Color::Indexed(4))
    } else if s == "magenta" {
        Some(Color::Indexed(5))
    } else if s == "cyan" {
        Some(Color::Indexed(6))
    } else if s == "white" {
        Some(Color::Indexed(7))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use crate::{command::Command, keybindings::parse_key_sequence};

    use super::*;

    #[test]
    fn dashboard_config_defaults_and_clamps() {
        let d = DashboardConfig::default();
        assert_eq!(d.interval_ms, 1000);
        assert_eq!(d.history, 300);
        assert_eq!(d.interval(), Duration::from_secs(1));
        assert_eq!(d.history(), 300);

        let c = DashboardConfig {
            interval_ms: 1,
            history: 99_999,
        };
        assert_eq!(c.interval(), Duration::from_millis(250));
        assert_eq!(c.history(), 3600);

        let c = DashboardConfig {
            interval_ms: 1_000_000,
            history: 1,
        };
        assert_eq!(c.interval(), Duration::from_secs(10));
        assert_eq!(c.history(), 30);

        let cfg: Config = json5::from_str("{ keybindings: {} }").unwrap();
        assert_eq!(cfg.dashboard.interval_ms, 1000);
        assert_eq!(cfg.dashboard.history, 300);
    }

    #[test]
    fn pubsub_config_defaults_and_clamps() {
        let d = PubSubConfig::default();
        assert_eq!(d.buffer_messages(), 1000);
        assert_eq!(d.buffer_bytes(), 8 * 1024 * 1024);
        assert_eq!(d.max_payload(), 64 * 1024);

        let c = PubSubConfig {
            buffer_messages: 1,
            buffer_bytes: 1,
            max_payload: 1,
            notify: NotifyConfig::default(),
        };
        assert_eq!(c.buffer_messages(), 10);
        assert_eq!(c.buffer_bytes(), 64 * 1024);
        assert_eq!(c.max_payload(), 1024);

        let c = PubSubConfig {
            buffer_messages: usize::MAX,
            buffer_bytes: usize::MAX,
            max_payload: usize::MAX,
            notify: NotifyConfig::default(),
        };
        assert_eq!(c.buffer_messages(), 100_000);
        assert_eq!(c.buffer_bytes(), 256 * 1024 * 1024);
        assert_eq!(c.max_payload(), 256 * 1024 * 1024);

        // max_payload never exceeds the buffer.
        let c = PubSubConfig {
            buffer_messages: 100,
            buffer_bytes: 100 * 1024,
            max_payload: 1024 * 1024,
            notify: NotifyConfig::default(),
        };
        assert_eq!(c.max_payload(), 100 * 1024);

        let cfg: Config =
            json5::from_str("{ keybindings: {}, pubsub: { buffer_messages: 50 } }").unwrap();
        assert_eq!(cfg.pubsub.buffer_messages(), 50);
        assert_eq!(cfg.pubsub.max_payload(), 64 * 1024);
    }

    #[test]
    fn pubsub_notify_config_defaults_and_clamps() {
        let d = NotifyConfig::default();
        assert_eq!(d.cooldown(), Duration::from_secs(30));
        assert_eq!(d.mute_rate(), 10);
        assert_eq!(d.mute_after(), Duration::from_secs(10));
        let p = d.policy();
        assert!((p.mute_rate - 10.0).abs() < f64::EPSILON);
        assert_eq!(p.cooldown, Duration::from_secs(30));

        let c = NotifyConfig {
            cooldown_secs: 0,
            mute_rate: 0,
            mute_after_secs: 0,
        };
        assert_eq!(c.cooldown(), Duration::from_secs(1));
        assert_eq!(c.mute_rate(), 1);
        assert_eq!(c.mute_after(), Duration::from_secs(1));

        let c = NotifyConfig {
            cooldown_secs: u64::MAX,
            mute_rate: u64::MAX,
            mute_after_secs: u64::MAX,
        };
        assert_eq!(c.cooldown(), Duration::from_hours(1));
        assert_eq!(c.mute_rate(), 100_000);
        assert_eq!(c.mute_after(), Duration::from_mins(10));

        // A partial section keeps the other defaults.
        let cfg: Config =
            json5::from_str("{ keybindings: {}, pubsub: { notify: { mute_rate: 50 } } }").unwrap();
        assert_eq!(cfg.pubsub.notify.mute_rate(), 50);
        assert_eq!(cfg.pubsub.notify.cooldown(), Duration::from_secs(30));
        assert_eq!(cfg.pubsub.buffer_messages(), 1000);
    }

    #[test]
    fn bundled_pubsub_bindings() -> Result<()> {
        use crate::keybindings::KeyResolution;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let c = Config::new()?;
        let resolve = |mode, code| {
            c.keybindings
                .resolve(mode, &[KeyEvent::new(code, KeyModifiers::empty())])
        };
        for mode in [Mode::KeySpace, Mode::Dashboard, Mode::PubSub] {
            assert_eq!(
                resolve(mode, KeyCode::Char('b')),
                KeyResolution::Command(Command::TogglePubSub),
                "{mode:?}"
            );
        }
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Esc),
            KeyResolution::Command(Command::TogglePubSub)
        );
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Char('a')),
            KeyResolution::Command(Command::PubSubAdd)
        );
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Char('d')),
            KeyResolution::Command(Command::PubSubRemove)
        );
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Char('c')),
            KeyResolution::Command(Command::PubSubClear)
        );
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Char('r')),
            KeyResolution::Command(Command::PubSubRefresh)
        );
        assert_eq!(
            resolve(Mode::PubSub, KeyCode::Enter),
            KeyResolution::Command(Command::PubSubEnter)
        );
        assert_eq!(
            c.keybindings.resolve(
                Mode::PubSub,
                &[KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)]
            ),
            KeyResolution::Command(Command::PubSubPublish)
        );
        assert_eq!(
            c.keybindings.resolve(
                Mode::PubSub,
                &[KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)]
            ),
            KeyResolution::Command(Command::PubSubToggleNotify)
        );
        Ok(())
    }

    #[test]
    fn test_parse_style_default() {
        let style = parse_style("");
        assert_eq!(style, Style::default());
    }

    #[test]
    fn test_parse_style_foreground() {
        let style = parse_style("red");
        assert_eq!(style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn test_parse_style_background() {
        let style = parse_style("on blue");
        assert_eq!(style.bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn test_parse_style_modifiers() {
        let style = parse_style("underline red on blue");
        assert_eq!(style.fg, Some(Color::Indexed(1)));
        assert_eq!(style.bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn test_process_color_string() {
        let (color, modifiers) = process_color_string("underline bold inverse gray");
        assert_eq!(color, "gray");
        assert!(modifiers.contains(Modifier::UNDERLINED));
        assert!(modifiers.contains(Modifier::BOLD));
        assert!(modifiers.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_parse_color_rgb() {
        let color = parse_color("rgb123");
        let expected = 16 + 36 + 2 * 6 + 3;
        assert_eq!(color, Some(Color::Indexed(expected)));
    }

    #[test]
    fn test_parse_color_unknown() {
        let color = parse_color("unknown");
        assert_eq!(color, None);
    }

    #[test]
    fn test_config() -> Result<()> {
        let c = Config::new()?;
        assert_eq!(
            c.keybindings
                .get(&Mode::Common)
                .unwrap()
                .get(&parse_key_sequence("<Ctrl-c>").unwrap_or_default())
                .unwrap(),
            &Command::Quit
        );
        Ok(())
    }

    #[test]
    fn bundled_yank_and_wrap_bindings() -> Result<()> {
        use crate::keybindings::KeyResolution;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let c = Config::new()?;
        let resolve = |code, modifiers| {
            c.keybindings
                .resolve(Mode::KeySpace, &[KeyEvent::new(code, modifiers)])
        };
        assert_eq!(
            resolve(KeyCode::Char('y'), KeyModifiers::empty()),
            KeyResolution::Command(Command::YankValue)
        );
        // Terminals send Shift-y as an uppercase char with SHIFT set.
        assert_eq!(
            resolve(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            KeyResolution::Command(Command::YankKey)
        );
        assert_eq!(
            resolve(KeyCode::Char('w'), KeyModifiers::empty()),
            KeyResolution::Command(Command::ToggleWrap)
        );
        assert_eq!(
            resolve(KeyCode::Right, KeyModifiers::empty()),
            KeyResolution::Command(Command::ScrollRight)
        );
        Ok(())
    }
}

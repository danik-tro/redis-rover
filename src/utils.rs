use std::{path::PathBuf, sync::LazyLock};

use byte_unit::{Byte, UnitType};
use color_eyre::eyre::Result;
use directories::ProjectDirs;
use tracing::error;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    self, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, Layer,
};

const VERSION_MESSAGE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("RROVER_GIT_DESCRIBE"),
    " (",
    env!("RROVER_BUILD_DATE"),
    ")"
);

static PROJECT_NAME: LazyLock<String> = LazyLock::new(|| env!("CARGO_CRATE_NAME").to_uppercase());
pub static DATA_FOLDER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var(format!("{}_DATA", PROJECT_NAME.as_str()))
        .ok()
        .map(PathBuf::from)
});
pub static CONFIG_FOLDER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var(format!("{}_CONFIG", PROJECT_NAME.as_str()))
        .ok()
        .map(PathBuf::from)
});
pub static LOG_ENV: LazyLock<String> =
    LazyLock::new(|| format!("{}_LOGLEVEL", PROJECT_NAME.as_str()));
pub static LOG_FILE: LazyLock<String> = LazyLock::new(|| format!("{}.log", env!("CARGO_PKG_NAME")));

fn project_directory() -> Option<ProjectDirs> {
    ProjectDirs::from("io.github", "danik-tro", env!("CARGO_PKG_NAME"))
}

/// Where versions up to 0.x kept their files: under the qualifier of the
/// project template (`com.kdheepak.redis-rover` on macOS, `kdheepak\…` on
/// Windows; the same path as today on Linux).
fn legacy_project_directory() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "kdheepak", env!("CARGO_PKG_NAME"))
}

/// `current`, unless it doesn't exist yet and `legacy` does: an existing
/// config keeps working after the directory was renamed.
fn current_or_legacy(current: PathBuf, legacy: Option<PathBuf>) -> PathBuf {
    match legacy {
        Some(legacy) if !current.exists() && legacy.exists() => legacy,
        _ => current,
    }
}

/// Install panic + eyre hooks that tear down the TUI and emit a
/// human-readable crash report.
///
/// # Errors
///
/// Returns an error if the eyre hook cannot be installed.
pub fn initialize_panic_handler() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default()
        .panic_section(format!(
            "This is a bug. Consider reporting it at {}",
            env!("CARGO_PKG_REPOSITORY")
        ))
        .capture_span_trace_by_default(false)
        .display_location_section(false)
        .display_env_section(false)
        .into_hooks();
    eyre_hook.install()?;
    std::panic::set_hook(Box::new(move |panic_info| {
        if let Err(r) = crate::tui::restore() {
            error!("Unable to exit Terminal: {r:?}");
        }

        #[cfg(not(debug_assertions))]
        {
            use human_panic::{handle_dump, print_msg, Metadata};
            let meta = Metadata::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                .authors(env!("CARGO_PKG_AUTHORS").replace(':', ", "))
                .homepage(env!("CARGO_PKG_HOMEPAGE"));

            let file_path = handle_dump(&meta, panic_info);
            // prints human-panic message
            print_msg(file_path, &meta)
                .expect("human-panic: printing error message to console failed");
            eprintln!("{}", panic_hook.panic_report(panic_info)); // prints color-eyre stack trace to stderr
        }
        let msg = format!("{}", panic_hook.panic_report(panic_info));
        log::error!("Error: {error}", error = strip_ansi_escapes::strip_str(msg));

        #[cfg(debug_assertions)]
        {
            // Better Panic stacktrace that is only enabled when debugging.
            better_panic::Settings::auto()
                .most_recent_first(false)
                .lineno_suffix(true)
                .verbosity(better_panic::Verbosity::Full)
                .create_panic_handler()(panic_info);
        }

        std::process::exit(libc::EXIT_FAILURE);
    }));
    Ok(())
}

pub fn get_data_dir() -> PathBuf {
    let directory = if let Some(s) = DATA_FOLDER.clone() {
        s
    } else if let Some(proj_dirs) = project_directory() {
        current_or_legacy(
            proj_dirs.data_local_dir().to_path_buf(),
            legacy_project_directory().map(|dirs| dirs.data_local_dir().to_path_buf()),
        )
    } else {
        PathBuf::from(".").join(".data")
    };
    directory
}

pub fn get_config_dir() -> PathBuf {
    let directory = if let Some(s) = CONFIG_FOLDER.clone() {
        s
    } else if let Some(proj_dirs) = project_directory() {
        current_or_legacy(
            proj_dirs.config_local_dir().to_path_buf(),
            legacy_project_directory().map(|dirs| dirs.config_local_dir().to_path_buf()),
        )
    } else {
        PathBuf::from(".").join(".config")
    };
    directory
}

/// Create (or truncate) `path` readable and writable by its owner only. The
/// log holds key names and error text from the server, which other local
/// users have no business reading. A file left from an older version with
/// wider permissions is narrowed too.
fn create_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Initialise the file-based tracing subscriber under the data directory.
///
/// # Errors
///
/// Returns an error if the data directory cannot be created or the log
/// file cannot be opened for writing.
pub fn initialize_logging() -> Result<()> {
    let directory = get_data_dir();
    std::fs::create_dir_all(&directory)?;
    let log_path = directory.join(LOG_FILE.as_str());
    let log_file = create_private_file(&log_path)?;
    // Read here, never written back with `set_var`: other threads already
    // run by now, and changing the environment under them is unsound.
    let directives = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var(LOG_ENV.as_str()))
        .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME")));
    let file_subscriber = tracing_subscriber::fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_writer(log_file)
        .with_target(false)
        .with_ansi(false)
        .with_filter(tracing_subscriber::filter::EnvFilter::new(directives));
    tracing_subscriber::registry()
        .with(file_subscriber)
        .with(ErrorLayer::default())
        .init();
    Ok(())
}

/// Similar to the `std::dbg!` macro, but generates `tracing` events rather
/// than printing to stdout.
///
/// By default, the verbosity level for the generated events is `DEBUG`, but
/// this can be customized.
// Crate-private debugging helper (not exported from the library).
#[allow(unused_macros)]
macro_rules! trace_dbg {
    (target: $target:expr, level: $level:expr, $ex:expr) => {{
        match $ex {
            value => {
                tracing::event!(target: $target, $level, ?value, stringify!($ex));
                value
            }
        }
    }};
    (level: $level:expr, $ex:expr) => {
        trace_dbg!(target: module_path!(), level: $level, $ex)
    };
    (target: $target:expr, $ex:expr) => {
        trace_dbg!(target: $target, level: tracing::Level::DEBUG, $ex)
    };
    ($ex:expr) => {
        trace_dbg!(level: tracing::Level::DEBUG, $ex)
    };
}

pub fn version() -> String {
    let author = clap::crate_authors!();

    // let current_exe_path = PathBuf::from(clap::crate_name!()).display().to_string();
    let config_dir_path = get_config_dir().display().to_string();
    let data_dir_path = get_data_dir().display().to_string();

    format!(
        "\
{VERSION_MESSAGE}

Authors: {author}

Config directory: {config_dir_path}
Data directory: {data_dir_path}"
    )
}

/// Format a duration in seconds as at most two units, e.g. `3d 4h`, `5m 2s`.
pub fn format_duration(secs: usize) -> String {
    if secs == 0 {
        return "0s".into();
    }
    let units = [
        (secs / 86_400, "d"),
        (secs % 86_400 / 3_600, "h"),
        (secs % 3_600 / 60, "m"),
        (secs % 60, "s"),
    ];
    units
        .iter()
        .skip_while(|(n, _)| *n == 0)
        .take(2)
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Compact count: `123`, `12.4k`, `1.2M`, `3.0G`.
pub fn fmt_compact(v: f64) -> String {
    let a = v.abs();
    if a < 1_000.0 {
        format!("{v:.0}")
    } else if a < 1_000_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else if a < 1_000_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else {
        format!("{:.1}G", v / 1_000_000_000.0)
    }
}

/// Binary-unit byte size, e.g. `1.5 MiB`.
pub fn fmt_bytes(v: u64) -> String {
    let adjusted = Byte::from_u64(v).get_appropriate_unit(UnitType::Binary);
    format!("{adjusted:.1}")
}

/// Byte throughput, e.g. `8.4 MiB/s`.
pub fn fmt_rate_bytes(bps: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = bps.max(0.0).round() as u64;
    format!("{}/s", fmt_bytes(bytes))
}

/// Ratio `0.0..=1.0` as a percentage, e.g. `98.2 %`.
pub fn fmt_pct(ratio: f64) -> String {
    format!("{:.1} %", ratio * 100.0)
}

/// `1234567` → `1,234,567`.
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A TTL as typed in the TTL popup ([`parse_ttl`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlInput {
    /// Expire in this many seconds; `0` deletes the key.
    Set(u64),
    /// Move the current TTL by this many seconds (`+10m`, `-5m`).
    Shift(i64),
    /// Remove the TTL (`PERSIST`).
    Persist,
}

/// What the TTL popup accepts, for its error message and hint.
pub const TTL_HELP: &str = "use 90, 90s, 15m, 2h30m, 1d, +10m, -5m or persist";

/// Parse a TTL: `90` / `90s` (seconds), `15m`, `2h30m`, `1d`, each unit at
/// most once and from the largest down; a leading `+` / `-` shifts the
/// current TTL; `persist` removes it.
///
/// # Errors
///
/// A message naming the accepted forms when `text` is none of them.
pub fn parse_ttl(text: &str) -> Result<TtlInput, String> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("persist") {
        return Ok(TtlInput::Persist);
    }
    let bad = || format!("“{text}” is not a TTL: {TTL_HELP}");
    let (sign, body) = match text.as_bytes().first() {
        Some(b'+') => (Some(1), &text[1..]),
        Some(b'-') => (Some(-1), &text[1..]),
        _ => (None, text),
    };
    let secs = parse_duration_secs(body).ok_or_else(bad)?;
    match sign {
        None => Ok(TtlInput::Set(secs)),
        Some(sign) => {
            let secs = i64::try_from(secs).map_err(|_| bad())?;
            Ok(TtlInput::Shift(sign * secs))
        }
    }
}

/// Duration units, largest first, with their length in seconds.
const UNITS: [(u8, u64); 4] = [(b'd', 86_400), (b'h', 3_600), (b'm', 60), (b's', 1)];

/// `90`, `90s`, `2h30m`, `1d`: units `d h m s` in that order, each once.
fn parse_duration_secs(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse().ok();
    }
    let mut total: u64 = 0;
    let mut next_unit = 0;
    let mut digits = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_digit() {
            digits.push(char::from(byte));
            continue;
        }
        let at = UNITS[next_unit..]
            .iter()
            .position(|(unit, _)| unit.eq_ignore_ascii_case(&byte))?
            + next_unit;
        if digits.is_empty() {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        total = total.checked_add(n.checked_mul(UNITS[at].1)?)?;
        digits.clear();
        next_unit = at + 1;
    }
    // Trailing digits without a unit (`1h30`).
    digits.is_empty().then_some(total)
}

#[cfg(test)]
mod ttl_tests {
    use super::{parse_ttl, TtlInput};

    #[test]
    fn accepts_every_documented_form() {
        for (text, want) in [
            ("90", TtlInput::Set(90)),
            ("90s", TtlInput::Set(90)),
            ("15m", TtlInput::Set(900)),
            ("2h30m", TtlInput::Set(9_000)),
            ("1d", TtlInput::Set(86_400)),
            ("1d2h3m4s", TtlInput::Set(93_784)),
            (" 2H ", TtlInput::Set(7_200)),
            ("+10m", TtlInput::Shift(600)),
            ("-5m", TtlInput::Shift(-300)),
            ("+30", TtlInput::Shift(30)),
            ("-1", TtlInput::Shift(-1)),
            ("persist", TtlInput::Persist),
            ("PERSIST", TtlInput::Persist),
            ("0", TtlInput::Set(0)),
            ("0s", TtlInput::Set(0)),
        ] {
            assert_eq!(parse_ttl(text), Ok(want), "{text}");
        }
    }

    #[test]
    fn rejects_bad_input_with_the_accepted_forms() {
        for text in [
            "",
            "  ",
            "5x",
            "1m1h",
            "1h1h",
            "h",
            "1h30",
            "+",
            "-",
            "+persist",
            "--5m",
            "+-5m",
            "1.5h",
            "abc",
            "99999999999999999999",
            "999999999999999999d",
        ] {
            let err = parse_ttl(text).expect_err(text);
            assert!(err.contains("15m") && err.contains("persist"), "{err}");
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn compact() {
        assert_eq!(fmt_compact(123.0), "123");
        assert_eq!(fmt_compact(12_400.0), "12.4k");
        assert_eq!(fmt_compact(1_200_000.0), "1.2M");
        assert_eq!(fmt_compact(3_000_000_000.0), "3.0G");
    }

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(fmt_bytes(1_565_498_352), "1.5 GiB");
    }

    #[test]
    fn rate_bytes() {
        assert_eq!(fmt_rate_bytes(1024.0 * 1024.0 * 8.5), "8.5 MiB/s");
        assert_eq!(fmt_rate_bytes(-5.0), "0 B/s");
    }

    #[test]
    fn pct() {
        assert_eq!(fmt_pct(0.982), "98.2 %");
        assert_eq!(fmt_pct(0.0), "0.0 %");
    }
}

#[cfg(all(test, unix))]
mod log_file_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::create_private_file;

    /// #70: the log file is the owner's only, also when an older, wider
    /// file is already there.
    #[test]
    fn log_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("rrover-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("redis-rover.log");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        drop(create_private_file(&path).unwrap());
        assert_eq!(mode(&path), 0o600);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(create_private_file(&path).unwrap());
        assert_eq!(mode(&path), 0o600, "an old 0644 file is narrowed");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

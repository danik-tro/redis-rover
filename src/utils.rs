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
    env!("VERGEN_GIT_DESCRIBE"),
    " (",
    env!("VERGEN_BUILD_DATE"),
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
    ProjectDirs::from("com", "kdheepak", env!("CARGO_PKG_NAME"))
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
            let meta = Metadata {
                version: env!("CARGO_PKG_VERSION").into(),
                name: env!("CARGO_PKG_NAME").into(),
                authors: env!("CARGO_PKG_AUTHORS").replace(':', ", ").into(),
                homepage: env!("CARGO_PKG_HOMEPAGE").into(),
            };

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
        proj_dirs.data_local_dir().to_path_buf()
    } else {
        PathBuf::from(".").join(".data")
    };
    directory
}

pub fn get_config_dir() -> PathBuf {
    let directory = if let Some(s) = CONFIG_FOLDER.clone() {
        s
    } else if let Some(proj_dirs) = project_directory() {
        proj_dirs.config_local_dir().to_path_buf()
    } else {
        PathBuf::from(".").join(".config")
    };
    directory
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
    let log_file = std::fs::File::create(log_path)?;
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG")
            .or_else(|_| std::env::var(LOG_ENV.as_str()))
            .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME"))),
    );
    let file_subscriber = tracing_subscriber::fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_writer(log_file)
        .with_target(false)
        .with_ansi(false)
        .with_filter(tracing_subscriber::filter::EnvFilter::from_default_env());
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
#[macro_export]
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

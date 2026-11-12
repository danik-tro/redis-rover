//! Where to connect: the CLI flags, a connection profile from the config and
//! `REDIS_URL`, resolved into one [`Target`].
//!
//! [`resolve`] is pure (no environment reads, no I/O), so the resolution
//! order is table-tested. Running `password_command` and the `--token`
//! prompt happen later, in [`Target::connection_info`].

use std::{
    collections::HashMap,
    fmt::{self, Write as _},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{eyre, Result};
use redis::{ConnectionInfo, IntoConnectionInfo};
use serde::Deserialize;

use crate::cli::{apply_tls_flags, Cli, TlsArgs};

/// Used when neither `--url`, a profile nor `REDIS_URL` names a server.
pub const DEFAULT_URL: &str = "redis://localhost:6379";

/// One entry of `profiles` in the config file.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub url: Option<String>,
    pub db: Option<i64>,
    pub tls: Option<TlsProfile>,
    /// Shell command that prints the password (`op read …`, `pass show …`).
    #[serde(alias = "passwordCommand")]
    pub password_command: Option<String>,
    /// Inferred from the host when unset ([`Environment::infer`]).
    pub environment: Option<Environment>,
    /// Overrides the environment's default; `--read-only` / `--writable` win.
    #[serde(alias = "readOnly")]
    pub read_only: Option<bool>,
}

/// How much protection a connection gets: `Prod` and `Unknown` start
/// read-only, `Prod` also asks for typed confirms.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Local,
    Staging,
    Prod,
    Unknown,
}

impl Environment {
    /// `Local` for a loopback host (`localhost`, `127.0.0.0/8`, `::1`) or a
    /// unix socket, `Unknown` otherwise. An SSH tunnel to production on
    /// `localhost` is inferred as `Local`: give such a profile an explicit
    /// `environment`.
    #[must_use]
    pub fn infer(addr: &redis::ConnectionAddr) -> Self {
        let host = match addr {
            redis::ConnectionAddr::Tcp(host, _) | redis::ConnectionAddr::TcpTls { host, .. } => {
                host
            }
            redis::ConnectionAddr::Unix(_) => return Self::Local,
            #[allow(unreachable_patterns)]
            _ => return Self::Unknown,
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if loopback {
            Self::Local
        } else {
            Self::Unknown
        }
    }

    /// Whether a session starts read-only without a flag or profile setting.
    #[must_use]
    pub fn default_read_only(self) -> bool {
        matches!(self, Self::Prod | Self::Unknown)
    }

    /// Upper-case name for the footer badge.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Staging => "STAGING",
            Self::Prod => "PROD",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Staging => "staging",
            Self::Prod => "prod",
            Self::Unknown => "unknown",
        })
    }
}

/// TLS settings of a profile. The `--tls-*` flags override them field by field.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsProfile {
    pub ca: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    #[serde(default)]
    pub insecure: bool,
}

/// Where a resolved setting came from, for `--print-target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A command-line flag, e.g. `--url`.
    Flag(&'static str),
    /// The named profile.
    Profile(String),
    /// The `REDIS_URL` environment variable.
    EnvVar,
    /// The connection URL itself (its path, scheme or password).
    Url,
    /// Nothing set it.
    Default,
    /// Inferred from the host (environment).
    Inferred,
    /// The environment's default (read-only).
    Environment,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flag(flag) => write!(f, "{flag}"),
            Self::Profile(name) => write!(f, "profile {name}"),
            Self::EnvVar => write!(f, "$REDIS_URL"),
            Self::Url => write!(f, "url"),
            Self::Default => write!(f, "default"),
            Self::Inferred => write!(f, "inferred from host"),
            Self::Environment => write!(f, "environment default"),
        }
    }
}

/// Where the password comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordSource {
    None,
    /// Embedded in the URL.
    Url,
    /// `--token`: prompted for before the TUI starts.
    Token,
    /// A profile's `password_command`, run before connecting.
    Command(String),
}

/// A configuration error: exits with code 3 (see `main.rs`).
#[derive(Debug)]
pub enum TargetError {
    UnknownProfile { name: String, defined: Vec<String> },
    InvalidUrl { source: Source, detail: String },
    Tls(String),
    PasswordCommand { command: String, detail: String },
    Config(String),
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProfile { name, defined } => {
                write!(f, "unknown profile `{name}`. ")?;
                if defined.is_empty() {
                    write!(f, "No profiles are defined in the config file")
                } else {
                    write!(f, "Defined profiles: {}", defined.join(", "))
                }
            }
            Self::InvalidUrl { source, detail } => {
                write!(f, "invalid Redis URL from {source}: {detail}")
            }
            Self::Tls(detail) => write!(f, "{detail}"),
            Self::PasswordCommand { command, detail } => {
                write!(f, "password_command `{command}` failed: {detail}")
            }
            Self::Config(detail) => write!(f, "invalid configuration: {detail}"),
        }
    }
}

impl std::error::Error for TargetError {}

/// The resolved connection target. Its `Debug` never shows a password: the
/// `redis` settings redact it.
#[derive(Clone, Debug)]
pub struct Target {
    /// The profile used, if any.
    pub profile: Option<String>,
    /// Address, DB and URL credentials (no `password_command` / token yet).
    info: ConnectionInfo,
    pub url_source: Source,
    pub db_source: Source,
    /// The `--tls-*` flags merged over the profile's `tls`.
    tls: TlsArgs,
    pub tls_source: Source,
    pub password: PasswordSource,
    pub environment: Environment,
    pub env_source: Source,
    /// Whether the session starts read-only.
    pub read_only: bool,
    pub read_only_source: Source,
}

/// Resolve the target from the flags, the config's `profiles` and the value
/// of `REDIS_URL`.
///
/// Order: `--url` > profile `url` > `REDIS_URL` > [`DEFAULT_URL`]; `--db` >
/// profile `db` > the URL's path; each `--tls-*` flag > profile `tls`;
/// `--token` > profile `password_command` > the URL's password.
///
/// # Errors
///
/// An unknown profile, a malformed URL, or a TLS client certificate without
/// its key (or the other way round).
pub fn resolve(
    cli: &Cli,
    profiles: &HashMap<String, Profile>,
    redis_url_env: Option<&str>,
) -> Result<Target, TargetError> {
    let name = cli.profile_name();
    let profile = match name {
        Some(name) => Some(profiles.get(name).ok_or_else(|| {
            let mut defined: Vec<String> = profiles.keys().cloned().collect();
            defined.sort();
            TargetError::UnknownProfile {
                name: name.to_owned(),
                defined,
            }
        })?),
        None => None,
    };
    let profile_source = || Source::Profile(name.unwrap_or_default().to_owned());

    let (url, url_source) = if let Some(url) = cli.url.as_deref() {
        (url, Source::Flag("--url"))
    } else if let Some(url) = profile.and_then(|p| p.url.as_deref()) {
        (url, profile_source())
    } else if let Some(url) = redis_url_env.filter(|u| !u.trim().is_empty()) {
        (url.trim(), Source::EnvVar)
    } else {
        (DEFAULT_URL, Source::Default)
    };
    let mut info = url
        .into_connection_info()
        .map_err(|e| TargetError::InvalidUrl {
            source: url_source.clone(),
            detail: e.to_string(),
        })?;

    let db_source = if let Some(db) = cli.db {
        info = with_db(info, db);
        Source::Flag("--db")
    } else if let Some(db) = profile.and_then(|p| p.db) {
        info = with_db(info, db);
        profile_source()
    } else if url_names_db(url) {
        Source::Url
    } else {
        Source::Default
    };

    let profile_tls = profile.and_then(|p| p.tls.as_ref());
    let tls = merge_tls(&cli.tls, profile_tls)?;
    let tls_source = if cli.tls.any() {
        Source::Flag("--tls-*")
    } else if profile_tls.is_some() {
        profile_source()
    } else if matches!(info.addr(), redis::ConnectionAddr::TcpTls { .. }) {
        Source::Url
    } else {
        Source::Default
    };

    let password = if cli.token {
        PasswordSource::Token
    } else if let Some(cmd) = profile.and_then(|p| p.password_command.clone()) {
        PasswordSource::Command(cmd)
    } else if info.redis_settings().password().is_some() {
        PasswordSource::Url
    } else {
        PasswordSource::None
    };

    let (environment, env_source) = match profile.and_then(|p| p.environment) {
        Some(env) => (env, profile_source()),
        None => (Environment::infer(info.addr()), Source::Inferred),
    };
    let (read_only, read_only_source) = if cli.read_only {
        (true, Source::Flag("--read-only"))
    } else if cli.writable {
        (false, Source::Flag("--writable"))
    } else if let Some(read_only) = profile.and_then(|p| p.read_only) {
        (read_only, profile_source())
    } else {
        (environment.default_read_only(), Source::Environment)
    };

    Ok(Target {
        profile: name.map(str::to_owned),
        environment,
        env_source,
        read_only,
        read_only_source,
        info,
        url_source,
        db_source,
        tls,
        tls_source,
        password,
    })
}

fn with_db(info: ConnectionInfo, db: i64) -> ConnectionInfo {
    let settings = info.redis_settings().clone().set_db(db);
    info.set_redis_settings(settings)
}

/// The URL picks a DB: `redis://host/3`, or `?db=3` on a unix socket.
fn url_names_db(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    if url.starts_with("unix:") || url.starts_with("redis+unix:") {
        return rest.contains("db=");
    }
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    rest.split_once('/')
        .is_some_and(|(_, path)| !path.trim_matches('/').is_empty())
}

/// `~/x` → `$HOME/x`; anything else unchanged.
fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    directories::BaseDirs::new()
        .map_or_else(|| path.to_path_buf(), |dirs| dirs.home_dir().join(rest))
}

/// The `--tls-*` flags win field by field over the profile's `tls`.
fn merge_tls(flags: &TlsArgs, profile: Option<&TlsProfile>) -> Result<TlsArgs, TargetError> {
    let Some(profile) = profile else {
        return Ok(flags.clone());
    };
    let pick = |flag: &Option<PathBuf>, prof: &Option<PathBuf>| {
        flag.clone().or_else(|| prof.as_deref().map(expand_home))
    };
    let merged = TlsArgs {
        tls_ca: pick(&flags.tls_ca, &profile.ca),
        tls_cert: pick(&flags.tls_cert, &profile.cert),
        tls_key: pick(&flags.tls_key, &profile.key),
        insecure: flags.insecure || profile.insecure,
    };
    if merged.tls_cert.is_some() != merged.tls_key.is_some() {
        return Err(TargetError::Tls(
            "a TLS client certificate needs its key (tls.cert and tls.key go together)".into(),
        ));
    }
    Ok(merged)
}

impl Target {
    /// Address and DB, without credentials.
    #[cfg(test)]
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    #[cfg(test)]
    pub fn tls(&self) -> &TlsArgs {
        &self.tls
    }

    /// The connection settings with the password applied: prompts for
    /// `--token`, or runs the profile's `password_command`. Run before the
    /// TUI starts.
    ///
    /// # Errors
    ///
    /// The prompt can't be read, the password command fails, or TLS settings
    /// are used without a `rediss://` URL.
    pub fn connection_info(&self) -> Result<ConnectionInfo> {
        let info = match &self.password {
            PasswordSource::Token => {
                let token = rpassword::prompt_password("Redis auth token: ")
                    .map_err(|e| eyre!("Failed to read token: {e}"))?;
                with_password(self.info.clone(), token)
            }
            PasswordSource::Command(cmd) => {
                with_password(self.info.clone(), run_password_command(cmd)?)
            }
            PasswordSource::None | PasswordSource::Url => self.info.clone(),
        };
        apply_tls_flags(info, &self.tls)
    }

    /// Builds the Redis client, including TLS certificates.
    ///
    /// # Errors
    ///
    /// See [`Target::connection_info`]; also fails if a certificate or key
    /// file can't be read.
    pub fn client(&self) -> Result<redis::Client> {
        let info = self.connection_info()?;
        let client = if self.tls.has_certificates() {
            redis::Client::build_with_tls(info, self.tls.load_certificates()?)
        } else {
            redis::Client::open(info)
        };
        client.map_err(|e| eyre!(e))
    }
}

impl Target {
    /// The `--print-target` report: one `name  value  (source)` row per
    /// setting. Opens no socket and doesn't run `password_command`; the
    /// password is masked.
    ///
    /// # Errors
    ///
    /// TLS settings with a URL that isn't `rediss://`.
    pub fn report(&self) -> Result<String> {
        let info = apply_tls_flags(self.info.clone(), &self.tls)?;
        let redis = info.redis_settings();
        let mut rows: Vec<(&str, String, Option<String>)> = Vec::new();
        if let Some(profile) = &self.profile {
            rows.push(("profile", profile.clone(), None));
        }
        rows.push((
            "target",
            crate::cli::connection_label(&info),
            Some(self.url_source.to_string()),
        ));
        rows.push((
            "db",
            redis.db().to_string(),
            Some(self.db_source.to_string()),
        ));
        rows.push((
            "tls",
            self.tls_summary(&info),
            Some(self.tls_source.to_string()),
        ));
        rows.push((
            "user",
            redis.username().unwrap_or("default").to_owned(),
            None,
        ));
        let password = match &self.password {
            PasswordSource::None => ("none".to_owned(), None),
            PasswordSource::Url => ("********".to_owned(), Some("url".to_owned())),
            PasswordSource::Token => ("prompted at start".to_owned(), Some("--token".to_owned())),
            PasswordSource::Command(cmd) => (
                format!("from `{cmd}` (not run)"),
                Some("password_command".to_owned()),
            ),
        };
        rows.push(("password", password.0, password.1));
        rows.push((
            "environment",
            self.environment.to_string(),
            Some(self.env_source.to_string()),
        ));
        rows.push((
            "read-only",
            if self.read_only { "yes" } else { "no" }.to_owned(),
            Some(self.read_only_source.to_string()),
        ));

        let width = rows
            .iter()
            .map(|(_, v, _)| v.chars().count())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for (name, value, source) in rows {
            let line = match source {
                Some(source) => format!("{name:<11} {value:<width$}  ({source})"),
                None => format!("{name:<11} {value}"),
            };
            out.push_str(line.trim_end());
            out.push('\n');
        }
        Ok(out)
    }

    /// `off`, or how the server certificate is checked and which client
    /// certificate is sent.
    fn tls_summary(&self, info: &ConnectionInfo) -> String {
        let redis::ConnectionAddr::TcpTls { insecure, .. } = info.addr() else {
            return "off".to_owned();
        };
        let mut summary = if *insecure {
            "on, certificate NOT verified".to_owned()
        } else if let Some(ca) = &self.tls.tls_ca {
            format!("on, verified with CA {}", ca.display())
        } else {
            "on, verified with the bundled roots".to_owned()
        };
        if let Some(cert) = &self.tls.tls_cert {
            let _ = write!(summary, ", client cert {}", cert.display());
        }
        summary
    }
}

/// Sets the password on the typed settings, whose `Debug` redacts it.
fn with_password(info: ConnectionInfo, password: String) -> ConnectionInfo {
    let settings = info.redis_settings().clone().set_password(password);
    info.set_redis_settings(settings)
}

/// Run `command` with `sh -c` (`cmd /C` on Windows) and return its stdout
/// minus one trailing newline. The output is a secret: it is never logged
/// and never part of an error.
///
/// # Errors
///
/// The command can't start, exits non-zero (the error carries its stderr),
/// prints nothing, or prints something that isn't UTF-8.
pub fn run_password_command(command: &str) -> Result<String, TargetError> {
    let fail = |detail: String| TargetError::PasswordCommand {
        command: command.to_owned(),
        detail,
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let output = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| fail(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(fail(if stderr.is_empty() {
            output.status.to_string()
        } else {
            format!("{}: {stderr}", output.status)
        }));
    }
    let mut password =
        String::from_utf8(output.stdout).map_err(|_| fail("its output is not UTF-8".into()))?;
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    if password.is_empty() {
        return Err(fail("it printed nothing".into()));
    }
    Ok(password)
}

#[cfg(test)]
#[allow(clippy::type_complexity)] // the resolution tables
mod tests {
    use std::collections::HashMap;

    use clap::Parser;
    use pretty_assertions::assert_eq;
    use redis::ConnectionAddr;

    use super::{resolve, run_password_command, PasswordSource, Profile, Source, TargetError};
    use crate::cli::Cli;

    fn profiles() -> HashMap<String, Profile> {
        HashMap::from([
            (
                "prod".to_owned(),
                Profile {
                    url: Some("redis://prod.example.com:6380/2".into()),
                    db: Some(5),
                    password_command: Some("printf s3; printf cret".into()),
                    ..Profile::default()
                },
            ),
            (
                "bare".to_owned(),
                Profile {
                    db: Some(7),
                    ..Profile::default()
                },
            ),
        ])
    }

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["rrover"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }

    fn host_port(info: &redis::ConnectionInfo) -> (String, u16) {
        match info.addr() {
            ConnectionAddr::Tcp(h, p)
            | ConnectionAddr::TcpTls {
                host: h, port: p, ..
            } => (h.clone(), *p),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn url_resolution_order() {
        let env = Some("redis://env.example.com:6390");
        // (args, REDIS_URL, expected host, port, source)
        let cases: &[(&[&str], Option<&str>, &str, u16, Source)] = &[
            (&[], None, "localhost", 6379, Source::Default),
            (&[], env, "env.example.com", 6390, Source::EnvVar),
            (&[], Some("  "), "localhost", 6379, Source::Default),
            (&["bare"], env, "env.example.com", 6390, Source::EnvVar),
            (
                &["prod"],
                env,
                "prod.example.com",
                6380,
                Source::Profile("prod".into()),
            ),
            (
                &["--profile", "prod"],
                None,
                "prod.example.com",
                6380,
                Source::Profile("prod".into()),
            ),
            (
                &["prod", "--url", "redis://flag.example.com:1"],
                env,
                "flag.example.com",
                1,
                Source::Flag("--url"),
            ),
        ];
        for (args, env, host, port, source) in cases {
            let t = resolve(&cli(args), &profiles(), *env).unwrap();
            assert_eq!(host_port(t.info()), ((*host).to_owned(), *port), "{args:?}");
            assert_eq!(&t.url_source, source, "{args:?}");
        }
    }

    #[test]
    fn db_resolution_order() {
        // (args, REDIS_URL, expected db, source)
        let cases: &[(&[&str], Option<&str>, i64, Source)] = &[
            (&[], None, 0, Source::Default),
            (&["--url", "redis://h/3"], None, 3, Source::Url),
            (
                &["--url", "redis://h/3", "--db", "4"],
                None,
                4,
                Source::Flag("--db"),
            ),
            (&["prod"], None, 5, Source::Profile("prod".into())),
            (&["prod", "--db", "1"], None, 1, Source::Flag("--db")),
            (
                &["bare"],
                Some("redis://h/9"),
                7,
                Source::Profile("bare".into()),
            ),
            (&[], Some("redis://h/9"), 9, Source::Url),
            (&["--url", "unix:///tmp/r.sock?db=2"], None, 2, Source::Url),
        ];
        for (args, env, db, source) in cases {
            let t = resolve(&cli(args), &profiles(), *env).unwrap();
            assert_eq!(t.info().redis_settings().db(), *db, "{args:?}");
            assert_eq!(&t.db_source, source, "{args:?}");
        }
    }

    #[test]
    fn tls_flags_win_over_the_profile() {
        let mut map = profiles();
        map.insert(
            "tls".into(),
            Profile {
                url: Some("rediss://tls.example.com:6380".into()),
                tls: Some(super::TlsProfile {
                    ca: Some("profile-ca.pem".into()),
                    cert: Some("c.pem".into()),
                    key: Some("k.pem".into()),
                    insecure: false,
                }),
                ..Profile::default()
            },
        );
        let t = resolve(&cli(&["tls"]), &map, None).unwrap();
        assert_eq!(t.tls().tls_ca.as_deref(), Some("profile-ca.pem".as_ref()));
        assert_eq!(t.tls_source, Source::Profile("tls".into()));

        let t = resolve(&cli(&["tls", "--tls-ca", "flag-ca.pem"]), &map, None).unwrap();
        assert_eq!(t.tls().tls_ca.as_deref(), Some("flag-ca.pem".as_ref()));
        assert_eq!(t.tls().tls_cert.as_deref(), Some("c.pem".as_ref()));
        assert_eq!(t.tls_source, Source::Flag("--tls-*"));

        let t = resolve(&cli(&["--url", "rediss://h:1"]), &map, None).unwrap();
        assert_eq!(t.tls_source, Source::Url);

        // A cert without its key is a config error, not a silent plain connection.
        map.insert(
            "half".into(),
            Profile {
                tls: Some(super::TlsProfile {
                    cert: Some("c.pem".into()),
                    ..super::TlsProfile::default()
                }),
                ..Profile::default()
            },
        );
        assert!(matches!(
            resolve(&cli(&["half"]), &map, None),
            Err(TargetError::Tls(_))
        ));
    }

    #[test]
    fn password_source_order() {
        let p = profiles();
        let source = |args: &[&str]| resolve(&cli(args), &p, None).unwrap().password;
        assert_eq!(source(&[]), PasswordSource::None);
        assert_eq!(source(&["--url", "redis://:pw@h"]), PasswordSource::Url);
        assert_eq!(
            source(&["prod"]),
            PasswordSource::Command("printf s3; printf cret".into())
        );
        assert_eq!(source(&["prod", "--token"]), PasswordSource::Token);
    }

    #[test]
    fn environment_is_inferred_from_the_host() {
        use super::Environment::{Local, Unknown};
        let cases = [
            ("redis://localhost:6379", Local),
            ("redis://LOCALHOST", Local),
            ("redis://127.0.0.1", Local),
            ("redis://127.8.9.10:7000", Local),
            ("redis://[::1]:6379", Local),
            ("unix:///tmp/redis.sock", Local),
            ("rediss://localhost:6380", Local),
            ("redis://10.0.0.5:6379", Unknown),
            ("redis://redis.example.com", Unknown),
            ("redis://localhost.example.com", Unknown),
        ];
        for (url, env) in cases {
            let t = resolve(&cli(&["--url", url]), &HashMap::new(), None).unwrap();
            assert_eq!(t.environment, env, "{url}");
            assert_eq!(t.env_source, Source::Inferred, "{url}");
        }
    }

    #[test]
    fn read_only_resolution_order() {
        use super::Environment;
        let map = HashMap::from([
            (
                "prod".to_owned(),
                Profile {
                    url: Some("redis://localhost".into()),
                    environment: Some(Environment::Prod),
                    ..Profile::default()
                },
            ),
            (
                "staging".to_owned(),
                Profile {
                    url: Some("redis://10.0.0.5".into()),
                    environment: Some(Environment::Staging),
                    ..Profile::default()
                },
            ),
            (
                "pinned".to_owned(),
                Profile {
                    url: Some("redis://10.0.0.5".into()),
                    read_only: Some(false),
                    ..Profile::default()
                },
            ),
        ]);
        // (args, environment, read-only, source)
        let cases: &[(&[&str], Environment, bool, Source)] = &[
            (&[], Environment::Local, false, Source::Environment),
            (
                &["--url", "redis://10.0.0.5"],
                Environment::Unknown,
                true,
                Source::Environment,
            ),
            (
                &["--url", "redis://10.0.0.5", "--writable"],
                Environment::Unknown,
                false,
                Source::Flag("--writable"),
            ),
            (&["prod"], Environment::Prod, true, Source::Environment),
            (
                &["prod", "--writable"],
                Environment::Prod,
                false,
                Source::Flag("--writable"),
            ),
            (
                &["prod", "--writable", "--read-only"],
                Environment::Prod,
                true,
                Source::Flag("--read-only"),
            ),
            (
                &["staging"],
                Environment::Staging,
                false,
                Source::Environment,
            ),
            (
                &["staging", "--read-only"],
                Environment::Staging,
                true,
                Source::Flag("--read-only"),
            ),
            (
                &["pinned"],
                Environment::Unknown,
                false,
                Source::Profile("pinned".into()),
            ),
            // The profile's environment wins over the host, even with --url.
            (
                &["prod", "--url", "redis://127.0.0.1"],
                Environment::Prod,
                true,
                Source::Environment,
            ),
        ];
        for (args, env, read_only, source) in cases {
            let t = resolve(&cli(args), &map, None).unwrap();
            assert_eq!(t.environment, *env, "{args:?}");
            assert_eq!(t.read_only, *read_only, "{args:?}");
            assert_eq!(&t.read_only_source, source, "{args:?}");
        }
    }

    #[test]
    fn unknown_profile_lists_the_defined_ones() {
        let err = resolve(&cli(&["nope"]), &profiles(), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown profile `nope`. Defined profiles: bare, prod"
        );
        let err = resolve(&cli(&["nope"]), &HashMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("No profiles are defined"), "{err}");
    }

    #[test]
    fn positional_and_flag_profile_conflict() {
        assert!(Cli::try_parse_from(["rrover", "a", "--profile", "b"]).is_err());
    }

    #[test]
    fn invalid_urls_name_their_source() {
        let err = resolve(&cli(&[]), &HashMap::new(), Some("http://x")).unwrap_err();
        assert!(err.to_string().contains("$REDIS_URL"), "{err}");
        let map = HashMap::from([(
            "bad".to_owned(),
            Profile {
                url: Some("nope://".into()),
                ..Profile::default()
            },
        )]);
        let err = resolve(&cli(&["bad"]), &map, None).unwrap_err();
        assert!(err.to_string().contains("profile bad"), "{err}");
    }

    #[test]
    fn profile_config_parses() {
        let p: HashMap<String, Profile> = json5::from_str(
            r#"{ prod: { url: "rediss://h:6380", db: 1, passwordCommand: "op read x",
                         tls: { ca: "~/ca.pem", insecure: true } } }"#,
        )
        .unwrap();
        let prod = &p["prod"];
        assert_eq!(prod.password_command.as_deref(), Some("op read x"));
        let env: Profile = json5::from_str(r#"{ environment: "prod", readOnly: false }"#).unwrap();
        assert_eq!(env.environment, Some(super::Environment::Prod));
        assert_eq!(env.read_only, Some(false));
        assert!(json5::from_str::<Profile>(r#"{ environment: "production" }"#).is_err());
        assert!(prod.tls.as_ref().unwrap().insecure);
        // A typo is an error, not a silently ignored setting.
        assert!(json5::from_str::<Profile>(r#"{ urll: "redis://h" }"#).is_err());
    }

    #[test]
    fn report_masks_the_password_and_names_sources() {
        let t = resolve(
            &cli(&["--url", "redis://alice:s3cret@db.example.com:6390/4"]),
            &HashMap::new(),
            None,
        )
        .unwrap();
        let report = t.report().unwrap();
        assert!(!report.contains("s3cret"), "{report}");
        assert_eq!(
            report,
            "target      redis://db.example.com:6390  (--url)\n\
             db          4                            (url)\n\
             tls         off                          (default)\n\
             user        alice\n\
             password    ********                     (url)\n\
             environment unknown                      (inferred from host)\n\
             read-only   yes                          (environment default)\n"
        );

        let t = resolve(&cli(&["prod"]), &profiles(), None).unwrap();
        let report = t.report().unwrap();
        assert!(report.starts_with("profile     prod\n"), "{report}");
        assert!(report.contains("(not run)"), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn password_command_trims_one_newline() {
        assert_eq!(run_password_command("printf 'pw\\n\\n'").unwrap(), "pw\n");
        assert_eq!(run_password_command("printf 'pw\\r\\n'").unwrap(), "pw");
        assert_eq!(run_password_command("printf pw").unwrap(), "pw");
    }

    #[cfg(unix)]
    #[test]
    fn password_command_failure_shows_stderr_not_stdout() {
        let err = run_password_command("printf s3; printf cret; echo locked >&2; exit 4")
            .unwrap_err()
            .to_string();
        assert!(err.contains("locked"), "{err}");
        assert!(!err.contains("s3cret"), "{err}");
        assert!(run_password_command("true")
            .unwrap_err()
            .to_string()
            .contains("printed nothing"));
    }

    #[cfg(unix)]
    #[test]
    fn password_never_shows_in_debug_output() {
        let t = resolve(&cli(&["prod"]), &profiles(), None).unwrap();
        let info = t.connection_info().unwrap();
        assert_eq!(info.redis_settings().password(), Some("s3cret"));
        assert!(!format!("{info:?}").contains("s3cret"));
        assert!(!format!("{t:?}").contains("s3cret"));
    }
}

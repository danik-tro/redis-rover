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
use redis::{cluster::ClusterClientBuilder, ConnectionInfo, IntoConnectionInfo};
use serde::Deserialize;

use crate::{
    cli::{apply_tls_flags, Cli, TlsArgs},
    redis_client::{
        conn::{client_at, ClusterClients},
        escape::printable,
        sentinel::{SentinelTarget, DEFAULT_PORT},
    },
};

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
    /// Inferred from the hosts when unset ([`Environment::infer`]).
    pub environment: Option<Environment>,
    /// Overrides the environment's default; `--read-only` / `--writable` win.
    #[serde(alias = "readOnly")]
    pub read_only: Option<bool>,
    /// `true`: connect as a cluster client. `false`: stay on the one node
    /// even if it is in a cluster. Unset: detect it from `INFO cluster`.
    pub cluster: Option<bool>,
    /// Connect through Sentinel instead of `url`.
    pub sentinel: Option<SentinelProfile>,
}

/// `sentinel: { hosts, master, password }` of a profile. Its `Debug` never
/// shows the password.
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SentinelProfile {
    /// `host:port`; the port defaults to 26379.
    pub hosts: Vec<String>,
    pub master: String,
    /// `AUTH` for the Sentinels. The master's password is the profile's
    /// `password_command` or `--token`.
    pub password: Option<String>,
}

impl fmt::Debug for SentinelProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentinelProfile")
            .field("hosts", &self.hosts)
            .field("master", &self.master)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Whether the session uses the cluster client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClusterMode {
    /// Ask the server (`INFO cluster`) after connecting.
    #[default]
    Auto,
    /// `redis-cluster://` or profile `cluster: true`.
    On,
    /// Profile `cluster: false`.
    Off,
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

    /// [`Environment::infer`] over every seed: `Local` only if all of them
    /// are loopback, since a session can reach any of them.
    #[must_use]
    pub fn infer_all<'a>(addrs: impl IntoIterator<Item = &'a redis::ConnectionAddr>) -> Self {
        addrs
            .into_iter()
            .map(Self::infer)
            .max_by_key(|env| env.strictness())
            .unwrap_or(Self::Unknown)
    }

    /// How much protection the environment gives: `Local` < `Staging` <
    /// `Unknown` (starts read-only) < `Prod` (also typed confirms).
    fn strictness(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Staging => 1,
            Self::Unknown => 2,
            Self::Prod => 3,
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
    /// Inferred from the `--url` host, because `--url` replaced the URL of
    /// the named profile and the host calls for more protection than the
    /// profile's settings.
    UrlOverridesProfile(String),
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
            Self::UrlOverridesProfile(name) => {
                write!(f, "inferred from the --url host, not profile {name}")
            }
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
    /// On a cluster, the first seed.
    info: ConnectionInfo,
    /// Every seed of a `redis-cluster://` URL, the first one included.
    /// Empty for any other URL.
    seeds: Vec<redis::ConnectionAddr>,
    /// Set when the master is found through Sentinel; `info` then holds the
    /// credentials and DB, and its address is only a placeholder until
    /// [`Target::resolve_master`].
    sentinel: Option<SentinelTarget>,
    pub cluster: ClusterMode,
    pub cluster_source: Source,
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

    // `--url` replaces a profile's Sentinels as it replaces its `url`.
    let profile_sentinel = checked_sentinel(profile)?.filter(|_| cli.url.is_none());
    let sentinel_url =
        profile_sentinel.map(|s| format!("redis+sentinel://{}/{}", s.hosts.join(","), s.master));

    let (url, url_source) = if let Some(url) = cli.url.as_deref() {
        (url, Source::Flag("--url"))
    } else if let Some(url) = sentinel_url.as_deref() {
        (url, profile_source())
    } else if let Some(url) = profile.and_then(|p| p.url.as_deref()) {
        (url, profile_source())
    } else if let Some(url) = redis_url_env.filter(|u| !u.trim().is_empty()) {
        (url.trim(), Source::EnvVar)
    } else {
        (DEFAULT_URL, Source::Default)
    };
    let ParsedUrl {
        mut info,
        seeds,
        names_db,
        sentinel,
    } = parse_target_url(url, &url_source, profile_sentinel)?;

    let db_source = if let Some(db) = cli.db {
        info = with_db(info, db);
        Source::Flag("--db")
    } else if let Some(db) = profile.and_then(|p| p.db) {
        info = with_db(info, db);
        profile_source()
    } else if names_db {
        Source::Url
    } else {
        Source::Default
    };

    let profile_tls = profile.and_then(|p| p.tls.as_ref());
    let tls = merge_tls(&cli.tls, profile_tls)?;
    let tls_source = tls_source(cli, profile_tls.is_some(), &info, &profile_source);

    let password = password_source(cli, profile, &info);

    // `--url` replaced the profile's own URL: the profile describes another
    // server, so its `environment` and `read_only: false` can only add
    // protection here, never take it away (`rrover local --url prod-db`
    // must not come up as LOCAL and writable).
    let url_replaced =
        cli.url.is_some() && profile.is_some_and(|p| p.url.is_some() || p.sentinel.is_some());
    let override_source = || Source::UrlOverridesProfile(name.unwrap_or_default().to_owned());
    let inferred = infer_environment(&info, &seeds, sentinel.as_ref());

    // A replaced URL is another server: the profile's `cluster` says nothing
    // about it.
    let profile_cluster = profile.and_then(|p| p.cluster).filter(|_| !url_replaced);
    let (cluster, cluster_source) = match (seeds.is_empty(), profile_cluster) {
        (_, Some(true)) if sentinel.is_some() => {
            return Err(TargetError::Config(
                "a Sentinel target can't also be a cluster".to_owned(),
            ));
        }
        _ if sentinel.is_some() => (ClusterMode::Off, Source::Url),
        (false, _) => (ClusterMode::On, Source::Url),
        (true, Some(true)) => (ClusterMode::On, profile_source()),
        (true, Some(false)) => (ClusterMode::Off, profile_source()),
        (true, None) => (ClusterMode::Auto, Source::Default),
    };
    if cluster == ClusterMode::On && info.redis_settings().db() != 0 {
        return Err(TargetError::Config(
            "Redis Cluster only has DB 0".to_owned(),
        ));
    }
    let (environment, env_source) = match profile.and_then(|p| p.environment) {
        Some(env) if url_replaced && inferred.strictness() > env.strictness() => {
            (inferred, override_source())
        }
        Some(env) => (env, profile_source()),
        None => (inferred, Source::Inferred),
    };
    let profile_read_only = profile
        .and_then(|p| p.read_only)
        .filter(|read_only| *read_only || !url_replaced || !environment.default_read_only());
    let (read_only, read_only_source) =
        read_only_choice(cli, profile_read_only, environment, &profile_source);

    Ok(Target {
        profile: name.map(str::to_owned),
        environment,
        env_source,
        read_only,
        read_only_source,
        info,
        seeds,
        sentinel,
        cluster,
        cluster_source,
        url_source,
        db_source,
        tls,
        tls_source,
        password,
    })
}

/// The profile's `sentinel`, refused next to a `url`.
fn checked_sentinel(profile: Option<&Profile>) -> Result<Option<&SentinelProfile>, TargetError> {
    let sentinel = profile.and_then(|p| p.sentinel.as_ref());
    if sentinel.is_some() && profile.is_some_and(|p| p.url.is_some()) {
        return Err(TargetError::Config(
            "a profile takes `url` or `sentinel`, not both".to_owned(),
        ));
    }
    Ok(sentinel)
}

/// What the resolved URL says about the connection.
struct ParsedUrl {
    info: ConnectionInfo,
    seeds: Vec<redis::ConnectionAddr>,
    names_db: bool,
    sentinel: Option<SentinelTarget>,
}

/// Parse `url` of any supported scheme. `profile` gives a Sentinel URL the
/// password for the Sentinels, which a URL doesn't carry.
fn parse_target_url(
    url: &str,
    source: &Source,
    profile: Option<&SentinelProfile>,
) -> Result<ParsedUrl, TargetError> {
    let parts = parse_sentinel_url(url).map_err(|detail| TargetError::InvalidUrl {
        source: source.clone(),
        detail,
    })?;
    if let Some(parts) = parts {
        let password = profile.and_then(|s| s.password.clone());
        return Ok(ParsedUrl {
            info: parts.info,
            seeds: Vec::new(),
            names_db: parts.names_db,
            sentinel: Some(SentinelTarget::new(parts.hosts, parts.master, password)),
        });
    }
    let (info, seeds) = parse_url(url, source)?;
    Ok(ParsedUrl {
        info,
        seeds,
        names_db: url_names_db(url),
        sentinel: None,
    })
}

/// The environment the addresses of a target imply.
fn infer_environment(
    info: &ConnectionInfo,
    seeds: &[redis::ConnectionAddr],
    sentinel: Option<&SentinelTarget>,
) -> Environment {
    match sentinel {
        Some(sentinel) => Environment::infer_all(&sentinel.hosts),
        None if seeds.is_empty() => Environment::infer(info.addr()),
        None => Environment::infer_all(seeds),
    }
}

/// Where the TLS settings came from.
fn tls_source(
    cli: &Cli,
    in_profile: bool,
    info: &ConnectionInfo,
    profile_source: &dyn Fn() -> Source,
) -> Source {
    if cli.tls.any() {
        Source::Flag("--tls-*")
    } else if in_profile {
        profile_source()
    } else if matches!(info.addr(), redis::ConnectionAddr::TcpTls { .. }) {
        Source::Url
    } else {
        Source::Default
    }
}

/// `--read-only` > `--writable` > the profile > the environment's default.
fn read_only_choice(
    cli: &Cli,
    profile: Option<bool>,
    environment: Environment,
    profile_source: &dyn Fn() -> Source,
) -> (bool, Source) {
    if cli.read_only {
        (true, Source::Flag("--read-only"))
    } else if cli.writable {
        (false, Source::Flag("--writable"))
    } else if let Some(read_only) = profile {
        (read_only, profile_source())
    } else {
        (environment.default_read_only(), Source::Environment)
    }
}

/// `--token` > profile `password_command` > the URL's password.
fn password_source(cli: &Cli, profile: Option<&Profile>, info: &ConnectionInfo) -> PasswordSource {
    if cli.token {
        PasswordSource::Token
    } else if let Some(cmd) = profile.and_then(|p| p.password_command.clone()) {
        PasswordSource::Command(cmd)
    } else if info.redis_settings().password().is_some() {
        PasswordSource::Url
    } else {
        PasswordSource::None
    }
}

/// The connection settings of `url` (its first seed on a cluster URL) and
/// the address of every seed (empty for any other URL).
fn parse_url(
    url: &str,
    source: &Source,
) -> Result<(ConnectionInfo, Vec<redis::ConnectionAddr>), TargetError> {
    let invalid = |detail: String| TargetError::InvalidUrl {
        source: source.clone(),
        detail,
    };
    let seed_urls = cluster_seed_urls(url).map_err(invalid)?;
    let info = seed_urls
        .as_ref()
        .and_then(|seeds| seeds.first())
        .map_or(url, String::as_str)
        .into_connection_info()
        .map_err(|e| invalid(e.to_string()))?;
    let seeds = seed_urls
        .iter()
        .flatten()
        .map(|seed| {
            seed.as_str()
                .into_connection_info()
                .map(|seed| seed.addr().clone())
                .map_err(|e| invalid(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((info, seeds))
}

fn with_db(info: ConnectionInfo, db: i64) -> ConnectionInfo {
    let settings = info.redis_settings().clone().set_db(db);
    info.set_redis_settings(settings)
}

/// What a `redis+sentinel://` URL says.
struct SentinelUrl {
    /// Credentials and DB, with the first Sentinel as a placeholder address.
    info: ConnectionInfo,
    hosts: Vec<redis::ConnectionAddr>,
    master: String,
    names_db: bool,
}

/// `host:port`, with the Sentinel port added when `host` has none.
fn with_sentinel_port(host: &str) -> String {
    if host
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.ends_with(']'))
    {
        host.to_owned()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

/// `redis+sentinel://[user:pw@]h1:26379,h2/mymaster[/db]`; `None` for any
/// other scheme. The credentials are the master's. Errors never quote the
/// URL, which may hold a password.
fn parse_sentinel_url(url: &str) -> Result<Option<SentinelUrl>, String> {
    const SHAPE: &str = "a redis+sentinel:// URL looks like \
                         redis+sentinel://host:26379,host:26379/master/0";
    let Some(rest) = url.strip_prefix("redis+sentinel://") else {
        return Ok(None);
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_once('/').ok_or(SHAPE)?;
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (format!("{credentials}@"), hosts),
        None => (String::new(), authority),
    };
    let mut parts = path.trim_end_matches('/').split('/');
    let master = parts.next().filter(|m| !m.is_empty()).ok_or(SHAPE)?;
    let db = match parts.next() {
        Some(db) => Some(
            db.parse::<i64>()
                .map_err(|_| "the DB of a redis+sentinel:// URL must be a number".to_owned())?,
        ),
        None => None,
    };
    if parts.next().is_some() {
        return Err(SHAPE.to_owned());
    }
    let hosts: Vec<String> = hosts
        .split(',')
        .map(|host| match host.trim() {
            "" => Err(SHAPE.to_owned()),
            host => Ok(with_sentinel_port(host)),
        })
        .collect::<Result<_, _>>()?;
    let addrs = hosts
        .iter()
        .map(|host| {
            format!("redis://{host}")
                .as_str()
                .into_connection_info()
                .map(|info| info.addr().clone())
                .map_err(|_| "a Sentinel host in the URL is not valid".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let info = format!("redis://{credentials}{}/{}", hosts[0], db.unwrap_or(0))
        .as_str()
        .into_connection_info()
        .map_err(|_| "the credentials of the redis+sentinel:// URL are not valid".to_owned())?;
    Ok(Some(SentinelUrl {
        info,
        hosts: addrs,
        master: master.to_owned(),
        names_db: db.is_some(),
    }))
}

/// `redis-cluster://[user:pw@]h1:7000,h2:7001[/0]` as one `redis://` URL per
/// seed; `None` for any other scheme. Errors never quote the URL, which may
/// hold a password.
fn cluster_seed_urls(url: &str) -> Result<Option<Vec<String>>, String> {
    let Some(rest) = url.strip_prefix("redis-cluster://") else {
        return Ok(None);
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (format!("{credentials}@"), hosts),
        None => (String::new(), authority),
    };
    let seeds: Vec<String> = hosts
        .split(',')
        .map(|host| format!("redis://{credentials}{}{tail}", host.trim()))
        .collect();
    if hosts.split(',').any(|host| host.trim().is_empty()) {
        return Err("a redis-cluster:// URL needs host:port seeds, separated by commas".to_owned());
    }
    Ok(Some(seeds))
}

/// Whether `url` parses as a connection URL, `redis-cluster://` included.
///
/// # Errors
///
/// The reason it doesn't, without the URL itself.
pub fn validate_url(url: &str) -> Result<(), String> {
    if parse_sentinel_url(url)?.is_some() {
        return Ok(());
    }
    match cluster_seed_urls(url)? {
        Some(seeds) => seeds.iter().try_for_each(|seed| {
            seed.as_str()
                .into_connection_info()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }),
        None => url
            .into_connection_info()
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
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

    /// The client plus, unless the profile turned it off, a cluster client
    /// over the seeds. Runs the password prompt or command once. In
    /// [`ClusterMode::Auto`] a target that can't be a cluster (a unix socket,
    /// a DB other than 0) gets none.
    ///
    /// # Errors
    ///
    /// See [`Target::client`]; in [`ClusterMode::On`], also an invalid
    /// cluster configuration.
    pub fn clients(&self) -> Result<(redis::Client, Option<ClusterClients>)> {
        let info = self.connection_info()?;
        let client = if self.tls.has_certificates() {
            redis::Client::build_with_tls(info.clone(), self.tls.load_certificates()?)
        } else {
            redis::Client::open(info.clone())
        }
        .map_err(|e| eyre!(e))?;
        if self.cluster == ClusterMode::Off {
            return Ok((client, None));
        }
        let cluster = self.cluster_clients(&info);
        match (self.cluster, cluster) {
            (_, Ok(cluster)) => Ok((client, Some(cluster))),
            (ClusterMode::On, Err(err)) => Err(err),
            (_, Err(err)) => {
                log::debug!("No cluster client for this target: {err}");
                Ok((client, None))
            }
        }
    }

    /// The client for the master Sentinel names now, or `client` itself on
    /// a target without Sentinel. A master that isn't on loopback tightens an
    /// inferred environment: Sentinels reached through a local tunnel say
    /// nothing about where the data is.
    ///
    /// # Errors
    ///
    /// Sentinel can't name a reachable master.
    pub async fn resolve_master(&mut self, client: redis::Client) -> Result<redis::Client> {
        let Some(sentinel) = &self.sentinel else {
            return Ok(client);
        };
        let within = crate::config::get().connection.connect_timeout();
        let (host, port) = sentinel.resolve(&client, within).await.map_err(|e| {
            eyre!(
                "Sentinel could not name the master `{}`: {e}",
                sentinel.master
            )
        })?;
        let master = client_at(&client, &host, port).map_err(|e| eyre!(e))?;
        self.tighten_for(master.get_connection_info().addr());
        Ok(master)
    }

    /// Apply what `addr` implies to an environment inferred from elsewhere.
    fn tighten_for(&mut self, addr: &redis::ConnectionAddr) {
        let at_master = Environment::infer(addr);
        if self.env_source != Source::Inferred
            || at_master.strictness() <= self.environment.strictness()
        {
            return;
        }
        self.environment = at_master;
        if self.read_only_source == Source::Environment {
            self.read_only = at_master.default_read_only();
        }
    }

    fn cluster_clients(&self, info: &ConnectionInfo) -> Result<ClusterClients> {
        if info.redis_settings().db() != 0 || matches!(info.addr(), redis::ConnectionAddr::Unix(_))
        {
            return Err(eyre!("not a cluster target"));
        }
        let nodes: Vec<ConnectionInfo> = if self.seeds.is_empty() {
            vec![info.clone()]
        } else {
            self.seeds
                .iter()
                .map(|addr| info.clone().set_addr(addr.clone()))
                .collect()
        };
        let builder = || -> Result<ClusterClientBuilder> {
            let builder = ClusterClientBuilder::new(nodes.clone());
            Ok(if self.tls.has_certificates() {
                builder.certs(self.tls.load_certificates()?)
            } else {
                builder
            })
        };
        Ok(ClusterClients {
            plain: builder()?.build().map_err(|e| eyre!(e))?,
            resp3: builder()?
                .use_protocol(redis::ProtocolVersion::RESP3)
                .build()
                .map_err(|e| eyre!(e))?,
        })
    }

    /// What the footer and `--probe` call this target (no credentials).
    #[must_use]
    pub fn label(&self, info: &ConnectionInfo) -> String {
        if let Some(sentinel) = &self.sentinel {
            // The address comes from a server's reply.
            return format!(
                "sentinel {} → {}",
                printable(&sentinel.master),
                printable(&info.addr().to_string())
            );
        }
        if self.seeds.is_empty() {
            return crate::cli::connection_label(info);
        }
        let hosts: Vec<String> = self.seeds.iter().map(ToString::to_string).collect();
        format!("redis-cluster://{}", hosts.join(","))
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
            self.sentinel
                .as_ref()
                .map_or_else(|| self.label(&info), SentinelTarget::url),
            Some(self.url_source.to_string()),
        ));
        // `Auto` is only known after connecting; see `--probe`.
        match self.cluster {
            ClusterMode::Auto => {}
            ClusterMode::Off if self.sentinel.is_some() => {}
            ClusterMode::On => rows.push((
                "cluster",
                "yes".to_owned(),
                Some(self.cluster_source.to_string()),
            )),
            ClusterMode::Off => rows.push((
                "cluster",
                "no (one node)".to_owned(),
                Some(self.cluster_source.to_string()),
            )),
        }
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
        if self
            .sentinel
            .as_ref()
            .is_some_and(SentinelTarget::has_password)
        {
            rows.push(("sentinel pw", "********".to_owned(), None));
        }
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

    #[test]
    fn sentinel_urls_name_the_hosts_master_and_db() {
        let cli = Cli::parse_from([
            "rrover",
            "--url",
            "redis+sentinel://user:s3cret@10.0.0.1:26380,10.0.0.2/mymaster/3",
        ]);
        let target = resolve(&cli, &HashMap::new(), None).unwrap();
        assert_eq!(target.info().redis_settings().db(), 3);
        assert_eq!(target.db_source, Source::Url);
        assert_eq!(target.cluster, super::ClusterMode::Off);
        let report = target.report().unwrap();
        assert!(report.contains("redis+sentinel://10.0.0.1:26380,10.0.0.2:26379/mymaster"));
        assert!(!report.contains("s3cret"), "{report}");
        assert!(report.contains("********"));
    }

    #[test]
    fn malformed_sentinel_urls_are_refused_without_quoting_them() {
        for url in [
            "redis+sentinel://h1:26379",
            "redis+sentinel://h1:26379/",
            "redis+sentinel://:pw-secret@h1/m/notadb",
            "redis+sentinel://h1,,h2/m",
            "redis+sentinel://h1/m/0/extra",
        ] {
            let Err(err) = super::parse_sentinel_url(url) else {
                panic!("accepted a malformed URL");
            };
            assert!(!err.contains("pw-secret"), "{err}");
        }
    }

    #[test]
    fn a_sentinel_profile_masks_its_password() {
        let profiles = HashMap::from([(
            "ha".to_owned(),
            Profile {
                sentinel: Some(super::SentinelProfile {
                    hosts: vec!["127.0.0.1:26379".into(), "127.0.0.2".into()],
                    master: "mymaster".into(),
                    password: Some("sentinel-secret".into()),
                }),
                ..Profile::default()
            },
        )]);
        let cli = Cli::parse_from(["rrover", "ha"]);
        let target = resolve(&cli, &profiles, None).unwrap();
        let report = target.report().unwrap();
        assert!(!report.contains("sentinel-secret"), "{report}");
        assert!(report.contains("sentinel pw"), "{report}");
        assert!(!format!("{target:?}").contains("sentinel-secret"));
        assert!(!format!("{profiles:?}").contains("sentinel-secret"));
    }

    #[test]
    fn a_profile_takes_url_or_sentinel() {
        let profiles = HashMap::from([(
            "both".to_owned(),
            Profile {
                url: Some("redis://h".into()),
                sentinel: Some(super::SentinelProfile {
                    hosts: vec!["h".into()],
                    master: "m".into(),
                    password: None,
                }),
                ..Profile::default()
            },
        )]);
        let cli = Cli::parse_from(["rrover", "both"]);
        assert!(matches!(
            resolve(&cli, &profiles, None),
            Err(TargetError::Config(_))
        ));
    }

    #[test]
    fn a_remote_master_tightens_loopback_sentinels() {
        let cli = Cli::parse_from(["rrover", "--url", "redis+sentinel://127.0.0.1/m"]);
        let mut target = resolve(&cli, &HashMap::new(), None).unwrap();
        assert_eq!(target.environment, super::Environment::Local);
        assert!(!target.read_only);
        target.tighten_for(&ConnectionAddr::Tcp("10.0.1.7".into(), 6379));
        assert_eq!(target.environment, super::Environment::Unknown);
        assert!(target.read_only);
    }

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
    #[allow(clippy::too_many_lines)] // one row per case
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
            (
                "local".to_owned(),
                Profile {
                    url: Some("redis://localhost".into()),
                    environment: Some(Environment::Local),
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
            // A stricter profile environment stays, even with --url.
            (
                &["prod", "--url", "redis://127.0.0.1"],
                Environment::Prod,
                true,
                Source::Environment,
            ),
            // #73: --url replaced the profile's URL with another host, so
            // the profile can't make it less protected.
            (
                &["local", "--url", "redis://prod-db:6379"],
                Environment::Unknown,
                true,
                Source::Environment,
            ),
            (
                &["pinned", "--url", "redis://10.0.0.9"],
                Environment::Unknown,
                true,
                Source::Environment,
            ),
            (
                &["local", "--url", "redis://prod-db:6379", "--writable"],
                Environment::Unknown,
                false,
                Source::Flag("--writable"),
            ),
            // Another loopback port is still local.
            (
                &["local", "--url", "redis://127.0.0.1:6390"],
                Environment::Local,
                false,
                Source::Profile("local".into()),
            ),
        ];
        for (args, env, read_only, source) in cases {
            let t = resolve(&cli(args), &map, None).unwrap();
            assert_eq!(t.environment, *env, "{args:?}");
            assert_eq!(t.read_only, *read_only, "{args:?}");
            assert_eq!(&t.read_only_source, source, "{args:?}");
        }

        let t = resolve(
            &cli(&["local", "--url", "redis://prod-db:6379"]),
            &map,
            None,
        )
        .unwrap();
        assert_eq!(t.env_source, Source::UrlOverridesProfile("local".into()));
        assert!(
            t.report()
                .unwrap()
                .contains("(inferred from the --url host, not profile local)"),
            "{}",
            t.report().unwrap()
        );
    }

    #[test]
    fn cluster_resolution_order() {
        use super::ClusterMode::{Auto, Off, On};
        let map = HashMap::from([
            (
                "on".to_owned(),
                Profile {
                    url: Some("redis://10.0.0.1:7000".into()),
                    cluster: Some(true),
                    ..Profile::default()
                },
            ),
            (
                "off".to_owned(),
                Profile {
                    url: Some("redis://10.0.0.1:7000".into()),
                    cluster: Some(false),
                    ..Profile::default()
                },
            ),
        ]);
        // (args, mode, source)
        let cases: &[(&[&str], super::ClusterMode, Source)] = &[
            (&[], Auto, Source::Default),
            (&["--url", "redis://h:1"], Auto, Source::Default),
            (&["--url", "redis-cluster://a:1,b:2"], On, Source::Url),
            (&["on"], On, Source::Profile("on".into())),
            (&["off"], Off, Source::Profile("off".into())),
            // `--url` replaced the profile's server.
            (&["on", "--url", "redis://h:1"], Auto, Source::Default),
            (&["off", "--url", "redis-cluster://a:1"], On, Source::Url),
        ];
        for (args, mode, source) in cases {
            let t = resolve(&cli(args), &map, None).unwrap();
            assert_eq!(t.cluster, *mode, "{args:?}");
            assert_eq!(&t.cluster_source, source, "{args:?}");
        }
    }

    #[test]
    fn cluster_url_keeps_every_seed_and_its_credentials() {
        let t = resolve(
            &cli(&[
                "--url",
                "redis-cluster://u:s3cret@10.0.0.1:7000,10.0.0.2:7001/0",
            ]),
            &HashMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(host_port(t.info()), ("10.0.0.1".to_owned(), 7000));
        assert_eq!(t.info().redis_settings().password(), Some("s3cret"));
        assert_eq!(t.seeds.len(), 2);
        let report = t.report().unwrap();
        assert!(!report.contains("s3cret"), "{report}");
        assert!(
            report.contains("redis-cluster://10.0.0.1:7000,10.0.0.2:7001"),
            "{report}"
        );
        assert!(report.contains("cluster"), "{report}");
    }

    #[test]
    fn bad_cluster_urls_are_refused_without_quoting_them() {
        for url in [
            "redis-cluster://",
            "redis-cluster://u:s3cret@a:1,,b:2",
            "redis-cluster://u:s3cret@a:notaport",
        ] {
            let err = resolve(&cli(&[]), &HashMap::new(), Some(url))
                .err()
                .unwrap_or_else(|| panic!("{url} accepted"))
                .to_string();
            assert!(!err.contains("s3cret"), "{err}");
        }
        let err = resolve(
            &cli(&["--url", "redis-cluster://a:1", "--db", "2"]),
            &HashMap::new(),
            None,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("DB 0"), "{err}");
    }

    #[test]
    fn a_cluster_is_local_only_if_every_seed_is_loopback() {
        use super::Environment::{Local, Unknown};
        let cases = [
            ("redis-cluster://127.0.0.1:7000,localhost:7001", Local),
            ("redis-cluster://127.0.0.1:7000,10.0.0.2:7001", Unknown),
            ("redis-cluster://10.0.0.2:7001,127.0.0.1:7000", Unknown),
        ];
        for (url, env) in cases {
            let t = resolve(&cli(&["--url", url]), &HashMap::new(), None).unwrap();
            assert_eq!(t.environment, env, "{url}");
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

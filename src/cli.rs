use std::path::{Path, PathBuf};

use clap::{Args, Parser, ValueHint};
use color_eyre::eyre::{bail, eyre, Result};
use redis::{ClientTlsConfig, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, TlsCertificates};
use rustls::pki_types::{pem::PemObject, CertificateDer};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about)]
#[allow(clippy::struct_excessive_bools)] // independent clap flags
pub struct Cli {
    #[arg(
        short,
        long,
        value_name = "FLOAT",
        help = "Tick rate, i.e. number of ticks per second",
        default_value_t = 4.0
    )]
    pub tick_rate: f64,

    #[arg(
        short,
        long,
        value_name = "FLOAT",
        help = "Frame rate, i.e. number of frames per second",
        default_value_t = 4.0
    )]
    pub frame_rate: f64,

    /// Connection profile from the config file (same as `--profile`).
    #[arg(value_name = "PROFILE", conflicts_with = "profile")]
    pub profile_arg: Option<String>,

    #[arg(
        long,
        value_name = "NAME",
        help = "Connection profile from the config file's `profiles`"
    )]
    pub profile: Option<String>,

    #[arg(
        short,
        long,
        value_name = "URL",
        help = "Redis connection URL. Default: the profile's url, then $REDIS_URL, then redis://localhost:6379",
        value_parser = parse_redis_url,
    )]
    pub url: Option<String>,

    #[arg(
        long,
        value_name = "N",
        help = "Logical database; overrides the URL path"
    )]
    pub db: Option<i64>,

    #[arg(
        long,
        help = "Prompt for an auth token securely (input is hidden). The token is used as the connection password."
    )]
    pub token: bool,

    #[arg(
        long,
        help = "Block all writes (add, edit, delete, TTL). Safe for browsing production."
    )]
    pub read_only: bool,

    #[arg(
        long,
        help = "Start writable on a prod or unknown host (they start read-only). --read-only wins"
    )]
    pub writable: bool,

    #[arg(
        long,
        help = "Print the resolved connection target (password masked) and exit without connecting"
    )]
    pub print_target: bool,

    #[arg(
        long,
        conflicts_with = "print_target",
        help = "Connect, print a short server report (PING, RESP3, INFO, role) and exit"
    )]
    pub probe: bool,

    #[command(flatten)]
    pub tls: TlsArgs,
}

/// TLS options. They only apply to `rediss://` URLs.
#[derive(Args, Debug, Default, Clone)]
pub struct TlsArgs {
    #[arg(
        long,
        value_name = "PATH",
        value_hint = ValueHint::FilePath,
        help = "PEM file with the CA certificate(s) used to verify the server. Replaces the built-in root certificates."
    )]
    pub tls_ca: Option<PathBuf>,

    #[arg(
        long,
        value_name = "PATH",
        value_hint = ValueHint::FilePath,
        requires = "tls_key",
        help = "PEM client certificate chain for mutual TLS (needs --tls-key)"
    )]
    pub tls_cert: Option<PathBuf>,

    #[arg(
        long,
        value_name = "PATH",
        value_hint = ValueHint::FilePath,
        requires = "tls_cert",
        help = "PEM client private key for mutual TLS (needs --tls-cert)"
    )]
    pub tls_key: Option<PathBuf>,

    #[arg(
        long,
        conflicts_with = "tls_ca",
        help = "Skip server certificate verification. Unsafe: only use for local or dev servers."
    )]
    pub insecure: bool,
}

impl TlsArgs {
    pub(crate) fn any(&self) -> bool {
        self.tls_ca.is_some() || self.tls_cert.is_some() || self.tls_key.is_some() || self.insecure
    }

    pub(crate) fn has_certificates(&self) -> bool {
        self.tls_ca.is_some() || self.tls_cert.is_some()
    }

    /// Reads the PEM files named by the flags.
    pub(crate) fn load_certificates(&self) -> Result<TlsCertificates> {
        let root_cert = self
            .tls_ca
            .as_deref()
            .map(|p| read_certs("--tls-ca", p))
            .transpose()?;
        let client_tls = match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => Some(ClientTlsConfig {
                client_cert: read_certs("--tls-cert", cert)?,
                client_key: read_pem("--tls-key", key)?,
            }),
            _ => None,
        };
        Ok(TlsCertificates {
            client_tls,
            root_cert,
        })
    }
}

fn read_pem(flag: &str, path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| eyre!("failed to read {flag} {}: {e}", path.display()))
}

/// Reads a PEM file and checks it has at least one certificate. Without this,
/// a wrong file gives an empty trust store and a confusing handshake error.
fn read_certs(flag: &str, path: &Path) -> Result<Vec<u8>> {
    let pem = read_pem(flag, path)?;
    let mut certs = CertificateDer::pem_slice_iter(&pem);
    match certs.next() {
        Some(Ok(_)) => Ok(pem),
        Some(Err(e)) => bail!("invalid {flag} {}: {e}", path.display()),
        None => bail!("{flag} {} contains no PEM certificates", path.display()),
    }
}

/// Checks that TLS flags are only used with a `rediss://` URL and applies
/// `--insecure` to the address.
pub(crate) fn apply_tls_flags(info: ConnectionInfo, tls: &TlsArgs) -> Result<ConnectionInfo> {
    if !tls.any() {
        return Ok(info);
    }
    let ConnectionAddr::TcpTls {
        host,
        port,
        insecure,
        tls_params,
    } = info.addr().clone()
    else {
        bail!("--tls-ca, --tls-cert, --tls-key and --insecure require a rediss:// URL");
    };
    Ok(info.set_addr(ConnectionAddr::TcpTls {
        host,
        port,
        insecure: insecure || tls.insecure,
        tls_params,
    }))
}

/// Validates a Redis URL at parse time so a malformed `--url` fails fast with a
/// descriptive error instead of a generic "something went wrong" later. Returns
/// the original string unchanged on success.
fn parse_redis_url(s: &str) -> Result<String, String> {
    s.into_connection_info()
        .map(|_| s.to_string())
        .map_err(|e| format!("invalid Redis URL: {e}"))
}

/// Human-readable connection target for display, e.g. `redis://localhost:6379`.
/// The DB is shown separately in the status bar, since it can be switched.
/// Built from the parsed address (never the raw `--url`), so credentials
/// embedded in the URL are never shown.
pub fn connection_label(info: &ConnectionInfo) -> String {
    let mut suffix = "";
    let addr = match info.addr() {
        ConnectionAddr::Tcp(host, port) => format!("redis://{host}:{port}"),
        ConnectionAddr::TcpTls {
            host,
            port,
            insecure,
            ..
        } => {
            if *insecure {
                suffix = " (insecure)";
            }
            format!("rediss://{host}:{port}")
        }
        ConnectionAddr::Unix(path) => format!("unix://{}", path.display()),
        #[allow(unreachable_patterns)]
        other => other.to_string(),
    };
    format!("{addr}{suffix}")
}

impl Cli {
    /// The profile named by the positional `PROFILE` or `--profile`.
    #[must_use]
    pub fn profile_name(&self) -> Option<&str> {
        self.profile_arg.as_deref().or(self.profile.as_deref())
    }

    /// Builds the Redis client from the flags alone (no profiles, no
    /// `REDIS_URL`), prompting for a token if `--token` was passed.
    ///
    /// # Errors
    ///
    /// Fails if a profile is named, the URL is invalid, the TLS flags are
    /// used without a `rediss://` URL, or a certificate / key file can't be
    /// read.
    pub fn client(&self) -> Result<redis::Client> {
        crate::target::resolve(self, &std::collections::HashMap::new(), None)?.client()
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use redis::{ConnectionAddr, IntoConnectionInfo};

    use super::{apply_tls_flags, connection_label, Cli, TlsArgs};

    fn insecure() -> TlsArgs {
        TlsArgs {
            insecure: true,
            ..TlsArgs::default()
        }
    }

    #[test]
    fn rediss_without_flags_is_unchanged() {
        let info = "rediss://example.com:6380".into_connection_info().unwrap();
        let out = apply_tls_flags(info.clone(), &TlsArgs::default()).unwrap();
        assert_eq!(out.addr(), info.addr());
    }

    #[test]
    fn insecure_flag_sets_insecure_addr() {
        let info = "rediss://example.com:6380".into_connection_info().unwrap();
        let out = apply_tls_flags(info, &insecure()).unwrap();
        assert!(matches!(
            out.addr(),
            ConnectionAddr::TcpTls { insecure: true, .. }
        ));
        assert_eq!(
            connection_label(&out),
            "rediss://example.com:6380 (insecure)"
        );
    }

    #[test]
    fn tls_flags_require_rediss_url() {
        let info = "redis://localhost:6379".into_connection_info().unwrap();
        let err = apply_tls_flags(info, &insecure()).unwrap_err();
        assert!(err.to_string().contains("rediss://"));

        let ca = TlsArgs {
            tls_ca: Some("ca.pem".into()),
            ..TlsArgs::default()
        };
        let info = "unix:///tmp/redis.sock".into_connection_info().unwrap();
        assert!(apply_tls_flags(info, &ca).is_err());
    }

    #[test]
    fn cert_and_key_must_be_given_together() {
        assert!(Cli::try_parse_from(["rrover", "--tls-cert", "c.pem"]).is_err());
        assert!(Cli::try_parse_from(["rrover", "--tls-key", "k.pem"]).is_err());
        assert!(
            Cli::try_parse_from(["rrover", "--tls-cert", "c.pem", "--tls-key", "k.pem"]).is_ok()
        );
    }

    #[test]
    fn read_only_flag_parses() {
        assert!(!Cli::try_parse_from(["rrover"]).unwrap().read_only);
        assert!(
            Cli::try_parse_from(["rrover", "--read-only"])
                .unwrap()
                .read_only
        );
    }

    #[test]
    fn insecure_conflicts_with_ca() {
        assert!(Cli::try_parse_from(["rrover", "--insecure", "--tls-ca", "ca.pem"]).is_err());
    }

    #[test]
    fn missing_ca_file_error_names_the_path() {
        let cli = Cli::try_parse_from([
            "rrover",
            "-u",
            "rediss://localhost:6380",
            "--tls-ca",
            "/nonexistent/rrover-ca.pem",
        ])
        .unwrap();
        let err = cli.client().unwrap_err().to_string();
        assert!(err.contains("--tls-ca"), "{err}");
        assert!(err.contains("/nonexistent/rrover-ca.pem"), "{err}");
    }

    #[test]
    fn invalid_ca_file_is_an_error() {
        let path = std::env::temp_dir().join(format!("rrover-bad-ca-{}.pem", std::process::id()));
        std::fs::write(&path, "not a certificate").unwrap();
        let cli = Cli::try_parse_from([
            "rrover",
            "-u",
            "rediss://localhost:6380",
            "--tls-ca",
            path.to_str().unwrap(),
        ])
        .unwrap();
        let result = cli.client();
        std::fs::remove_file(&path).ok();
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no PEM certificates"), "{err}");
    }

    #[test]
    fn label_includes_host_and_port_but_not_db() {
        let info = "redis://localhost:6379".into_connection_info().unwrap();
        assert_eq!(connection_label(&info), "redis://localhost:6379");

        let info = "redis://example.com:6380/3".into_connection_info().unwrap();
        assert_eq!(connection_label(&info), "redis://example.com:6380");
    }

    #[test]
    fn label_never_contains_credentials() {
        let info = "redis://user:s3cret@localhost:6379/1"
            .into_connection_info()
            .unwrap();
        let label = connection_label(&info);
        assert_eq!(label, "redis://localhost:6379");
        assert!(!label.contains("s3cret"));
    }
}

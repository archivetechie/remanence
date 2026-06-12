//! Server-side mutual-TLS setup for the daemon's TCP listener.
//!
//! The daemon loads operator-provisioned PEM material into tonic's server TLS
//! config. Certificate generation, rotation, revocation, and per-client
//! authorization remain outside this transport-hardening slice.

use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use remanence_state::DaemonTlsConfig;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// A resolved TCP/mTLS listener.
pub struct TlsListener {
    /// TCP address to bind.
    pub addr: SocketAddr,
    /// Server TLS configuration that requires a client certificate.
    pub tls: ServerTlsConfig,
}

/// Failure to load daemon TLS material.
#[derive(Debug, thiserror::Error)]
pub enum TlsConfigError {
    /// A configured PEM file could not be read.
    #[error("read TLS file {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The server private key is readable by group or other users.
    #[error(
        "TLS private key {path} has insecure permissions {mode:o}; expected no group/other bits"
    )]
    InsecurePrivateKeyPermissions {
        /// Path with unsafe mode bits.
        path: PathBuf,
        /// Unix mode bits observed on the key file.
        mode: u32,
    },

    /// A configured PEM file was readable but did not contain usable PEM.
    #[error("invalid TLS PEM {path}: {detail}")]
    InvalidPem {
        /// Path containing invalid PEM bytes.
        path: PathBuf,
        /// Validation failure detail.
        detail: String,
    },
}

/// Build a mutual-TLS server config from operator-provisioned PEM files.
pub fn load_server_tls(config: &DaemonTlsConfig) -> Result<ServerTlsConfig, TlsConfigError> {
    let cert = read_tls_file(&config.cert)?;
    validate_der_pem_blocks(&config.cert, &cert, &["CERTIFICATE"])?;
    let key = read_private_key_file(&config.key)?;
    validate_der_pem_blocks(
        &config.key,
        &key,
        &["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"],
    )?;
    let client_ca = read_tls_file(&config.client_ca)?;
    validate_der_pem_blocks(&config.client_ca, &client_ca, &["CERTIFICATE"])?;
    Ok(ServerTlsConfig::new()
        .identity(Identity::from_pem(cert, key))
        .client_ca_root(Certificate::from_pem(client_ca)))
}

fn read_private_key_file(path: &Path) -> Result<Vec<u8>, TlsConfigError> {
    reject_insecure_key_permissions(path)?;
    read_tls_file(path)
}

#[cfg(unix)]
fn reject_insecure_key_permissions(path: &Path) -> Result<(), TlsConfigError> {
    let metadata = std::fs::metadata(path).map_err(|source| TlsConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TlsConfigError::InsecurePrivateKeyPermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_key_permissions(_path: &Path) -> Result<(), TlsConfigError> {
    Ok(())
}

fn read_tls_file(path: &Path) -> Result<Vec<u8>, TlsConfigError> {
    std::fs::read(path).map_err(|source| TlsConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_der_pem_blocks(
    path: &Path,
    bytes: &[u8],
    labels: &[&str],
) -> Result<(), TlsConfigError> {
    let text = std::str::from_utf8(bytes).map_err(|err| TlsConfigError::InvalidPem {
        path: path.to_path_buf(),
        detail: format!("not UTF-8 PEM text: {err}"),
    })?;
    let mut found = false;
    for label in labels {
        let begin_marker = format!("-----BEGIN {label}-----");
        let end_marker = format!("-----END {label}-----");
        let mut rest = text;
        while let Some(begin) = rest.find(&begin_marker) {
            let after_begin = &rest[begin + begin_marker.len()..];
            let Some(end) = after_begin.find(&end_marker) else {
                return Err(TlsConfigError::InvalidPem {
                    path: path.to_path_buf(),
                    detail: format!("missing END marker for {label}"),
                });
            };
            let body = after_begin[..end]
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<String>();
            let der =
                STANDARD
                    .decode(body.as_bytes())
                    .map_err(|err| TlsConfigError::InvalidPem {
                        path: path.to_path_buf(),
                        detail: format!("{label} body is not base64: {err}"),
                    })?;
            if der.len() < 16 || der.first() != Some(&0x30) {
                return Err(TlsConfigError::InvalidPem {
                    path: path.to_path_buf(),
                    detail: format!("{label} body is not DER-like"),
                });
            }
            found = true;
            rest = &after_begin[end + end_marker.len()..];
        }
    }
    if !found {
        return Err(TlsConfigError::InvalidPem {
            path: path.to_path_buf(),
            detail: format!("missing PEM block with label {}", labels.join(" or ")),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn load_server_tls_errors_on_missing_file() {
        let config = DaemonTlsConfig {
            cert: "/nonexistent/s.crt".into(),
            key: "/nonexistent/s.key".into(),
            client_ca: "/nonexistent/ca.crt".into(),
        };
        let err = load_server_tls(&config).expect_err("missing cert file");
        assert!(matches!(err, TlsConfigError::Read { .. }));
    }

    #[test]
    fn load_server_tls_builds_from_readable_pem_bytes() {
        let dir = std::env::temp_dir().join(format!("rem-s2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write_pem = |name: &str, label: &str| {
            let path = dir.join(name);
            std::fs::write(
                &path,
                format!(
                    "-----BEGIN {label}-----\nMBACAQECAQECAQECAQECAQEA\n-----END {label}-----\n"
                ),
            )
            .unwrap();
            path
        };
        let key = write_pem("s.key", "PRIVATE KEY");
        #[cfg(unix)]
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = DaemonTlsConfig {
            cert: write_pem("s.crt", "CERTIFICATE"),
            key,
            client_ca: write_pem("ca.crt", "CERTIFICATE"),
        };
        let _tls = load_server_tls(&config).expect("builds ServerTlsConfig");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_server_tls_rejects_unparseable_pem_bytes() {
        let dir = std::env::temp_dir().join(format!("rem-s2-bad-pem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem = b"-----BEGIN CERTIFICATE-----\nQQ==\n-----END CERTIFICATE-----\n";
        let cert = dir.join("s.crt");
        let key = dir.join("s.key");
        let client_ca = dir.join("ca.crt");
        std::fs::write(&cert, pem).unwrap();
        std::fs::write(
            &key,
            b"-----BEGIN PRIVATE KEY-----\nQQ==\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&client_ca, pem).unwrap();
        let config = DaemonTlsConfig {
            cert,
            key,
            client_ca,
        };

        let err = load_server_tls(&config).expect_err("garbage PEM must fail at load");

        assert!(matches!(err, TlsConfigError::InvalidPem { .. }), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn load_server_tls_rejects_group_or_world_readable_private_key() {
        let dir = std::env::temp_dir().join(format!("rem-s2-insecure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_pem =
            b"-----BEGIN CERTIFICATE-----\nMBACAQECAQECAQECAQECAQEA\n-----END CERTIFICATE-----\n";
        let key_pem =
            b"-----BEGIN PRIVATE KEY-----\nMBACAQECAQECAQECAQECAQEA\n-----END PRIVATE KEY-----\n";
        let cert = dir.join("s.crt");
        let key = dir.join("s.key");
        let client_ca = dir.join("ca.crt");
        std::fs::write(&cert, cert_pem).unwrap();
        std::fs::write(&key, key_pem).unwrap();
        std::fs::write(&client_ca, cert_pem).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config = DaemonTlsConfig {
            cert,
            key,
            client_ca,
        };

        let err = load_server_tls(&config).expect_err("insecure key mode");

        assert!(matches!(
            err,
            TlsConfigError::InsecurePrivateKeyPermissions { mode: 0o644, .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}

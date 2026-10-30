// BSD 2-Clause License
//
// Apprise - Push Notification Library.
// Copyright (c) 2026, Chris Caron <lead2gold@gmail.com>
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! Socket and TLS utilities for TCP transports.
//!
//! Mirrors the Python `apprise.utils.socket` module which provides:
//! - TCP keepalive configuration
//! - Timeout coercion (requests-compatible float or (connect, read) tuple)
//! - TLS context building helpers
//!
//! The full `SocketTransport` (connect, read, write, start_tls, reconnect)
//! is handled by `tokio::net::TcpStream` and `tokio-rustls` in the Rust port.
//! This module provides shared configuration helpers used across plugins that
//! need raw TCP connections (SMTP, IRC, XMPP, MQTT, etc.).

use std::time::Duration;

/// Default connection/read timeout in seconds (matches Python's default of 10.0).
pub const DEFAULT_TIMEOUT_SECS: f64 = 10.0;

/// Timeout specification matching Python's requests-compatible format.
///
/// - `Single(f64)` - both connect and read use the same timeout
/// - `Split { connect, read }` - separate connect and read timeouts
/// - `None` via the `Default` impl - uses `DEFAULT_TIMEOUT_SECS` for both
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutConfig {
  /// Both connect and read timeouts set to the same value.
  Single(f64),
  /// Separate connect and read timeouts (either may be None for no limit).
  Split { connect: Option<f64>, read: Option<f64> },
}

impl Default for TimeoutConfig {
  fn default() -> Self {
    TimeoutConfig::Single(DEFAULT_TIMEOUT_SECS)
  }
}

impl TimeoutConfig {
  /// Create a timeout config from a single value in seconds.
  pub fn from_secs(secs: f64) -> Result<Self, String> {
    if secs < 0.0 {
      return Err("timeout must be >= 0".to_string());
    }
    Ok(TimeoutConfig::Single(secs))
  }

  /// Create a split timeout config with separate connect and read values.
  pub fn split(connect: Option<f64>, read: Option<f64>) -> Result<Self, String> {
    if let Some(c) = connect {
      if c < 0.0 {
        return Err("connect timeout must be >= 0".to_string());
      }
    }
    if let Some(r) = read {
      if r < 0.0 {
        return Err("read timeout must be >= 0".to_string());
      }
    }
    Ok(TimeoutConfig::Split { connect, read })
  }

  /// Get the connect timeout as a `Duration`, or `None` for no limit.
  pub fn connect_timeout(&self) -> Option<Duration> {
    match self {
      TimeoutConfig::Single(secs) => Some(Duration::from_secs_f64(*secs)),
      TimeoutConfig::Split { connect, .. } => connect.map(Duration::from_secs_f64),
    }
  }

  /// Get the read timeout as a `Duration`, or `None` for no limit.
  pub fn read_timeout(&self) -> Option<Duration> {
    match self {
      TimeoutConfig::Single(secs) => Some(Duration::from_secs_f64(*secs)),
      TimeoutConfig::Split { read, .. } => read.map(Duration::from_secs_f64),
    }
  }
}

/// Configure TCP keepalive on a `tokio::net::TcpStream`.
///
/// Uses the `socket2` crate for cross-platform keepalive support.
///
/// Mirrors Python's `socket.setsockopt(SOL_SOCKET, SO_KEEPALIVE, 1)` call
/// in `SocketTransport.connect()`.
pub fn set_keepalive(stream: &tokio::net::TcpStream, interval_secs: u64) -> std::io::Result<()> {
  use socket2::SockRef;

  let sock_ref = SockRef::from(stream);
  let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(interval_secs));

  // Set keepalive interval if supported on this platform
  #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "windows",))]
  let keepalive = keepalive.with_interval(Duration::from_secs(interval_secs));

  sock_ref.set_tcp_keepalive(&keepalive)
}

/// Coerce a timeout value into a `TimeoutConfig`.
///
/// Accepts:
/// - A positive float (used for both connect and read)
/// - Zero (no timeout, immediate)
///
/// Mirrors Python's `SocketTransport._coerce_timeout()`.
pub fn coerce_timeout(secs: f64) -> Result<TimeoutConfig, String> {
  TimeoutConfig::from_secs(secs)
}

/// Upstream `URLBase.socket_connect_timeout`, in seconds.
pub const DEFAULT_SOCKET_CONNECT_TIMEOUT: f64 = 4.0;

/// Upstream `URLBase.socket_read_timeout`, in seconds.
pub const DEFAULT_SOCKET_READ_TIMEOUT: f64 = 4.0;

/// The socket connect and read timeouts of a URL: `?cto=` and `?rto=`, or
/// the upstream defaults of 4 seconds each. Values that are not a positive
/// number fall back to the default, as `ServiceOptions` does.
pub fn url_timeouts(url: &crate::utils::parse::ParsedUrl) -> TimeoutConfig {
  let value = |key: &str, default: f64| url.get(key).and_then(|v| v.trim().parse::<f64>().ok()).filter(|n| n.is_finite() && *n > 0.0).unwrap_or(default);
  TimeoutConfig::Split { connect: Some(value("cto", DEFAULT_SOCKET_CONNECT_TIMEOUT)), read: Some(value("rto", DEFAULT_SOCKET_READ_TIMEOUT)) }
}

/// A rustls connector for raw TCP plugins.
///
/// With `verify` set, the server certificate must chain to a Mozilla root
/// and match the name it is checked against (upstream `CERT_REQUIRED` with
/// `check_hostname`). Without it, any certificate is accepted (upstream
/// `CERT_NONE`, the `?verify=no` URL option).
pub fn tls_connector(verify: bool) -> tokio_rustls::TlsConnector {
  crate::ensure_crypto_provider();
  let config = if verify {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()
  } else {
    rustls::ClientConfig::builder().dangerous().with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyCertificate::new())).with_no_client_auth()
  };
  tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
}

/// Certificate verifier behind `?verify=no`: every certificate is accepted.
/// Handshake signatures are still checked so the session keys stay bound to
/// the certificate the server presented.
#[derive(Debug)]
struct AcceptAnyCertificate(rustls::crypto::WebPkiSupportedAlgorithms);

impl AcceptAnyCertificate {
  fn new() -> Self {
    Self(rustls::crypto::ring::default_provider().signature_verification_algorithms)
  }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
  fn verify_server_cert(
    &self,
    _end_entity: &rustls::pki_types::CertificateDer<'_>,
    _intermediates: &[rustls::pki_types::CertificateDer<'_>],
    _server_name: &rustls::pki_types::ServerName<'_>,
    _ocsp_response: &[u8],
    _now: rustls::pki_types::UnixTime,
  ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
    Ok(rustls::client::danger::ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &rustls::pki_types::CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
  }

  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &rustls::pki_types::CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
  }

  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    self.0.supported_schemes()
  }
}

/// A TLS acceptor for tests of the raw-socket plugins, with a self-signed
/// certificate for `localhost` and `127.0.0.1` (valid until 2126).
#[cfg(test)]
pub(crate) fn test_tls_acceptor() -> tokio_rustls::TlsAcceptor {
  use base64::Engine;
  const CERT_DER: &str = "MIIBmzCCAUGgAwIBAgIUHDSzYecy1X4n48MGhv8zBUVdsOgwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAzMDA1MDMxOFoYDzIxMjYxMDA2MDUwMzE4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATVYK0hJr30ola9SZxnmyK5EHVY+n9n3RlYQGXrRdR1+MTSEhP3IuKidzohfbKhcVvKXT9F+DZXHJX/NXMHVHzuo28wbTAdBgNVHQ4EFgQUn2wtLITa3gfGa/s6IYZPB7+YEcMwHwYDVR0jBBgwFoAUn2wtLITa3gfGa/s6IYZPB7+YEcMwDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZIzj0EAwIDSAAwRQIgfu3SM0e5ylzsHSoV34x8CrxccRR1kjKvO70IYa04Pc4CIQD9c+VNfOpXdGAZigNorHAbaxs3Tgtb4psOjrfWr8VJdg==";
  const KEY_DER: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg/HKD2wnsJF0wgJ7HrPuBTscl39R6RBDPk0kfyv0A/OWhRANCAATVYK0hJr30ola9SZxnmyK5EHVY+n9n3RlYQGXrRdR1+MTSEhP3IuKidzohfbKhcVvKXT9F+DZXHJX/NXMHVHzu";
  crate::ensure_crypto_provider();
  let decode = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).unwrap();
  let cert = rustls::pki_types::CertificateDer::from(decode(CERT_DER));
  let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(decode(KEY_DER)));
  let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert], key).unwrap();
  tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_timeout_config_default() {
    let cfg = TimeoutConfig::default();
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs_f64(10.0)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs_f64(10.0)));
  }

  #[test]
  fn test_timeout_config_single() {
    let cfg = TimeoutConfig::from_secs(5.0).unwrap();
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(5)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(5)));
  }

  #[test]
  fn test_timeout_config_split() {
    let cfg = TimeoutConfig::split(Some(3.0), Some(10.0)).unwrap();
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(3)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(10)));
  }

  #[test]
  fn test_timeout_config_split_none() {
    let cfg = TimeoutConfig::split(None, None).unwrap();
    assert_eq!(cfg.connect_timeout(), None);
    assert_eq!(cfg.read_timeout(), None);
  }

  #[test]
  fn test_timeout_config_negative_rejected() {
    assert!(TimeoutConfig::from_secs(-1.0).is_err());
    assert!(TimeoutConfig::split(Some(-1.0), None).is_err());
    assert!(TimeoutConfig::split(None, Some(-1.0)).is_err());
  }

  #[test]
  fn test_coerce_timeout() {
    let cfg = coerce_timeout(7.5).unwrap();
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs_f64(7.5)));
    assert!(coerce_timeout(-1.0).is_err());
  }

  #[test]
  fn test_timeout_config_zero() {
    let cfg = TimeoutConfig::from_secs(0.0).unwrap();
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(0)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(0)));
  }

  #[test]
  fn test_url_timeouts() {
    let parse = |u: &str| url_timeouts(&crate::utils::parse::ParsedUrl::parse(u).unwrap());
    let cfg = parse("irc://host/#chan");
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(4)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(4)));

    let cfg = parse("irc://host/#chan?cto=2.5&rto=20");
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs_f64(2.5)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(20)));

    // Unusable values keep the defaults
    let cfg = parse("irc://host/#chan?cto=0&rto=abc");
    assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(4)));
    assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(4)));
  }
}

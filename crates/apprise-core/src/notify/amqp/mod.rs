//! AMQP notifications.
//!
//! Publishes to brokers such as RabbitMQ, ActiveMQ Artemis, Apache Qpid or
//! Azure Service Bus, over AMQP 0-9-1 (most brokers, RabbitMQ's default) or
//! AMQP 1.0 (the OASIS standard). Select one with `?version=`; 0.9.1 is the
//! default.
//!
//!   amqp://user:password@hostname/target
//!   amqps://user:password@hostname:port/target1/target2
//!
//! AMQP 0-9-1 targets are routing keys: with the default exchange use a
//! queue name, or pick another exchange with `?exchange=`. AMQP 1.0 targets
//! are addresses; put ones with slashes (RabbitMQ's `/queues/name`) in `?to=`.

mod v091;
mod v10;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, ServiceOptions};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list, unquote};

const AMQP_PORT: u16 = 5672;
const AMQP_SECURE_PORT: u16 = 5671;

/// Upstream `socket_connect_timeout` and `socket_read_timeout` defaults.
const DEFAULT_TIMEOUT_SECS: f64 = 4.0;

/// AMQP 0-9-1 cannot carry longer names than this (short strings).
const SHORTSTR_MAX: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
  V091,
  V10,
}

impl Version {
  /// Accept `0.9`, `0-9-1`, `v1`, `1.0` and similar spellings, but nothing
  /// written around them.
  fn parse(value: &str) -> Option<Self> {
    let value = value.trim();
    let body = value.strip_prefix(['v', 'V']).unwrap_or(value);
    let well_formed = body.split(['.', '-']).all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
      return None;
    }
    let digits: String = body.chars().filter(char::is_ascii_digit).collect();
    match digits.as_str() {
      "0" | "09" | "091" => Some(Self::V091),
      "1" | "10" => Some(Self::V10),
      _ => None,
    }
  }

  fn as_str(self) -> &'static str {
    match self {
      Self::V091 => "0.9.1",
      Self::V10 => "1.0",
    }
  }
}

/// How to reach and log into the broker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Config {
  pub host: String,
  pub port: u16,
  pub secure: bool,
  pub verify_certificate: bool,
  /// No user means the protocol's default (0-9-1) or anonymous (1.0) login.
  pub user: Option<String>,
  pub password: Option<String>,
  pub vhost: Option<String>,
  pub connect_timeout: Duration,
  pub read_timeout: Duration,
  /// AMQP 0-9-1 exchange ("" is the default exchange).
  pub exchange: String,
  /// AMQP 0-9-1: a message no queue receives counts as a failure.
  pub mandatory: bool,
}

/// One message, built once and published to every target.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
  pub body: Vec<u8>,
  pub content_type: &'static str,
  pub app_id: String,
  /// Seconds since the epoch.
  pub timestamp: u64,
  pub persistent: bool,
  pub priority: Option<u8>,
  /// Expiry in milliseconds.
  pub ttl: Option<u32>,
  pub headers: BTreeMap<String, String>,
}

pub struct Amqp {
  version: Version,
  config: Config,
  targets: Vec<String>,
  persistent: bool,
  priority: Option<u8>,
  ttl: Option<u32>,
  headers: BTreeMap<String, String>,
  tags: Vec<String>,
}

impl Amqp {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let secure = url.schema == "amqps";
    let given = |key: &str| url.get(key).filter(|v| !v.is_empty());

    // version= wins over its ver= alias
    let version = match given("version").or_else(|| given("ver")) {
      None => Version::V091,
      Some(v) => match Version::parse(v) {
        Some(version) => version,
        None => {
          tracing::warn!("An invalid AMQP version ({}) was specified.", v);
          return None;
        }
      },
    };

    // Options only AMQP 0-9-1 understands
    let exchange = given("exchange");
    let mandatory = given("mandatory");
    if version == Version::V10 {
      for (key, value) in [("exchange", exchange), ("mandatory", mandatory)] {
        if value.is_some() {
          tracing::warn!("The AMQP {} option is only used by AMQP 0.9.1; ignoring it.", key);
        }
      }
    }
    let (exchange, mandatory) = match version {
      Version::V091 => (exchange.unwrap_or_default().to_string(), mandatory.is_none_or(parse_bool)),
      Version::V10 => (String::new(), true),
    };
    let vhost = given("vhost").map(String::from);

    if version == Version::V091 {
      for (key, value) in [("exchange", Some(exchange.as_str())), ("vhost", vhost.as_deref())] {
        if value.is_some_and(|v| v.len() > SHORTSTR_MAX) {
          tracing::warn!("The AMQP {} specified is longer than {} bytes.", key, SHORTSTR_MAX);
          return None;
        }
      }
    }

    let persistent = given("persistent").is_none_or(parse_bool);
    let priority = bounded("priority", given("priority"), u8::MAX.into()).ok()?.map(|v| v as u8);
    let ttl = bounded("ttl", given("ttl"), u32::MAX.into()).ok()?.map(|v| v as u32);

    // Extra headers come from +key=value arguments, unquoted once more as
    // upstream does
    let headers = url.qsd_add.iter().map(|(k, v)| (unquote(k), unquote(v))).filter(|(k, _)| !k.is_empty()).collect();

    // Path targets split on slashes; to= keeps them for AMQP 1.0 addresses
    let mut found = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      found.push(to.to_string());
    }
    let mut targets = parse_list(&found.join(" "));
    if version == Version::V091 {
      targets.retain(|t| {
        let fits = t.len() <= SHORTSTR_MAX;
        if !fits {
          tracing::warn!("Dropped AMQP routing key longer than {} bytes ({}).", SHORTSTR_MAX, t);
        }
        fits
      });
    }

    let options = ServiceOptions::from_url(url).unwrap_or_default();
    let config = Config {
      port: url.port.unwrap_or(if secure { AMQP_SECURE_PORT } else { AMQP_PORT }),
      host,
      secure,
      verify_certificate: url.verify_certificate(),
      user: url.user.clone().filter(|u| !u.is_empty()),
      password: url.password.clone(),
      vhost,
      connect_timeout: Duration::from_secs_f64(options.cto.unwrap_or(DEFAULT_TIMEOUT_SECS)),
      read_timeout: Duration::from_secs_f64(options.rto.unwrap_or(DEFAULT_TIMEOUT_SECS)),
      exchange,
      mandatory,
    };

    Some(Self { version, config, targets, persistent, priority, ttl, headers, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "AMQP",
      service_url: Some("https://www.amqp.org/"),
      setup_url: Some("https://appriseit.com/services/amqp/"),
      protocols: vec!["amqp", "amqps"],
      description: "Publish messages to an AMQP 0-9-1 or AMQP 1.0 broker.",
      attachment_support: false,
    }
  }

  fn message(&self, ctx: &NotifyContext) -> Message {
    Message {
      body: ctx.body.as_bytes().to_vec(),
      content_type: match ctx.body_format {
        NotifyFormat::Markdown => "text/markdown; charset=utf-8",
        NotifyFormat::Html => "text/html; charset=utf-8",
        _ => "text/plain; charset=utf-8",
      },
      app_id: ctx.asset.app_id.clone(),
      timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default(),
      persistent: self.persistent,
      priority: self.priority,
      ttl: self.ttl,
      headers: self.headers.clone(),
    }
  }
}

/// Parse an optional integer within `0..=max`.
fn bounded(key: &str, value: Option<&str>, max: i64) -> Result<Option<i64>, ()> {
  let Some(value) = value else { return Ok(None) };
  match value.trim().parse::<i64>() {
    Ok(n) if (0..=max).contains(&n) => Ok(Some(n)),
    _ => {
      tracing::warn!("An invalid AMQP {} ({}) was specified.", key, value);
      Err(())
    }
  }
}

#[async_trait]
impl Notify for Amqp {
  fn schemas(&self) -> &[&str] {
    &["amqp", "amqps"]
  }
  fn service_name(&self) -> &str {
    "AMQP"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    16_777_216
  }
  /// Text, Markdown and HTML go out as given, labelled with their type
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Text, NotifyFormat::Markdown, NotifyFormat::Html]
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // With an exchange (such as a fanout one) and no targets, publish once
    // with an empty routing key
    let targets = if self.targets.is_empty() && !self.config.exchange.is_empty() { vec![String::new()] } else { self.targets.clone() };
    if targets.is_empty() {
      tracing::warn!("There are no AMQP targets to notify.");
      return Ok(false);
    }
    // A retry skips targets that already accepted this message
    let targets: Vec<String> = targets.into_iter().filter(|t| !crate::delivery::is_delivered(t, false)).collect();
    if targets.is_empty() {
      return Ok(true);
    }

    let message = self.message(ctx);
    let url = format!("{}:{}", self.config.host, self.config.port);
    tracing::debug!("AMQP {} connect: {} (cert_verify={})", self.version.as_str(), url, self.config.verify_certificate);
    crate::notify::throttle().await;

    let mut publisher = match self.version {
      Version::V091 => Publisher::V091(v091::Publisher::new(self.config.clone())),
      Version::V10 => Publisher::V10(v10::Publisher::new(self.config.clone())),
    };
    if let Err(e) = publisher.connect().await {
      tracing::warn!("Could not connect to AMQP server {}.", url);
      tracing::debug!("AMQP Exception: {}", e);
      return Err(e);
    }

    let mut has_error = false;
    for target in &targets {
      let target_name = if target.is_empty() { format!("exchange {}", self.config.exchange) } else { target.clone() };
      crate::notify::throttle().await;
      tracing::debug!("AMQP publish to {}/{}", url, target_name);
      tracing::trace!("AMQP Payload: {}", ctx.body);
      match publisher.publish(target, &message).await {
        Ok(()) => {
          crate::delivery::mark_delivered(target, false);
          tracing::info!("Sent AMQP notification to {}.", target_name);
        }
        Err(e) => {
          // Move on to our next target
          tracing::warn!("Failed to publish AMQP notification to {}.", target_name);
          tracing::debug!("AMQP Exception: {}", e);
          has_error = true;
        }
      }
    }
    publisher.close().await;
    Ok(!has_error)
  }
}

/// The client for our protocol version.
enum Publisher {
  V091(v091::Publisher),
  V10(v10::Publisher),
}

impl Publisher {
  /// Connect and log in.
  async fn connect(&mut self) -> Result<(), NotifyError> {
    match self {
      Self::V091(p) => p.connect().await,
      Self::V10(p) => p.connect().await,
    }
  }

  /// Publish `message` to `target` and wait for the broker to accept it.
  async fn publish(&mut self, target: &str, message: &Message) -> Result<(), NotifyError> {
    match self {
      Self::V091(p) => p.publish(target, message).await,
      Self::V10(p) => p.publish(target, message).await,
    }
  }

  /// Close the connection; never fails.
  async fn close(&mut self) {
    match self {
      Self::V091(p) => p.close().await,
      Self::V10(p) => p.close().await,
    }
  }
}

fn amqp_error(msg: impl std::fmt::Display) -> NotifyError {
  NotifyError::Other(format!("AMQP: {}", msg))
}

// ── Transport ────────────────────────────────────────────────────────────────

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Io for T {}

/// Open a TCP connection to the broker, wrapped in TLS for amqps://.
async fn connect_stream(cfg: &Config) -> Result<Box<dyn Io>, NotifyError> {
  let connect = async {
    let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).await?;
    if !cfg.secure {
      return Ok::<Box<dyn Io>, NotifyError>(Box::new(tcp));
    }
    let name = rustls::pki_types::ServerName::try_from(cfg.host.clone()).map_err(|e| amqp_error(format!("invalid TLS host name: {}", e)))?;
    let tls = tls_connector(cfg.verify_certificate).connect(name, tcp).await?;
    Ok(Box::new(tls))
  };
  tokio::time::timeout(cfg.connect_timeout, connect).await.map_err(|_| amqp_error("timed out connecting to the broker"))?
}

fn tls_connector(verify: bool) -> tokio_rustls::TlsConnector {
  crate::ensure_crypto_provider();
  let builder = rustls::ClientConfig::builder();
  let config = if verify {
    // Upstream checks against certifi's Mozilla bundle
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    builder.with_root_certificates(roots).with_no_client_auth()
  } else {
    // verify=no accepts self-signed and otherwise unverified brokers
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider))).with_no_client_auth()
  };
  tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Skips certificate checks but still verifies the handshake signatures.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

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
    rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
  }

  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &rustls::pki_types::CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
  ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
  }

  fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
    self.0.signature_verification_algorithms.supported_schemes()
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  fn parse(url: &str) -> Option<Amqp> {
    ParsedUrl::parse(url).and_then(|p| Amqp::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "amqp://localhost",
      "amqp://localhost/queue",
      "amqp://user:pass@localhost:5000/queue1/queue2",
      "amqps://user@localhost/queue?verify=no",
      "amqp://localhost/?exchange=alerts",
      "amqp://localhost/key?exchange=amq.topic&mandatory=no&persistent=no&priority=5&ttl=60000&vhost=home&+source=nas",
      "amqp://localhost/?version=1.0&to=/queues/alerts",
      "amqps://user:pass@localhost/queue?ver=1&vhost=vhost:home",
    ] {
      assert!(from_url(url).is_some(), "should load: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let long = format!("amqp://localhost/queue?exchange={}", "x".repeat(256));
    for url in [
      "amqp://",
      "amqp://localhost/queue?version=2",
      "amqp://localhost/queue?priority=256",
      "amqp://localhost/queue?priority=-1",
      "amqp://localhost/queue?ttl=-1",
      "amqp://localhost/queue?ttl=soon",
      "amqp://localhost/queue?ttl=4294967296",
      long.as_str(),
    ] {
      assert!(from_url(url).is_none(), "should not load: {}", url);
    }
  }

  #[test]
  fn test_version_spelling() {
    for (value, expected) in [
      ("0.9.1", Version::V091),
      ("0.9", Version::V091),
      ("0-9-1", Version::V091),
      ("v0.9.1", Version::V091),
      ("0", Version::V091),
      ("1.0", Version::V10),
      ("1", Version::V10),
      ("V1", Version::V10),
      (" 1.0 ", Version::V10),
    ] {
      assert_eq!(Version::parse(value), Some(expected), "{}", value);
    }
    for value in ["2", "0.8", "", "abc", "0.9.1.1", "-1", "junk0.9.1x", "release-v1.0-final", "1..0", "v", "1.0.1"] {
      assert_eq!(Version::parse(value), None, "{}", value);
    }

    // version= wins over ver=, and either selects the protocol
    assert_eq!(parse("amqp://localhost/a?ver=1&version=0.9").unwrap().version, Version::V091);
    assert_eq!(parse("amqp://localhost/a?ver=1").unwrap().version, Version::V10);
    assert_eq!(parse("amqp://localhost/a").unwrap().version, Version::V091);
  }

  #[test]
  fn test_defaults() {
    let a = parse("amqp://localhost/queue").unwrap();
    assert_eq!(a.config.port, 5672);
    assert!(!a.config.secure);
    assert_eq!(a.config.exchange, "");
    assert!(a.config.vhost.is_none());
    assert!(a.config.mandatory);
    assert!(a.persistent);
    assert!(a.priority.is_none());
    assert!(a.ttl.is_none());
    assert!(a.headers.is_empty());
    assert!(a.config.user.is_none());
    assert_eq!(a.config.connect_timeout, Duration::from_secs(4));
    assert_eq!(a.config.read_timeout, Duration::from_secs(4));
    assert_eq!(parse("amqps://localhost/queue").unwrap().config.port, 5671);
  }

  #[test]
  fn test_options() {
    let a =
      parse("amqps://user:pass@example.com/q?exchange=amq.topic&vhost=home&persistent=no&mandatory=no&priority=9&ttl=0&+source=nas&verify=no&cto=2&rto=3")
        .unwrap();
    assert_eq!(
      a.config,
      Config {
        host: "example.com".into(),
        port: 5671,
        secure: true,
        verify_certificate: false,
        user: Some("user".into()),
        password: Some("pass".into()),
        vhost: Some("home".into()),
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(3),
        exchange: "amq.topic".into(),
        mandatory: false,
      }
    );
    assert!(!a.persistent);
    assert_eq!(a.priority, Some(9));
    assert_eq!(a.ttl, Some(0));
    assert_eq!(a.headers, BTreeMap::from([("source".to_string(), "nas".to_string())]));

    // Empty values fall back to the defaults
    let a = parse("amqp://localhost/q?exchange=&vhost=&priority=&ttl=").unwrap();
    assert_eq!(a.config.exchange, "");
    assert!(a.config.vhost.is_none());
    assert!(a.priority.is_none());
    assert!(a.ttl.is_none());

    assert_eq!(parse("amqp://localhost/q?priority=255&ttl=4294967295").unwrap().ttl, Some(u32::MAX));
  }

  #[test]
  fn test_v10_ignores_v091_options() {
    let a = parse("amqp://localhost/?to=/queues/q&version=1.0&exchange=x&mandatory=no").unwrap();
    assert_eq!(a.version, Version::V10);
    assert_eq!(a.config.exchange, "");
    assert!(a.config.mandatory);
  }

  #[test]
  fn test_shortstr_limits() {
    let long = "x".repeat(256);
    assert!(parse(&format!("amqp://localhost/q?vhost={}", long)).is_none());
    // Exactly 255 bytes is fine; multi-byte characters count as bytes
    assert_eq!(parse(&format!("amqp://localhost/q?vhost={}", "x".repeat(255))).unwrap().config.vhost.unwrap().len(), 255);
    assert!(parse(&format!("amqp://localhost/q?vhost={}", "%C3%A9".repeat(128))).is_none());

    // A routing key that is too long is set aside, not fatal
    let a = parse(&format!("amqp://localhost/ok/{}", long)).unwrap();
    assert_eq!(a.targets, vec!["ok"]);

    // AMQP 1.0 has no such limit
    let a = parse(&format!("amqp://localhost/{}?version=1.0&vhost={}", long, long)).unwrap();
    assert_eq!(a.targets, vec![long.clone()]);
    assert_eq!(a.config.vhost, Some(long));
  }

  #[test]
  fn test_targets() {
    let a = parse("amqp://localhost/a/b?to=c,/queues/d&exchange=ex&vhost=vh").unwrap();
    assert_eq!(a.targets, vec!["/queues/d", "a", "b", "c"]);
    let a = parse("amqp://localhost/q2/q1/q1?to=q3%20q2").unwrap();
    assert_eq!(a.targets, vec!["q1", "q2", "q3"]);
    assert!(parse("amqp://localhost").unwrap().targets.is_empty());
  }

  #[test]
  fn test_message() {
    let a = parse("amqp://localhost/q?priority=4&ttl=100&+source=nas").unwrap();
    let ctx = NotifyContext { body: "Title\r\nBody".into(), ..Default::default() };
    let m = a.message(&ctx);
    // Text, Markdown and HTML go out as given
    assert_eq!(a.notify_formats(), vec![NotifyFormat::Text, NotifyFormat::Markdown, NotifyFormat::Html]);
    assert_eq!(a.request_rate_per_sec(), 0.0);
    assert_eq!(m.body, b"Title\r\nBody");
    assert_eq!(m.content_type, "text/plain; charset=utf-8");
    assert_eq!(m.app_id, "Apprise");
    assert!(m.persistent);
    assert_eq!(m.priority, Some(4));
    assert_eq!(m.ttl, Some(100));
    assert_eq!(m.headers.get("source").map(String::as_str), Some("nas"));
    assert!(m.timestamp > 1_700_000_000);

    for (format, content_type) in [(NotifyFormat::Markdown, "text/markdown; charset=utf-8"), (NotifyFormat::Html, "text/html; charset=utf-8")] {
      let ctx = NotifyContext { body_format: format, ..Default::default() };
      assert_eq!(a.message(&ctx).content_type, content_type);
    }
  }

  #[test]
  fn test_static_details() {
    let details = Amqp::static_details();
    assert_eq!(details.service_name, "AMQP");
    assert_eq!(details.protocols, vec!["amqp", "amqps"]);
  }

  #[tokio::test]
  async fn test_send_without_targets() {
    let svc = from_url("amqp://localhost").unwrap();
    assert!(!svc.send(&NotifyContext::default()).await.unwrap());

    // AMQP 1.0 has no exchange to fall back on
    let svc = from_url("amqp://localhost/?exchange=x&ver=1").unwrap();
    assert!(!svc.send(&NotifyContext::default()).await.unwrap());
  }

  // ── Shared by the protocol tests ──────────────────────────────────────────

  pub(crate) const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBnDCCAUGgAwIBAgIUPCS+RKtw7xajrN16kep98xfBqxAwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAzMDA0NDU0OVoYDzIxMjYxMDA2
MDQ0NTQ5WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAS0CGqZFwexg2hPewlQ8WxMq8eaHFc+sQY3n5koWF/N3eWjjLI37CFd
Iqea/UdYy9V4WZ5XeMviGVIg3OfgBMkYo28wbTAdBgNVHQ4EFgQUXWTDmwmn/O/k
VHZKRNQeophBVcswHwYDVR0jBBgwFoAUXWTDmwmn/O/kVHZKRNQeophBVcswDwYD
VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI
zj0EAwIDSQAwRgIhAMpmNXQGLJrLp7ZH227z4TVhEDiEunIH4HXsm3GxhD/pAiEA
virMaIPPSUh7pupctR4Xjp3UOhdNHwtx2AjJR4jFLTg=
-----END CERTIFICATE-----";

  pub(crate) const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgGoKfO01j1GBO0mMW
Srs16SoFxdzu7A61wNt1m/503hihRANCAAS0CGqZFwexg2hPewlQ8WxMq8eaHFc+
sQY3n5koWF/N3eWjjLI37CFdIqea/UdYy9V4WZ5XeMviGVIg3OfgBMkY
-----END PRIVATE KEY-----";

  /// A TLS acceptor presenting the self-signed test certificate.
  pub(crate) fn test_acceptor() -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    crate::ensure_crypto_provider();
    let cert = CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert], key).unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
  }
}

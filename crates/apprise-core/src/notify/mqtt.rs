//! MQTT notifications.
//!
//! Upstream publishes through paho-mqtt. This module speaks the same packets
//! itself so it can offer every protocol version upstream accepts (3.1,
//! 3.1.1 and 5.0). As with paho, one connection is opened on the first
//! notification, kept for later ones, and closed with a DISCONNECT once the
//! notifier is dropped.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, ServiceOptions};
use crate::utils::parse::{ParsedUrl, parse_bool};

const MQTT_PORT: u16 = 1883;
const MQTT_SECURE_PORT: u16 = 8883;

/// Seconds the broker waits between packets before it drops us.
const KEEPALIVE_SECS: u16 = 30;

/// Upstream `socket_connect_timeout` and `socket_read_timeout` defaults.
const DEFAULT_TIMEOUT_SECS: f64 = 4.0;

/// Where to find the system root certificates. The bundled Mozilla roots are
/// used when none of them exist (upstream falls back to certifi).
const CA_CERTIFICATE_FILE_LOCATIONS: &[&str] = &[
  // Debian/Ubuntu/Gentoo etc.
  "/etc/ssl/certs/ca-certificates.crt",
  // Fedora/RHEL 6
  "/etc/pki/tls/certs/ca-bundle.crt",
  // OpenSUSE
  "/etc/ssl/ca-bundle.pem",
  // OpenELEC
  "/etc/pki/tls/cacert.pem",
  // CentOS/RHEL 7
  "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
  // macOS Homebrew; brew install ca-certificates
  "/usr/local/etc/ca-certificates/cert.pem",
];

// Control packet types (with the fixed flags PUBREL requires)
const CONNECT: u8 = 0x10;
const CONNACK: u8 = 0x20;
const PUBLISH: u8 = 0x30;
const PUBACK: u8 = 0x40;
const PUBREC: u8 = 0x50;
const PUBREL: u8 = 0x62;
const PUBCOMP: u8 = 0x70;
const PINGREQ: u8 = 0xC0;
const PINGRESP: u8 = 0xD0;
const DISCONNECT: u8 = 0xE0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
  V31,
  V311,
  V5,
}

impl Version {
  /// Accept `v3.1.1`, `311`, `v5.0` and similar by matching only the digits.
  fn parse(value: &str) -> Option<Self> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    match digits.as_str() {
      "311" => Some(Self::V311),
      "31" => Some(Self::V31),
      "5" | "50" => Some(Self::V5),
      _ => None,
    }
  }

  fn level(self) -> u8 {
    match self {
      Self::V31 => 3,
      Self::V311 => 4,
      Self::V5 => 5,
    }
  }
}

pub struct Mqtt {
  host: String,
  port: u16,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  /// Topics in the order they are published to.
  topics: Vec<String>,
  qos: u8,
  version: Version,
  client_id: Option<String>,
  session: bool,
  retain: bool,
  verify_certificate: bool,
  connect_timeout: Duration,
  read_timeout: Duration,
  tags: Vec<String>,
  state: Mutex<State>,
}

#[derive(Default)]
struct State {
  connection: Option<Connection>,
}

impl Mqtt {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let secure = url.schema == "mqtts";

    // Topics may contain slashes; only Apprise's list delimiters split them
    let path = urlencoding::decode(&collapse_slashes(&url.path)).map(|p| p.into_owned()).unwrap_or_default();
    let mut topics: Vec<String> = split_list(&path).map(String::from).collect();
    if let Some(to) = url.get("to") {
      topics.extend(split_list(to).map(String::from));
    }
    // Upstream keeps a sorted, unique list and publishes from its end
    topics.sort();
    topics.dedup();
    topics.reverse();

    let qos = match url.get("qos").map(str::trim).filter(|v| !v.is_empty()) {
      None => 0,
      Some(v) => match v.parse::<i64>() {
        Ok(n @ 0..=2) => n as u8,
        _ => {
          tracing::warn!("An invalid MQTT QOS ({}) was specified.", v);
          return None;
        }
      },
    };

    let version = match url.get("version").filter(|v| !v.is_empty()) {
      None => Version::V311,
      Some(v) => match Version::parse(v) {
        Some(version) => version,
        None => {
          tracing::warn!("An invalid MQTT Protocol version ({}) was specified.", v);
          return None;
        }
      },
    };

    let client_id = url.get("client_id").map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    // A session can only be resumed under a known client id
    let session = client_id.is_some() && url.get("session").is_some_and(parse_bool);
    let retain = url.get("retain").is_some_and(parse_bool);

    let options = ServiceOptions::from_url(url).unwrap_or_default();
    let connect_timeout = crate::utils::time::secs(options.cto.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let read_timeout = crate::utils::time::secs(options.rto.unwrap_or(DEFAULT_TIMEOUT_SECS));

    Some(Self {
      port: url.port.unwrap_or(if secure { MQTT_SECURE_PORT } else { MQTT_PORT }),
      host,
      secure,
      user: url.user.clone().filter(|u| !u.is_empty()),
      password: url.password.clone(),
      topics,
      qos,
      version,
      client_id,
      session,
      retain,
      verify_certificate: url.verify_certificate(),
      connect_timeout,
      read_timeout,
      tags: url.tags(),
      state: Mutex::new(State::default()),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "MQTT Notification",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/mqtt/"),
      protocols: vec!["mqtt", "mqtts"],
      description: "Publish messages to an MQTT broker.",
      attachment_support: false,
    }
  }

  /// Our connection, opened (or replaced) when the current one is unusable.
  async fn connection<'a>(&self, slot: &'a mut Option<Connection>) -> Result<&'a mut Connection, NotifyError> {
    if let Some(conn) = slot.as_mut() {
      let healthy = conn.is_open().await && (conn.last_io.elapsed() < Duration::from_secs(KEEPALIVE_SECS.into()) || conn.ping(self.read_timeout).await.is_ok());
      if !healthy {
        tracing::debug!("MQTT connection to {}:{} was lost; reconnecting", self.host, self.port);
        *slot = None;
      }
    }
    if slot.is_none() {
      *slot = Some(Connection::open(self).await?);
    }
    Ok(slot.as_mut().expect("connection was just opened"))
  }
}

#[async_trait]
impl Notify for Mqtt {
  fn schemas(&self) -> &[&str] {
    &["mqtt", "mqtts"]
  }
  fn service_name(&self) -> &str {
    "MQTT Notification"
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
    268_435_455
  }
  /// Seconds between two publishes
  fn request_rate_per_sec(&self) -> f64 {
    0.5
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.topics.is_empty() {
      tracing::warn!("There were no MQTT topics to notify.");
      return Ok(false);
    }

    let mut state = self.state.lock().await;
    for topic in &self.topics {
      // Skip a topic that already accepted this message so a retry does not
      // deliver it twice
      if crate::delivery::is_delivered(topic, false) {
        continue;
      }
      crate::notify::throttle().await;

      let url = format!("{}:{}/{}", self.host, self.port, topic);
      let conn = match self.connection(&mut state.connection).await {
        Ok(conn) => conn,
        Err(e) => {
          tracing::warn!("An MQTT connection could not be established for {}", url);
          tracing::debug!("MQTT Exception: {}", e);
          return Err(e);
        }
      };

      tracing::debug!("MQTT POST URL: {} (cert_verify={})", url, self.verify_certificate);
      tracing::trace!("MQTT Payload: {}", ctx.body);
      if let Err(e) = conn.publish(topic, ctx.body.as_bytes(), self.qos, self.retain, self.read_timeout).await {
        // The connection is in an unknown state now; start over next time
        state.connection = None;
        tracing::warn!("The MQTT message could not be delivered to {}", url);
        tracing::debug!("MQTT Exception: {}", e);
        return Err(e);
      }
      crate::delivery::mark_delivered(topic, false);
    }

    tracing::info!("Sent MQTT notification");
    Ok(true)
  }
}

// ── Connection ───────────────────────────────────────────────────────────────

enum Stream {
  Plain(TcpStream),
  Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Stream {
  async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    match self {
      Self::Plain(s) => s.read(buf).await,
      Self::Tls(s) => s.read(buf).await,
    }
  }

  async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
    match self {
      Self::Plain(s) => s.write_all(data).await,
      Self::Tls(s) => {
        s.write_all(data).await?;
        s.flush().await
      }
    }
  }

  /// Write `data` without waiting, for use where we cannot await (`Drop`).
  fn write_now(&mut self, data: &[u8]) {
    match self {
      Self::Plain(s) => {
        let _ = s.try_write(data);
      }
      Self::Tls(s) => {
        let (tcp, tls) = s.get_mut();
        if io::Write::write_all(&mut tls.writer(), data).is_ok() {
          tls.send_close_notify();
        }
        while tls.wants_write() {
          match tls.write_tls(&mut TryWriter(tcp)) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
          }
        }
      }
    }
  }
}

/// A non-blocking `io::Write` over a tokio socket.
struct TryWriter<'a>(&'a TcpStream);

impl io::Write for TryWriter<'_> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.0.try_write(buf)
  }
  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

struct Connection {
  stream: Stream,
  version: Version,
  /// Bytes received but not consumed as a packet yet.
  inbox: Vec<u8>,
  last_packet_id: u16,
  last_io: Instant,
}

impl Connection {
  async fn open(mqtt: &Mqtt) -> Result<Self, NotifyError> {
    let mut version = mqtt.version;
    // paho picks a random id for MQTT 3.1, which does not allow an empty one
    let mut client_id = mqtt.client_id.clone().unwrap_or_else(|| if version == Version::V31 { random_client_id() } else { String::new() });

    loop {
      let mut conn = Self::handshake(mqtt, version, &client_id).await?;
      match conn.read_packet(mqtt.read_timeout).await? {
        (CONNACK, body) if body.len() >= 2 => match body[1] {
          0 => return Ok(conn),
          // Like paho, fall back to MQTT 3.1 when the broker refuses 3.1.1
          1 if version == Version::V311 => version = Version::V31,
          // ...and to a random client id when an empty one is refused
          2 if version != Version::V5 && client_id.is_empty() => client_id = random_client_id(),
          code => return Err(NotifyError::Mqtt(format!("the broker refused the connection: {}", connack_reason(version, code)))),
        },
        (kind, _) => return Err(NotifyError::Mqtt(format!("unexpected packet 0x{:02x} instead of CONNACK", kind))),
      }
      if version == Version::V31 && client_id.is_empty() {
        client_id = random_client_id();
      }
    }
  }

  /// Connect, set up TLS when asked to, and send our CONNECT.
  async fn handshake(mqtt: &Mqtt, version: Version, client_id: &str) -> Result<Self, NotifyError> {
    let connect = async {
      let tcp = TcpStream::connect((mqtt.host.as_str(), mqtt.port)).await?;
      if !mqtt.secure {
        return Ok::<_, NotifyError>(Stream::Plain(tcp));
      }
      let name = rustls::pki_types::ServerName::try_from(mqtt.host.clone()).map_err(|e| NotifyError::Mqtt(format!("invalid TLS host name: {}", e)))?;
      let tls = tls_connector(mqtt.verify_certificate).connect(name, tcp).await?;
      Ok(Stream::Tls(Box::new(tls)))
    };
    let stream = timeout(mqtt.connect_timeout, connect).await.map_err(|_| NotifyError::Mqtt("timed out connecting to the broker".into()))??;

    let mut conn = Self { stream, version, inbox: Vec::new(), last_packet_id: 0, last_io: Instant::now() };
    let packet = connect_packet(version, client_id, !mqtt.session, mqtt.user.as_deref(), mqtt.password.as_deref());
    conn.write(&packet).await?;
    Ok(conn)
  }

  async fn write(&mut self, data: &[u8]) -> Result<(), NotifyError> {
    self.stream.write_all(data).await?;
    self.last_io = Instant::now();
    Ok(())
  }

  /// Read the next complete packet, returning its type and body.
  async fn read_packet(&mut self, wait: Duration) -> Result<(u8, Vec<u8>), NotifyError> {
    let read = async {
      loop {
        if let Some((header, body, used)) = parse_packet(&self.inbox)? {
          self.inbox.drain(..used);
          // PUBLISH carries flags in the low bits; every reply we expect does not
          let kind = if header & 0xF0 == PUBLISH { PUBLISH } else { header };
          return Ok((kind, body));
        }
        let mut buf = [0u8; 4096];
        let n = self.stream.read(&mut buf).await?;
        if n == 0 {
          return Err(NotifyError::Mqtt("the broker closed the connection".into()));
        }
        self.inbox.extend_from_slice(&buf[..n]);
      }
    };
    let packet = timeout(wait, read).await.map_err(|_| NotifyError::Mqtt("timed out waiting for the broker".into()))?;
    self.last_io = Instant::now();
    packet
  }

  /// Wait for the acknowledgement `kind` of packet `id`.
  async fn await_ack(&mut self, kind: u8, id: u16, wait: Duration) -> Result<(), NotifyError> {
    loop {
      let (got, body) = self.read_packet(wait).await?;
      if got == DISCONNECT {
        return Err(NotifyError::Mqtt("the broker closed the connection".into()));
      }
      if got != kind || body.len() < 2 || u16::from_be_bytes([body[0], body[1]]) != id {
        // Pings and stale acknowledgements are not what we wait for
        continue;
      }
      // MQTT 5 acknowledgements carry a reason code; 0x80 and up is a refusal
      if self.version == Version::V5 && body.len() > 2 && body[2] >= 0x80 {
        return Err(NotifyError::Mqtt(format!("the broker refused the message (reason 0x{:02x})", body[2])));
      }
      return Ok(());
    }
  }

  /// Publish `payload` and wait for the broker to take it, per its QoS.
  async fn publish(&mut self, topic: &str, payload: &[u8], qos: u8, retain: bool, wait: Duration) -> Result<(), NotifyError> {
    let id = if qos > 0 { self.next_packet_id() } else { 0 };
    self.write(&publish_packet(self.version, topic, payload, qos, retain, id)).await?;

    let flow = async {
      match qos {
        0 => Ok(()),
        1 => self.await_ack(PUBACK, id, wait).await,
        _ => {
          self.await_ack(PUBREC, id, wait).await?;
          let [hi, lo] = id.to_be_bytes();
          self.write(&[PUBREL, 2, hi, lo]).await?;
          self.await_ack(PUBCOMP, id, wait).await
        }
      }
    };
    // Upstream gives up once its read timeout passes without delivery
    timeout(wait, flow).await.map_err(|_| NotifyError::Mqtt("the message could not be delivered in time".into()))?
  }

  async fn ping(&mut self, wait: Duration) -> Result<(), NotifyError> {
    self.write(&[PINGREQ, 0]).await?;
    loop {
      match self.read_packet(wait).await? {
        (PINGRESP, _) => return Ok(()),
        (DISCONNECT, _) => return Err(NotifyError::Mqtt("the broker closed the connection".into())),
        _ => {}
      }
    }
  }

  /// Whether the broker still holds this connection open, without blocking.
  async fn is_open(&mut self) -> bool {
    let mut buf = [0u8; 1024];
    // A zero timeout still polls the read once, which reports a closed socket
    match timeout(Duration::ZERO, self.stream.read(&mut buf)).await {
      Err(_) => true,
      Ok(Ok(0)) | Ok(Err(_)) => false,
      Ok(Ok(n)) => {
        self.inbox.extend_from_slice(&buf[..n]);
        // A DISCONNECT from the broker ends the connection too
        !matches!(parse_packet(&self.inbox), Ok(Some((DISCONNECT, _, _))) | Err(_))
      }
    }
  }

  fn next_packet_id(&mut self) -> u16 {
    self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
    self.last_packet_id
  }
}

impl Drop for Connection {
  fn drop(&mut self) {
    // Let the broker know we are leaving (upstream does so in __del__)
    self.stream.write_now(&[DISCONNECT, 0]);
  }
}

// ── Packets ──────────────────────────────────────────────────────────────────

fn connect_packet(version: Version, client_id: &str, clean: bool, user: Option<&str>, password: Option<&str>) -> Vec<u8> {
  let mut var = Vec::new();
  put_str(&mut var, if version == Version::V31 { "MQIsdp" } else { "MQTT" });
  var.push(version.level());

  let mut flags = 0u8;
  if clean {
    flags |= 0x02;
  }
  if user.is_some() {
    flags |= 0x80;
    if password.is_some() {
      flags |= 0x40;
    }
  }
  var.push(flags);
  var.extend_from_slice(&KEEPALIVE_SECS.to_be_bytes());
  if version == Version::V5 {
    // No CONNECT properties
    var.push(0);
  }

  put_str(&mut var, client_id);
  if let Some(user) = user {
    put_str(&mut var, user);
    if let Some(password) = password {
      put_str(&mut var, password);
    }
  }
  frame(CONNECT, &var)
}

fn publish_packet(version: Version, topic: &str, payload: &[u8], qos: u8, retain: bool, id: u16) -> Vec<u8> {
  let mut var = Vec::with_capacity(topic.len() + payload.len() + 5);
  put_str(&mut var, topic);
  if qos > 0 {
    var.extend_from_slice(&id.to_be_bytes());
  }
  if version == Version::V5 {
    // No PUBLISH properties
    var.push(0);
  }
  var.extend_from_slice(payload);
  frame(PUBLISH | (qos << 1) | u8::from(retain), &var)
}

fn frame(header: u8, body: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(body.len() + 5);
  out.push(header);
  let mut len = body.len();
  loop {
    let mut byte = (len % 128) as u8;
    len /= 128;
    if len > 0 {
      byte |= 0x80;
    }
    out.push(byte);
    if len == 0 {
      break;
    }
  }
  out.extend_from_slice(body);
  out
}

fn put_str(out: &mut Vec<u8>, value: &str) {
  out.extend_from_slice(&(value.len() as u16).to_be_bytes());
  out.extend_from_slice(value.as_bytes());
}

/// Split one packet off the front of `data`: its header byte, its body, and
/// how many bytes it used. `None` means more bytes are needed.
fn parse_packet(data: &[u8]) -> Result<Option<(u8, Vec<u8>, usize)>, NotifyError> {
  let Some(&header) = data.first() else { return Ok(None) };
  let mut len = 0usize;
  let mut pos = 1;
  loop {
    let Some(&byte) = data.get(pos) else { return Ok(None) };
    len += usize::from(byte & 0x7F) << (7 * (pos - 1));
    pos += 1;
    if byte & 0x80 == 0 {
      break;
    }
    if pos > 4 {
      return Err(NotifyError::Mqtt("malformed packet length from the broker".into()));
    }
  }
  if data.len() < pos + len {
    return Ok(None);
  }
  Ok(Some((header, data[pos..pos + len].to_vec(), pos + len)))
}

fn connack_reason(version: Version, code: u8) -> String {
  let reason = match (version, code) {
    (Version::V5, _) => return format!("reason 0x{:02x}", code),
    (_, 1) => "unacceptable protocol version",
    (_, 2) => "client identifier rejected",
    (_, 3) => "server unavailable",
    (_, 4) => "bad user name or password",
    (_, 5) => "not authorized",
    _ => return format!("return code {}", code),
  };
  reason.to_string()
}

/// A 22 character client id, as paho generates for MQTT 3.1.
fn random_client_id() -> String {
  const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  (0..22).map(|_| CHARS[(rand::random::<u32>() % 62) as usize] as char).collect()
}

/// Split on Apprise's list delimiters (upstream `parse_list`).
fn split_list(value: &str) -> impl Iterator<Item = &str> {
  value.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).filter(|s| !s.is_empty())
}

fn collapse_slashes(path: &str) -> String {
  let mut out = String::with_capacity(path.len());
  for c in path.chars() {
    if !(c == '/' && (out.is_empty() || out.ends_with('/'))) {
      out.push(c);
    }
  }
  out
}

// ── TLS ──────────────────────────────────────────────────────────────────────

fn tls_connector(verify: bool) -> tokio_rustls::TlsConnector {
  crate::ensure_crypto_provider();
  let builder = rustls::ClientConfig::builder();
  let config = if verify {
    builder.with_root_certificates(root_store()).with_no_client_auth()
  } else {
    // verify=no accepts self-signed and otherwise unverified brokers
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider))).with_no_client_auth()
  };
  tokio_rustls::TlsConnector::from(Arc::new(config))
}

fn root_store() -> rustls::RootCertStore {
  use rustls::pki_types::{CertificateDer, pem::PemObject};

  let mut store = rustls::RootCertStore::empty();
  let system = CA_CERTIFICATE_FILE_LOCATIONS.iter().find(|p| std::path::Path::new(p).is_file());
  if let Some(certs) = system.and_then(|p| CertificateDer::pem_file_iter(p).ok()) {
    store.add_parsable_certificates(certs.flatten());
  }
  if store.is_empty() {
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
  }
  store
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
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use std::sync::Mutex as StdMutex;
  use tokio::net::TcpListener;

  fn parse(url: &str) -> Option<Mqtt> {
    ParsedUrl::parse(url).and_then(|p| Mqtt::from_url(&p))
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.to_string(), ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "mqtt://localhost",
      "mqtt://localhost/my/topic",
      "mqtts://localhost/topic",
      "mqtt://user@localhost/topic",
      "mqtt://user:pass@localhost:1234/topic",
      "mqtt://localhost/my/topic?qos=1&version=v3.1",
      "mqtt://localhost/t?version=v5.0&qos=2&retain=yes",
      "mqtt://user@localhost/my/topic?session=yes&client_id=apprise",
      "mqtt://localhost?to=my/topic",
    ] {
      assert!(from_url(url).is_some(), "should load: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "mqtt://",
      "mqtt://localhost?version=v1.0.0.0",
      "mqtt://localhost?version=abc",
      "mqtt://localhost?qos=123",
      "mqtt://localhost?qos=-1",
      "mqtt://localhost?qos=invalid",
      "mqtt://localhost?qos=1.0",
    ] {
      assert!(from_url(url).is_none(), "should not load: {}", url);
    }
  }

  #[test]
  fn test_defaults() {
    let m = parse("mqtt://localhost/my/topic").unwrap();
    assert_eq!(m.host, "localhost");
    assert_eq!(m.port, 1883);
    assert!(!m.secure);
    assert_eq!(m.qos, 0);
    assert_eq!(m.version, Version::V311);
    assert!(m.client_id.is_none());
    assert!(!m.session);
    assert!(!m.retain);
    assert_eq!(m.connect_timeout, Duration::from_secs(4));
    assert_eq!(m.read_timeout, Duration::from_secs(4));

    let m = parse("mqtts://localhost/my/topic").unwrap();
    assert!(m.secure);
    assert_eq!(m.port, 8883);
  }

  #[test]
  fn test_options() {
    let m = parse("mqtt://user:pass@localhost:1234/t?qos=+2&version=5&client_id=%20me%20&session=yes&retain=yes&cto=1.5&rto=2").unwrap();
    assert_eq!(m.port, 1234);
    assert_eq!(m.user.as_deref(), Some("user"));
    assert_eq!(m.password.as_deref(), Some("pass"));
    assert_eq!(m.qos, 2);
    assert_eq!(m.version, Version::V5);
    assert_eq!(m.client_id.as_deref(), Some("me"));
    assert!(m.session);
    assert!(m.retain);
    assert_eq!(m.connect_timeout, Duration::from_secs_f64(1.5));
    assert_eq!(m.read_timeout, Duration::from_secs(2));

    // A session needs a client id
    assert!(!parse("mqtt://localhost/t?session=yes").unwrap().session);
    assert!(parse("mqtt://localhost/t?session=yes&client_id=%20").unwrap().client_id.is_none());

    for (value, version) in [("v3.1.1", Version::V311), ("311", Version::V311), ("3.1", Version::V31), ("v5.0", Version::V5), ("50", Version::V5)] {
      assert_eq!(parse(&format!("mqtt://localhost/t?version={}", value)).unwrap().version, version, "{}", value);
    }
  }

  #[test]
  fn test_topics() {
    // Sorted and unique, then published from the end of the list
    let m = parse("mqtt://localhost/my/topic,my/other/topic").unwrap();
    assert_eq!(m.topics, vec!["my/topic", "my/other/topic"]);

    let m = parse("mqtt://localhost/b/c,a%2Cz/x%20y?to=d;a").unwrap();
    assert_eq!(m.topics, vec!["z/x", "y", "d", "b/c", "a"]);

    // Repeated slashes collapse, a trailing one is kept, %2F is a slash
    assert_eq!(parse("mqtt://localhost//a//b").unwrap().topics, vec!["a/b"]);
    assert_eq!(parse("mqtt://localhost/my/topic/").unwrap().topics, vec!["my/topic/"]);
    assert_eq!(parse("mqtt://localhost/%2Fa").unwrap().topics, vec!["/a"]);
    assert_eq!(parse("mqtt://localhost?to=my/topic").unwrap().topics, vec!["my/topic"]);
    assert!(parse("mqtt://localhost").unwrap().topics.is_empty());
  }

  #[test]
  fn test_static_details() {
    let details = Mqtt::static_details();
    assert_eq!(details.service_name, "MQTT Notification");
    assert_eq!(details.protocols, vec!["mqtt", "mqtts"]);
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_packets() {
    assert_eq!(connect_packet(Version::V311, "", true, None, None), b"\x10\x0c\x00\x04MQTT\x04\x02\x00\x1e\x00\x00");
    assert_eq!(connect_packet(Version::V31, "id", false, Some("u"), Some("p")), b"\x10\x16\x00\x06MQIsdp\x03\xc0\x00\x1e\x00\x02id\x00\x01u\x00\x01p");
    assert_eq!(connect_packet(Version::V5, "", true, Some("u"), None), b"\x10\x10\x00\x04MQTT\x05\x82\x00\x1e\x00\x00\x00\x00\x01u");

    assert_eq!(publish_packet(Version::V311, "t", b"hi", 0, false, 0), b"\x30\x05\x00\x01thi");
    assert_eq!(publish_packet(Version::V311, "t", b"hi", 1, true, 7), b"\x33\x07\x00\x01t\x00\x07hi");
    assert_eq!(publish_packet(Version::V5, "t", b"hi", 2, false, 1), b"\x34\x08\x00\x01t\x00\x01\x00hi");

    // Long bodies use a multi-byte remaining length
    let packet = frame(PUBLISH, &[0u8; 200]);
    assert_eq!(&packet[..3], b"\x30\xc8\x01");
    assert_eq!(parse_packet(&packet).unwrap().unwrap(), (PUBLISH, vec![0u8; 200], 203));
    assert!(parse_packet(&packet[..100]).unwrap().is_none());
    assert!(parse_packet(b"\x30\xff\xff\xff\xff\x01").is_err());

    assert_eq!(random_client_id().len(), 22);
  }

  // ── A scripted broker ──────────────────────────────────────────────────────

  /// How the fake broker answers.
  #[derive(Clone, Copy, Default)]
  struct Script {
    connack: u8,
    /// Reason code added to MQTT 5 acknowledgements.
    ack_reason: Option<u8>,
    /// Never acknowledge QoS 1/2 publishes.
    silent: bool,
    /// Close the connection after this many publishes.
    close_after: Option<usize>,
  }

  /// Every packet each connection sent us, as (header, body).
  type Log = Arc<StdMutex<Vec<Vec<(u8, Vec<u8>)>>>>;

  async fn broker(script: Script) -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let shared = log.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve(Box::new(stream), script, shared.clone()));
      }
    });
    (port, log)
  }

  async fn serve(mut stream: Box<dyn Io>, script: Script, log: Log) {
    let mut inbox = Vec::new();
    let mut version = 4;
    let mut published = 0;
    let mut slot = None;
    loop {
      let (header, body) = loop {
        match parse_packet(&inbox) {
          Ok(Some((header, body, used))) => {
            inbox.drain(..used);
            break (header, body);
          }
          Ok(None) => {}
          Err(_) => return,
        }
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf).await {
          Ok(0) | Err(_) => return,
          Ok(n) => inbox.extend_from_slice(&buf[..n]),
        }
      };
      // Only log connections that start like MQTT ones
      let index = match slot {
        Some(index) => index,
        None if header == CONNECT && body.len() >= 10 => {
          let mut log = log.lock().unwrap();
          log.push(Vec::new());
          *slot.insert(log.len() - 1)
        }
        None => return,
      };
      log.lock().unwrap()[index].push((header, body.clone()));
      let reply: Vec<u8> = match header & 0xF0 {
        0x10 => {
          version = body[2 + usize::from(body[1])];
          if version == 5 { vec![CONNACK, 3, 0, script.connack, 0] } else { vec![CONNACK, 2, 0, script.connack] }
        }
        0x30 => {
          published += 1;
          let qos = (header >> 1) & 3;
          let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
          let id = &body[2 + topic_len..4 + topic_len];
          let kind = if qos == 1 { PUBACK } else { PUBREC };
          match (qos, script.silent) {
            (0, _) | (_, true) => vec![],
            _ if version == 5 => vec![kind, 3, id[0], id[1], script.ack_reason.unwrap_or(0)],
            _ => vec![kind, 2, id[0], id[1]],
          }
        }
        0x60 => vec![PUBCOMP, 2, body[0], body[1]],
        0xC0 => vec![PINGRESP, 0],
        _ => vec![],
      };
      if !reply.is_empty() && stream.write_all(&reply).await.is_err() {
        return;
      }
      if script.close_after == Some(published) {
        return;
      }
    }
  }

  trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
  impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

  fn packets(log: &Log, connection: usize) -> Vec<(u8, Vec<u8>)> {
    log.lock().unwrap().get(connection).cloned().unwrap_or_default()
  }

  /// Wait for the broker to log `count` packets on `connection`.
  async fn wait_for(log: &Log, connection: usize, count: usize) {
    for _ in 0..200 {
      if packets(log, connection).len() >= count {
        return;
      }
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  }

  fn publish_body(topic: &str, payload: &str) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(&mut body, topic);
    body.extend_from_slice(payload.as_bytes());
    body
  }

  #[tokio::test]
  async fn test_send_default_reuses_connection() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/my/topic", port)).unwrap();
    assert!(svc.send(&ctx("test=test")).await.unwrap());
    // A second notification uses the connection that is already open
    assert!(svc.send(&ctx("foo=bar")).await.unwrap());
    wait_for(&log, 0, 3).await;

    let sent = packets(&log, 0);
    assert_eq!(log.lock().unwrap().len(), 1);
    // MQTT 3.1.1, clean session, 30s keep-alive and an empty client id
    assert_eq!(sent[0], (CONNECT, b"\x00\x04MQTT\x04\x02\x00\x1e\x00\x00".to_vec()));
    assert_eq!(sent[1], (PUBLISH, publish_body("my/topic", "test=test")));
    assert_eq!(sent[2], (PUBLISH, publish_body("my/topic", "foo=bar")));
  }

  #[tokio::test]
  async fn test_send_multiple_topics() {
    let (port, log) = broker(Script::default()).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("mqtt://127.0.0.1:{}/my/topic,my/other/topic", port)));
    let started = Instant::now();
    assert!(crate::notify::with_throttling_in_tests(apprise.notify(&ctx("test=test"))).await.is_success());
    // Publishes are spaced out like upstream's throttle (0.5s)
    assert!(started.elapsed() >= Duration::from_millis(500), "{:?}", started.elapsed());
    wait_for(&log, 0, 3).await;

    let sent = packets(&log, 0);
    assert_eq!(sent[1], (PUBLISH, publish_body("my/topic", "test=test")));
    assert_eq!(sent[2], (PUBLISH, publish_body("my/other/topic", "test=test")));
  }

  #[tokio::test]
  async fn test_disconnect_on_drop() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/my/topic", port)).unwrap();
    assert!(svc.send(&ctx("test=test")).await.unwrap());
    wait_for(&log, 0, 2).await;

    // The connection stays open while the notifier is around...
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(packets(&log, 0).iter().all(|(h, _)| *h != DISCONNECT));

    // ...and is closed once it is dropped
    drop(svc);
    wait_for(&log, 0, 3).await;
    assert_eq!(packets(&log, 0).last().unwrap(), &(DISCONNECT, vec![]));
  }

  #[tokio::test]
  async fn test_no_connection_without_send() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/my/topic", port)).unwrap();
    drop(svc);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(log.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn test_send_no_topic() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}", port)).unwrap();
    assert!(!svc.send(&ctx("test=test")).await.unwrap());
    assert!(log.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn test_send_qos1_v31() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/my/topic?qos=1&version=v3.1", port)).unwrap();
    assert!(svc.send(&ctx("a")).await.unwrap());
    assert!(svc.send(&ctx("b")).await.unwrap());

    let sent = packets(&log, 0);
    // MQTT 3.1 needs a client id, so a random 22 character one is used
    let (header, body) = &sent[0];
    assert_eq!(*header, CONNECT);
    assert_eq!(&body[..12], b"\x00\x06MQIsdp\x03\x02\x00\x1e");
    assert_eq!(&body[12..14], b"\x00\x16");
    assert_eq!(body.len(), 14 + 22);

    // QoS 1 publishes carry increasing packet ids and wait for PUBACK
    let mut first = Vec::new();
    put_str(&mut first, "my/topic");
    first.extend_from_slice(b"\x00\x01a");
    assert_eq!(sent[1], (PUBLISH | 0x02, first));
    assert_eq!(&sent[2].1[10..12], b"\x00\x02");
  }

  #[tokio::test]
  async fn test_send_qos2_v5_retain_session() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://user:pass@127.0.0.1:{}/t?qos=2&version=v5.0&retain=yes&session=yes&client_id=apprise", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.unwrap());

    let sent = packets(&log, 0);
    // No clean start, user and password, empty properties, our client id
    assert_eq!(sent[0], (CONNECT, b"\x00\x04MQTT\x05\xc0\x00\x1e\x00\x00\x07apprise\x00\x04user\x00\x04pass".to_vec()));
    assert_eq!(sent[1], (PUBLISH | 0x05, b"\x00\x01t\x00\x01\x00x".to_vec()));
    assert_eq!(sent[2], (PUBREL, b"\x00\x01".to_vec()));
  }

  #[tokio::test]
  async fn test_send_user_without_password() {
    let (port, log) = broker(Script::default()).await;
    let svc = from_url(&format!("mqtt://user@127.0.0.1:{}/t", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.unwrap());
    assert_eq!(packets(&log, 0)[0].1, b"\x00\x04MQTT\x04\x82\x00\x1e\x00\x00\x00\x04user".to_vec());
  }

  #[tokio::test]
  async fn test_connect_refused() {
    let (port, _) = broker(Script { connack: 5, ..Default::default() }).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.is_err());

    // Nothing listening (a freed test port could be taken by a parallel test)
    let svc = from_url("mqtt://127.0.0.1:1/t?cto=1").unwrap();
    assert!(svc.send(&ctx("x")).await.is_err());
  }

  #[tokio::test]
  async fn test_empty_client_id_rejected_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ids = Arc::new(StdMutex::new(Vec::new()));
    let seen = ids.clone();
    tokio::spawn(async move {
      while let Ok((mut stream, _)) = listener.accept().await {
        let mut buf = vec![0u8; 256];
        let n = stream.read(&mut buf).await.unwrap();
        let (_, body, _) = parse_packet(&buf[..n]).unwrap().unwrap();
        let id_len = usize::from(u16::from_be_bytes([body[10], body[11]]));
        seen.lock().unwrap().push(id_len);
        // Refuse empty client ids like some brokers do
        let code = if id_len == 0 { 2 } else { 0 };
        stream.write_all(&[CONNACK, 2, 0, code]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
      }
    });
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.unwrap());
    assert_eq!(*ids.lock().unwrap(), vec![0, 22]);
  }

  #[tokio::test]
  async fn test_reconnect_after_broker_closes() {
    let (port, log) = broker(Script { close_after: Some(1), ..Default::default() }).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t", port)).unwrap();
    assert!(svc.send(&ctx("a")).await.unwrap());
    wait_for(&log, 0, 2).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The broker dropped us; the next notification reconnects
    assert!(svc.send(&ctx("b")).await.unwrap());
    wait_for(&log, 1, 2).await;
    assert_eq!(packets(&log, 1)[0].0, CONNECT);
    assert_eq!(packets(&log, 1)[1], (PUBLISH, publish_body("t", "b")));
  }

  #[tokio::test]
  async fn test_publish_not_acknowledged() {
    let (port, _) = broker(Script { silent: true, ..Default::default() }).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t?qos=1&rto=0.2", port)).unwrap();
    let started = Instant::now();
    assert!(svc.send(&ctx("x")).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
  }

  #[tokio::test]
  async fn test_publish_refused_v5() {
    let (port, _) = broker(Script { ack_reason: Some(0x87), ..Default::default() }).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t?qos=1&version=5", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.is_err());

    // A success reason code below 0x80 is fine
    let (port, _) = broker(Script { ack_reason: Some(0x10), ..Default::default() }).await;
    let svc = from_url(&format!("mqtt://127.0.0.1:{}/t?qos=1&version=5", port)).unwrap();
    assert!(svc.send(&ctx("x")).await.unwrap());
  }

  // ── TLS ───────────────────────────────────────────────────────────────────

  const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
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

  const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgGoKfO01j1GBO0mMW
Srs16SoFxdzu7A61wNt1m/503hihRANCAAS0CGqZFwexg2hPewlQ8WxMq8eaHFc+
sQY3n5koWF/N3eWjjLI37CFdIqea/UdYy9V4WZ5XeMviGVIg3OfgBMkY
-----END PRIVATE KEY-----";

  async fn tls_broker() -> (u16, Log) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    crate::ensure_crypto_provider();
    let cert = CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert], key).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let shared = log.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        let Ok(tls) = acceptor.accept(stream).await else { continue };
        tokio::spawn(serve(Box::new(tls), Script::default(), shared.clone()));
      }
    });
    (port, log)
  }

  #[tokio::test]
  async fn test_tls_no_verify() {
    let (port, log) = tls_broker().await;
    let svc = from_url(&format!("mqtts://user:pass@localhost:{}/my/topic?verify=no", port)).unwrap();
    assert!(svc.send(&ctx("test=test")).await.unwrap());
    wait_for(&log, 0, 2).await;
    assert_eq!(packets(&log, 0)[1], (PUBLISH, publish_body("my/topic", "test=test")));

    // DISCONNECT is still sent through TLS when dropped
    drop(svc);
    wait_for(&log, 0, 3).await;
    assert_eq!(packets(&log, 0).last().unwrap(), &(DISCONNECT, vec![]));
  }

  #[tokio::test]
  async fn test_tls_verify_rejects_self_signed() {
    let (port, log) = tls_broker().await;
    let svc = from_url(&format!("mqtts://user:pass@localhost:{}/my/topic", port)).unwrap();
    assert!(svc.send(&ctx("test=test")).await.is_err());
    assert!(log.lock().unwrap().is_empty());
  }
}

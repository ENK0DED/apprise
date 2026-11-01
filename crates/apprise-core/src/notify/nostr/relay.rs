//! A small websocket client that speaks the Nostr relay protocol (upstream
//! `apprise/plugins/nostr/relay.py`).
//!
//! Relays are websocket servers. A client sends JSON arrays such as
//! `["EVENT", {...}]` or `["REQ", "<id>", {...}]` and the relay answers with
//! `["OK", ...]`, `["EVENT", ...]`, `["EOSE", ...]` and so on. Some relays ask
//! the client to prove who it is first (NIP-42) by sending `["AUTH", ...]`.
//!
//! - <https://github.com/nostr-protocol/nips/blob/master/01.md>
//! - <https://github.com/nostr-protocol/nips/blob/master/42.md>
//! - <https://datatracker.ietf.org/doc/html/rfc6455>

use super::crypto::{Event, NostrKey};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

/// Used to confirm the server really agreed to a websocket upgrade.
pub(super) const WS_ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// Websocket frame types (opcodes)
const WS_CONTINUATION: u8 = 0x0;
const WS_TEXT: u8 = 0x1;
const WS_CLOSE: u8 = 0x8;
const WS_PING: u8 = 0x9;
const WS_PONG: u8 = 0xA;

/// The most we read for the server's handshake reply (in bytes).
const WS_MAX_HEADER: usize = 16384;

/// The most we accept for one message from a relay (in bytes); the events we
/// ask for are small, so anything bigger is refused straight away.
const WS_MAX_MESSAGE: u64 = 1_048_576;

/// The most events kept from one query when it gives no limit of its own.
const RELAY_MAX_EVENTS: u64 = 10;

/// The most addresses tried for a relay found in someone else's list.
const RELAY_MAX_ADDRESSES: usize = 4;

/// A connection problem with a relay (upstream `AppriseSocketError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayError(pub String);

impl std::fmt::Display for RelayError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

fn error(message: impl Into<String>) -> RelayError {
  RelayError(message.into())
}

/// The events a query asks for (a NIP-01 filter), written in upstream's
/// key order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Filter {
  pub kinds: Vec<i64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub authors: Option<Vec<String>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub limit: Option<u64>,
}

/// How to reach a relay.
#[derive(Debug, Clone)]
pub struct RelayOptions {
  pub verify: bool,
  /// How long to wait to connect (and finish the TLS handshake).
  pub connect_timeout: Duration,
  /// How long to wait for each reply from the relay.
  pub timeout: Duration,
  pub user_agent: String,
  /// Set for relays someone else told us about, so we never connect to a
  /// private address.
  pub public_only: bool,
}

/// The pieces of a relay URL a connection needs (Python's `urlsplit()`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RelayAddress {
  pub secure: bool,
  pub host: String,
  pub port: u16,
  /// The request path, with its query.
  pub path: String,
  /// The Host header: the default port is left out.
  pub host_header: String,
}

impl RelayAddress {
  pub fn parse(url: &str) -> Option<Self> {
    let (scheme, rest) = url.split_once("://")?;
    let secure = scheme.eq_ignore_ascii_case("wss");
    let rest = rest.split('#').next().unwrap_or("");
    let (netloc, tail) = rest.find(['/', '?']).map_or((rest, ""), |i| rest.split_at(i));
    let (path, query) = tail.split_once('?').map_or((tail, None), |(p, q)| (p, Some(q)));
    let hostport = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (host, port) = match hostport.rsplit_once(':') {
      Some((h, p)) if !p.is_empty() => (h, Some(p.parse::<u16>().ok()?)),
      Some((h, _)) => (h, None),
      None => (hostport, None),
    };
    let host = host.to_lowercase();
    let path = format!("{}{}", if path.is_empty() { "/" } else { path }, query.filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default());
    let host_header = match port {
      Some(p) => format!("{host}:{p}"),
      None => host.clone(),
    };
    Some(Self { secure, port: port.unwrap_or(if secure { 443 } else { 80 }), host, path, host_header })
  }
}

/// A byte stream to a relay: TCP, or TLS over TCP (or, in tests, a pipe).
pub(super) trait RelayStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RelayStream for T {}

/// Open the TCP (and TLS) connection to one address.
async fn connect_to(ip: IpAddr, address: &RelayAddress, verify: bool, timeout: Duration) -> Result<Box<dyn RelayStream>, RelayError> {
  let deadline = Instant::now() + timeout;
  let tcp = tokio::time::timeout(timeout, TcpStream::connect(SocketAddr::new(ip, address.port)))
    .await
    .map_err(|_| error("timed out"))?
    .map_err(|e| error(e.to_string()))?;
  let _ = socket2::SockRef::from(&tcp).set_keepalive(true);
  if !address.secure {
    return Ok(Box::new(tcp));
  }
  // The certificate is always checked against the relay's own name, never
  // a reverse lookup of the address we connected to
  let tls =
    crate::utils::socket::start_tls(tcp, &address.host, verify, deadline.saturating_duration_since(Instant::now())).await.map_err(|e| error(e.to_string()))?;
  Ok(Box::new(tls))
}

/// Look `host` up within `timeout`.
async fn resolve(host: &str, port: u16, timeout: Duration) -> Result<Vec<IpAddr>, RelayError> {
  match tokio::time::timeout(timeout, tokio::net::lookup_host((host, port))).await {
    Ok(Ok(found)) => Ok(found.map(|a| a.ip()).collect()),
    Ok(Err(e)) => Err(error(e.to_string())),
    Err(_) => Err(error("DNS resolution timed out")),
  }
}

/// Connect to the relay by name; like upstream's sockets, only IPv4 is
/// spoken, so the name's first IPv4 address is used.
async fn connect_direct(address: &RelayAddress, options: &RelayOptions) -> Result<Box<dyn RelayStream>, RelayError> {
  let deadline = Instant::now() + options.connect_timeout;
  let found = resolve(&address.host, address.port, options.connect_timeout).await?;
  let ip = found.into_iter().find(IpAddr::is_ipv4).ok_or_else(|| error(format!("{} has no IPv4 address", address.host)))?;
  connect_to(ip, address, options.verify, deadline.saturating_duration_since(Instant::now())).await
}

/// The public IPv4 addresses worth trying among `found`: each once, and only
/// the first few. An error says why none can be used.
fn public_addresses(url: &str, found: &[IpAddr]) -> Result<Vec<IpAddr>, RelayError> {
  let approved: Vec<IpAddr> = found.iter().copied().filter(|ip| crate::utils::http::is_public_ip_address(&ip.to_string())).collect();
  if approved.is_empty() {
    return Err(error(format!("Relay {} has no usable public address: no address passed the policy", url)));
  }
  let mut addresses: Vec<IpAddr> = Vec::new();
  for ip in approved.into_iter().filter(IpAddr::is_ipv4) {
    if !addresses.contains(&ip) {
      addresses.push(ip);
      if addresses.len() >= RELAY_MAX_ADDRESSES {
        break;
      }
    }
  }
  if addresses.is_empty() {
    return Err(error(format!("Relay {} has no public IPv4 address.", url)));
  }
  Ok(addresses)
}

/// Connect to one of the relay's public addresses, in turn. The name is
/// looked up once, within the connect timeout, and we then connect to those
/// exact addresses.
async fn connect_public(url: &str, address: &RelayAddress, options: &RelayOptions) -> Result<Box<dyn RelayStream>, RelayError> {
  let found =
    resolve(&address.host, address.port, options.connect_timeout).await.map_err(|e| error(format!("Relay {} has no usable public address: {}", url, e)))?;
  let addresses = public_addresses(url, &found)?;
  connect_in_turn(url, &addresses, options.connect_timeout, |ip, remaining| connect_to(ip, address, options.verify, remaining)).await
}

/// Try `addresses` one after the other until one connects. They share one
/// connection timeout, so a long list of dead addresses cannot hold us up;
/// when none works, the last error is reported.
async fn connect_in_turn<S, F, Fut>(url: &str, addresses: &[IpAddr], timeout: Duration, mut connect: F) -> Result<S, RelayError>
where
  F: FnMut(IpAddr, Duration) -> Fut,
  Fut: std::future::Future<Output = Result<S, RelayError>>,
{
  let deadline = Instant::now() + timeout;
  let mut last = error(format!("Relay {} has no public IPv4 address.", url));
  for &ip in addresses {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      last = error(format!("Timed out connecting to relay {}.", url));
      break;
    }
    match connect(ip, remaining).await {
      Ok(stream) => return Ok(stream),
      // Try the next address
      Err(e) => last = e,
    }
  }
  Err(last)
}

/// Python's `str()` of a JSON value, as upstream reports a relay's reason.
fn py_str(value: &Value) -> String {
  match value {
    Value::String(s) => s.clone(),
    Value::Null => "None".to_string(),
    Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
    other => other.to_string(),
  }
}

/// Python's `str.strip()` whitespace.
fn py_trim(s: &str) -> &str {
  s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// One websocket connection to a Nostr relay.
pub struct NostrRelay {
  url: String,
  key: NostrKey,
  stream: Box<dyn RelayStream>,
  /// How long to wait for each reply from the relay.
  timeout: Duration,
  /// Bytes received but not yet turned into frames.
  buffer: Vec<u8>,
  /// The latest NIP-42 challenge, kept until we answer it.
  challenge: Option<String>,
}

impl NostrRelay {
  /// Connect to the relay at `url` and perform the websocket handshake.
  pub async fn connect(url: &str, key: &NostrKey, options: &RelayOptions) -> Result<Self, RelayError> {
    let address = RelayAddress::parse(url).ok_or_else(|| error(format!("Invalid relay URL {}", url)))?;
    // Relays found in someone else's list may only reach public addresses
    let stream = if options.public_only { connect_public(url, &address, options).await? } else { connect_direct(&address, options).await? };
    Self::handshake(url, key, stream, &address, options).await
  }

  /// Ask the server to switch the connection on `stream` over to a
  /// websocket.
  pub(super) async fn handshake(
    url: &str,
    key: &NostrKey,
    stream: Box<dyn RelayStream>,
    address: &RelayAddress,
    options: &RelayOptions,
  ) -> Result<Self, RelayError> {
    let mut relay = Self { url: url.to_string(), key: key.clone(), stream, timeout: options.timeout, buffer: Vec::new(), challenge: None };
    let ws_key = B64.encode(rand::random::<[u8; 16]>());
    let request = format!(
      "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {agent}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
      path = address.path,
      host = address.host_header,
      agent = options.user_agent,
      key = ws_key,
    );
    relay.write(request.as_bytes()).await?;

    // Read until the end of the response headers
    let deadline = Instant::now() + relay.timeout;
    let end = loop {
      if let Some(end) = relay.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
        break end;
      }
      if relay.buffer.len() > WS_MAX_HEADER {
        return Err(error("Relay sent an oversized websocket handshake."));
      }
      relay.fill(deadline).await?;
    };
    // A single read can bring in more than the limit at once
    if end > WS_MAX_HEADER {
      return Err(error("Relay sent an oversized websocket handshake."));
    }
    // Anything after the headers already belongs to the websocket
    let head: String = relay.buffer[..end].iter().map(|&b| b as char).collect();
    relay.buffer.drain(..end + 4);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");

    // A successful upgrade always answers with status 101
    if status.split(' ').nth(1) != Some("101") {
      return Err(error(format!("Relay refused the websocket upgrade: {}", status)));
    }

    // The server must prove it understood our websocket request; a header
    // given twice counts as written last
    let accept = lines.filter_map(|line| {
      let (name, value) = line.split_once(':').unwrap_or((line, ""));
      py_trim(name).eq_ignore_ascii_case("sec-websocket-accept").then(|| py_trim(value).to_string())
    });
    let expected = B64.encode(Sha1::digest(format!("{}{}", ws_key, WS_ACCEPT_GUID).as_bytes()));
    if accept.last().as_deref() != Some(expected.as_str()) {
      return Err(error("Relay returned an invalid websocket handshake."));
    }
    tracing::trace!("Nostr relay connected: {}", relay.url);
    Ok(relay)
  }

  /// Say goodbye to the relay and close the connection. A polite close
  /// frame is nice to have but never required.
  pub async fn close(&mut self) {
    let _ = self.write_frame(WS_CLOSE, &1000u16.to_be_bytes()).await;
    let _ = tokio::time::timeout(self.timeout, self.stream.shutdown()).await;
  }

  /// Send `event` and return whether the relay accepted it, and why not.
  pub async fn publish(&mut self, event: &Event) -> Result<(bool, String), RelayError> {
    self.send(&("EVENT", event)).await?;
    let (mut accepted, mut reason) = self.wait_ok(&event.id).await?;

    // Relays that need us to log in first get one more try
    if !accepted && reason.starts_with("auth-required") && self.authenticate().await? {
      self.send(&("EVENT", event)).await?;
      (accepted, reason) = self.wait_ok(&event.id).await?;
    }
    Ok((accepted, reason))
  }

  /// The stored events that match `filter`.
  pub async fn query(&mut self, filter: &Filter) -> Result<Vec<Value>, RelayError> {
    // A random subscription id keeps replies apart
    let sub_id = hex::encode(rand::random::<[u8; 8]>());
    let mut events = Vec::new();
    let reason = self.subscribe(&sub_id, filter, &mut events).await?;

    // Relays that need us to log in first get one more try
    if reason.is_some_and(|r| r.starts_with("auth-required")) && self.authenticate().await? {
      self.subscribe(&sub_id, filter, &mut events).await?;
    }
    Ok(events)
  }

  /// Collect events into `events`; return why the relay closed the
  /// subscription, if it did.
  async fn subscribe(&mut self, sub_id: &str, filter: &Filter, events: &mut Vec<Value>) -> Result<Option<String>, RelayError> {
    self.send(&("REQ", sub_id, filter)).await?;
    let deadline = Instant::now() + self.timeout;

    // Never keep more events than we asked for
    let limit = filter.limit.unwrap_or(RELAY_MAX_EVENTS);
    loop {
      let message = self.recv(deadline).await?;
      if message.len() < 2 || message[1].as_str() != Some(sub_id) {
        // Not about our subscription
        continue;
      }
      match message[0].as_str() {
        Some("EVENT") if message.len() > 2 && message[2].is_object() => {
          events.push(message[2].clone());
          if events.len() as u64 >= limit {
            // We have all we asked for, so stop listening
            self.send(&("CLOSE", sub_id)).await?;
            return Ok(None);
          }
        }
        Some("EOSE") => {
          // Every stored event was sent, so we can stop listening
          self.send(&("CLOSE", sub_id)).await?;
          return Ok(None);
        }
        // The relay refused or ended the subscription
        Some("CLOSED") => return Ok(Some(message.get(2).map(py_str).unwrap_or_default())),
        _ => {}
      }
    }
  }

  /// Answer the relay's NIP-42 challenge; true if the relay accepted it.
  /// Each challenge is only answered once, so a refusal is final.
  async fn authenticate(&mut self) -> Result<bool, RelayError> {
    let Some(challenge) = self.challenge.take().filter(|c| !c.is_empty()) else { return Ok(false) };
    let tags = vec![vec!["relay".to_string(), self.url.clone()], vec!["challenge".to_string(), challenge]];
    let auth = self.key.sign(22242, "", tags, None);
    self.send(&("AUTH", &auth)).await?;
    let (accepted, reason) = self.wait_ok(&auth.id).await?;
    if !accepted {
      tracing::debug!("Nostr relay {} rejected authentication: {}", self.url, reason);
    }
    Ok(accepted)
  }

  /// Wait for the relay's `["OK", <id>, <accepted>, <reason>]` about
  /// `event_id`.
  async fn wait_ok(&mut self, event_id: &str) -> Result<(bool, String), RelayError> {
    let deadline = Instant::now() + self.timeout;
    loop {
      let message = self.recv(deadline).await?;
      if message.len() > 2 && message[0].as_str() == Some("OK") && message[1].as_str() == Some(event_id) {
        return Ok((message[2] == Value::Bool(true), message.get(3).map(py_str).unwrap_or_default()));
      }
    }
  }

  /// Send one protocol message (a compact JSON array) as a text frame.
  async fn send<T: Serialize>(&mut self, message: &T) -> Result<(), RelayError> {
    let data = serde_json::to_string(message).map_err(|e| error(e.to_string()))?;
    tracing::trace!("Nostr relay {} send: {}", self.url, data);
    self.write_frame(WS_TEXT, data.as_bytes()).await
  }

  /// The next protocol message from the relay; anything that is not a
  /// non-empty JSON array comes back empty.
  async fn recv(&mut self, deadline: Instant) -> Result<Vec<Value>, RelayError> {
    let mut message: Vec<u8> = Vec::new();
    loop {
      // Read frames until a complete text message arrives
      let (header, payload) = self.read_frame(deadline).await?;
      match header & 0x0F {
        // Relays check that we are still here
        WS_PING => self.write_frame(WS_PONG, &payload).await?,
        WS_CLOSE => return Err(error("The relay closed the connection.")),
        WS_TEXT | WS_CONTINUATION => {
          // Long messages can arrive split across several frames
          message.extend_from_slice(&payload);
          if message.len() as u64 > WS_MAX_MESSAGE {
            return Err(error("Relay sent an oversized message."));
          }
          if header & 0x80 != 0 {
            break;
          }
        }
        _ => {}
      }
    }
    tracing::trace!("Nostr relay {} recv: {}", self.url, String::from_utf8_lossy(&message));

    // Something that is not JSON is simply ignored
    let Ok(Value::Array(data)) = serde_json::from_slice::<Value>(&message) else { return Ok(Vec::new()) };
    match data.first().and_then(Value::as_str) {
      // Remember the challenge in case we need to log in later
      Some("AUTH") if data.len() > 1 => self.challenge = Some(py_str(&data[1])),
      Some("NOTICE") => {
        let notice = serde_json::to_string(&data[1..]).unwrap_or_default();
        tracing::debug!("Nostr relay {} notice: {}", self.url, notice);
      }
      _ => {}
    }
    Ok(data)
  }

  /// The first header byte and the payload of the next frame.
  async fn read_frame(&mut self, deadline: Instant) -> Result<(u8, Vec<u8>), RelayError> {
    loop {
      // The second byte holds the payload length, or says that a longer 2
      // or 8 byte length follows it
      let buf = &self.buffer;
      let short = if buf.len() >= 2 { buf[1] & 0x7F } else { 0 };
      let offset = match short {
        126 => 4,
        127 => 10,
        _ => 2,
      };
      if buf.len() >= offset {
        let length = if offset > 2 { buf[2..offset].iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)) } else { u64::from(short) };
        // Refuse an oversized frame before waiting for all of it
        if length > WS_MAX_MESSAGE {
          return Err(error("Relay sent an oversized frame."));
        }
        let end = offset + length as usize;
        if buf.len() >= end {
          let header = buf[0];
          let payload = buf[offset..end].to_vec();
          self.buffer.drain(..end);
          return Ok((header, payload));
        }
      }
      self.fill(deadline).await?;
    }
  }

  /// Send one masked frame, marked as the final frame of its message.
  async fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), RelayError> {
    let mut frame = vec![0x80 | opcode];
    // Client frames are always masked (0x80) and give their length
    match payload.len() {
      n if n < 126 => frame.push(0x80 | n as u8),
      n if n < 65536 => {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
      }
      n => {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(n as u64).to_be_bytes());
      }
    }
    // XOR the payload with a random 4 byte mask
    let mask: [u8; 4] = rand::random();
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    self.write(&frame).await
  }

  async fn write(&mut self, data: &[u8]) -> Result<(), RelayError> {
    match tokio::time::timeout(self.timeout, async {
      self.stream.write_all(data).await?;
      self.stream.flush().await
    })
    .await
    {
      Ok(Ok(())) => Ok(()),
      Ok(Err(e)) => Err(error(e.to_string())),
      Err(_) => Err(error("Timed out writing to the relay.")),
    }
  }

  /// Read more data from the relay into our buffer, giving up once we have
  /// waited long enough.
  async fn fill(&mut self, deadline: Instant) -> Result<(), RelayError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return Err(error("Timed out waiting for the relay."));
    }
    self.buffer.reserve(65536);
    match tokio::time::timeout(remaining, self.stream.read_buf(&mut self.buffer)).await {
      Ok(Ok(0)) => Err(error("Connection lost during read")),
      Ok(Ok(_)) => Ok(()),
      Ok(Err(e)) => Err(error(e.to_string())),
      Err(_) => Err(error("Timed out waiting for the relay.")),
    }
  }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
pub(super) mod tests;

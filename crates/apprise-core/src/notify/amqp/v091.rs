//! AMQP 0-9-1 publishing.
//!
//! Upstream publishes with pika; this speaks the small part of the protocol a
//! publisher needs, with the same settings: one confirming channel, so every
//! message waits for the broker's ack, and mandatory messages nobody can
//! receive count as failures.
//! See https://www.rabbitmq.com/amqp-0-9-1-reference and
//! https://www.rabbitmq.com/docs/confirms

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use super::{Config, Io, Message, amqp_error, connect_stream};
use crate::error::NotifyError;

const PROTOCOL_HEADER: &[u8] = b"AMQP\x00\x00\x09\x01";

const FRAME_METHOD: u8 = 1;
const FRAME_HEADER: u8 = 2;
const FRAME_BODY: u8 = 3;
const FRAME_END: u8 = 0xCE;

/// Bytes a frame adds around its payload.
const FRAME_OVERHEAD: u32 = 8;

/// What we offer during tuning (pika's defaults).
const CHANNEL_MAX: u16 = 65535;
const FRAME_MAX: u32 = 131_072;

// (class, method) ids
const CONNECTION_START: (u16, u16) = (10, 10);
const CONNECTION_START_OK: (u16, u16) = (10, 11);
const CONNECTION_TUNE: (u16, u16) = (10, 30);
const CONNECTION_TUNE_OK: (u16, u16) = (10, 31);
const CONNECTION_OPEN: (u16, u16) = (10, 40);
const CONNECTION_OPEN_OK: (u16, u16) = (10, 41);
const CONNECTION_CLOSE: (u16, u16) = (10, 50);
const CONNECTION_CLOSE_OK: (u16, u16) = (10, 51);
const CHANNEL_OPEN: (u16, u16) = (20, 10);
const CHANNEL_OPEN_OK: (u16, u16) = (20, 11);
const CHANNEL_CLOSE: (u16, u16) = (20, 40);
const CHANNEL_CLOSE_OK: (u16, u16) = (20, 41);
const CONFIRM_SELECT: (u16, u16) = (85, 10);
const CONFIRM_SELECT_OK: (u16, u16) = (85, 11);
const BASIC_PUBLISH: (u16, u16) = (60, 40);
const BASIC_RETURN: (u16, u16) = (60, 50);
const BASIC_ACK: (u16, u16) = (60, 80);
const BASIC_NACK: (u16, u16) = (60, 120);
const BASIC_CLASS: u16 = 60;

/// The channel our messages are published on.
const CHANNEL: u16 = 1;

pub(super) struct Publisher {
  config: Config,
  conn: Option<Connection>,
}

struct Connection {
  io: Box<dyn Io>,
  frame_max: u32,
  /// Whether our channel is open, and the delivery tag of our next message.
  channel_open: bool,
  next_tag: u64,
}

/// One frame read from the broker.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Frame {
  pub kind: u8,
  pub channel: u16,
  pub payload: Vec<u8>,
}

impl Frame {
  /// The (class, method) ids and arguments of a method frame.
  pub fn method(&self) -> Option<((u16, u16), &[u8])> {
    if self.kind != FRAME_METHOD || self.payload.len() < 4 {
      return None;
    }
    let p = &self.payload;
    Some(((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])), &p[4..]))
  }
}

impl Publisher {
  pub fn new(config: Config) -> Self {
    Self { config, conn: None }
  }

  async fn handshake(cfg: &Config) -> Result<Connection, NotifyError> {
    let mut conn = Connection { io: connect_stream(cfg).await?, frame_max: FRAME_MAX, channel_open: false, next_tag: 1 };
    conn.write(PROTOCOL_HEADER).await?;

    let start = conn.expect(CONNECTION_START, cfg.read_timeout).await?;
    let mechanisms = parse_start_mechanisms(&start)?;
    if !mechanisms.split(' ').any(|m| m == "PLAIN") {
      return Err(amqp_error(format!("the broker does not offer PLAIN authentication ({})", mechanisms)));
    }

    // pika logs in as guest/guest when no user is given
    let (user, password) = match &cfg.user {
      Some(user) => (user.as_str(), cfg.password.as_deref().unwrap_or("")),
      None => ("guest", "guest"),
    };
    let mut args = Vec::new();
    put_table(&mut args, &client_properties());
    put_shortstr(&mut args, "PLAIN");
    put_longstr(&mut args, format!("\0{}\0{}", user, password).as_bytes());
    put_shortstr(&mut args, "en_US");
    conn.send_method(0, CONNECTION_START_OK, &args).await?;

    let tune = conn.expect(CONNECTION_TUNE, cfg.read_timeout).await?;
    if tune.len() < 8 {
      return Err(amqp_error("malformed Connection.Tune"));
    }
    let channel_max = negotiate(CHANNEL_MAX.into(), u16::from_be_bytes([tune[0], tune[1]]).into()) as u16;
    let frame_max = negotiate(FRAME_MAX, u32::from_be_bytes([tune[2], tune[3], tune[4], tune[5]]));
    // Ask for heartbeats close to our read timeout so a silent broker is
    // noticed quickly (the broker's own proposal is often 60 seconds)
    let heartbeat = cfg.read_timeout.as_secs_f64().ceil().max(1.0).min(u16::MAX.into()) as u16;
    let mut args = Vec::new();
    args.extend_from_slice(&channel_max.to_be_bytes());
    args.extend_from_slice(&frame_max.to_be_bytes());
    args.extend_from_slice(&heartbeat.to_be_bytes());
    conn.send_method(0, CONNECTION_TUNE_OK, &args).await?;
    conn.frame_max = if frame_max == 0 { FRAME_MAX } else { frame_max };

    let mut args = Vec::new();
    put_shortstr(&mut args, cfg.vhost.as_deref().unwrap_or("/"));
    // Reserved capabilities and insist fields
    put_shortstr(&mut args, "");
    args.push(0);
    conn.send_method(0, CONNECTION_OPEN, &args).await?;
    conn.expect(CONNECTION_OPEN_OK, cfg.read_timeout).await?;
    Ok(conn)
  }

  pub async fn connect(&mut self) -> Result<(), NotifyError> {
    // pika bounds the whole handshake by connect plus read timeout
    let limit = self.config.connect_timeout + self.config.read_timeout;
    let conn = timeout(limit, Self::handshake(&self.config)).await.map_err(|_| amqp_error("timed out logging into the broker"))??;
    self.conn = Some(conn);
    Ok(())
  }

  pub async fn publish(&mut self, target: &str, message: &Message) -> Result<(), NotifyError> {
    let cfg = &self.config;
    let conn = self.conn.as_mut().ok_or_else(|| amqp_error("not connected"))?;
    let result = conn.publish(cfg, target, message).await;
    if let Err(Failure::Connection(_)) = &result {
      // The broker closed the whole connection; nothing else can be sent
      self.conn = None;
    }
    result.map_err(Failure::into_error)
  }

  pub async fn close(&mut self) {
    let Some(mut conn) = self.conn.take() else { return };
    let mut args = Vec::new();
    args.extend_from_slice(&200u16.to_be_bytes());
    put_shortstr(&mut args, "Normal shutdown");
    args.extend_from_slice(&[0, 0, 0, 0]);
    if conn.send_method(0, CONNECTION_CLOSE, &args).await.is_ok() {
      // Give the broker our read timeout to say goodbye
      let _ = conn.expect(CONNECTION_CLOSE_OK, self.config.read_timeout).await;
    }
    let _ = conn.io.shutdown().await;
  }
}

/// Why a publish failed: the channel or the whole connection is gone.
enum Failure {
  Channel(NotifyError),
  Connection(NotifyError),
}

impl Failure {
  fn into_error(self) -> NotifyError {
    match self {
      Self::Channel(e) | Self::Connection(e) => e,
    }
  }
}

impl From<NotifyError> for Failure {
  fn from(e: NotifyError) -> Self {
    Self::Connection(e)
  }
}

impl Connection {
  async fn write(&mut self, data: &[u8]) -> Result<(), NotifyError> {
    self.io.write_all(data).await?;
    self.io.flush().await?;
    Ok(())
  }

  async fn send_method(&mut self, channel: u16, method: (u16, u16), args: &[u8]) -> Result<(), NotifyError> {
    self.write(&method_frame(channel, method, args)).await
  }

  async fn read_frame(&mut self, wait: Duration) -> Result<Frame, NotifyError> {
    let read = async {
      let mut header = [0u8; 7];
      self.io.read_exact(&mut header).await?;
      if &header[..4] == b"AMQP" {
        return Err(amqp_error("the broker does not speak AMQP 0-9-1"));
      }
      let size = u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
      // Frames may not exceed the negotiated frame_max (at most FRAME_MAX);
      // a larger size would make us allocate whatever the broker claims.
      if size > self.frame_max as usize {
        return Err(amqp_error("the broker sent a frame larger than frame_max"));
      }
      let mut payload = vec![0u8; size + 1];
      self.io.read_exact(&mut payload).await?;
      if payload.pop() != Some(FRAME_END) {
        return Err(amqp_error("malformed frame from the broker"));
      }
      Ok(Frame { kind: header[0], channel: u16::from_be_bytes([header[1], header[2]]), payload })
    };
    timeout(wait, read).await.map_err(|_| amqp_error("timed out waiting for the broker"))?
  }

  /// Wait for `method` on channel 0, failing if the broker closes on us.
  async fn expect(&mut self, method: (u16, u16), wait: Duration) -> Result<Vec<u8>, NotifyError> {
    loop {
      let frame = self.read_frame(wait).await?;
      match frame.method() {
        Some((m, args)) if m == method => return Ok(args.to_vec()),
        Some((CONNECTION_CLOSE, args)) => {
          let reason = close_reason(args);
          let _ = self.send_method(0, CONNECTION_CLOSE_OK, &[]).await;
          return Err(amqp_error(format!("the broker closed the connection: {}", reason)));
        }
        _ => {}
      }
    }
  }

  /// Open our channel and put it in confirm mode.
  async fn open_channel(&mut self, wait: Duration) -> Result<(), Failure> {
    self.send_method(CHANNEL, CHANNEL_OPEN, &[0]).await?;
    self.await_channel(CHANNEL_OPEN_OK, wait).await?;
    self.send_method(CHANNEL, CONFIRM_SELECT, &[0]).await?;
    self.await_channel(CONFIRM_SELECT_OK, wait).await?;
    self.channel_open = true;
    self.next_tag = 1;
    Ok(())
  }

  /// Wait for `method` on our channel.
  async fn await_channel(&mut self, method: (u16, u16), wait: Duration) -> Result<Vec<u8>, Failure> {
    loop {
      let frame = self.read_frame(wait).await?;
      if let Some(args) = self.handle_close(&frame).await? {
        return Err(args);
      }
      if frame.channel == CHANNEL
        && let Some((m, args)) = frame.method()
        && m == method
      {
        return Ok(args.to_vec());
      }
    }
  }

  /// Answer a Channel.Close or Connection.Close from the broker.
  async fn handle_close(&mut self, frame: &Frame) -> Result<Option<Failure>, NotifyError> {
    match frame.method() {
      Some((CHANNEL_CLOSE, args)) if frame.channel == CHANNEL => {
        // The broker closes a channel after some errors (such as a missing
        // exchange); a new one is opened for the next message
        let reason = close_reason(args);
        self.channel_open = false;
        self.send_method(CHANNEL, CHANNEL_CLOSE_OK, &[]).await?;
        Ok(Some(Failure::Channel(amqp_error(format!("the broker closed the channel: {}", reason)))))
      }
      Some((CONNECTION_CLOSE, args)) if frame.channel == 0 => {
        let reason = close_reason(args);
        let _ = self.send_method(0, CONNECTION_CLOSE_OK, &[]).await;
        Ok(Some(Failure::Connection(amqp_error(format!("the broker closed the connection: {}", reason)))))
      }
      _ => Ok(None),
    }
  }

  async fn publish(&mut self, cfg: &Config, target: &str, message: &Message) -> Result<(), Failure> {
    let wait = cfg.read_timeout;
    if !self.channel_open {
      self.open_channel(wait).await?;
    }

    let mut args = Vec::new();
    args.extend_from_slice(&0u16.to_be_bytes());
    put_shortstr(&mut args, &cfg.exchange);
    put_shortstr(&mut args, target);
    args.push(u8::from(cfg.mandatory));
    let mut frames = method_frame(CHANNEL, BASIC_PUBLISH, &args);
    frames.extend(frame(FRAME_HEADER, CHANNEL, &content_header(message)));
    let chunk = (self.frame_max.saturating_sub(FRAME_OVERHEAD) as usize).max(1);
    for part in message.body.chunks(chunk) {
      frames.extend(frame(FRAME_BODY, CHANNEL, part));
    }
    self.write(&frames).await?;
    let tag = self.next_tag;
    self.next_tag += 1;

    // Wait for the broker to confirm it; a returned message precedes its ack
    let mut returned = None;
    let confirm = async {
      loop {
        let frame = self.read_frame(wait).await?;
        if let Some(failure) = self.handle_close(&frame).await? {
          return Err(failure);
        }
        if frame.channel != CHANNEL {
          continue;
        }
        match frame.method() {
          Some((BASIC_RETURN, args)) => returned = Some(close_reason(args)),
          Some((BASIC_ACK, args)) if acknowledges(args, tag) => {
            return match returned.take() {
              Some(reason) => Err(Failure::Channel(amqp_error(format!("the message could not be routed: {}", reason)))),
              None => Ok(()),
            };
          }
          Some((BASIC_NACK, args)) if acknowledges(args, tag) => return Err(Failure::Channel(amqp_error("the broker refused the message"))),
          _ => {}
        }
      }
    };
    timeout(wait, confirm).await.map_err(|_| Failure::Connection(amqp_error("timed out waiting for the broker to confirm the message")))?
  }
}

/// Whether an ack or nack (delivery tag + multiple flag) covers `tag`.
fn acknowledges(args: &[u8], tag: u64) -> bool {
  if args.len() < 9 {
    return false;
  }
  let acked = u64::from_be_bytes(args[..8].try_into().unwrap_or_default());
  acked == tag || (args[8] & 1 == 1 && acked >= tag)
}

/// Tuning picks the smaller value unless one side has no limit (0).
fn negotiate(ours: u32, theirs: u32) -> u32 {
  if ours == 0 || theirs == 0 { ours.max(theirs) } else { ours.min(theirs) }
}

/// "<code> <text>" from a close or return method.
fn close_reason(args: &[u8]) -> String {
  if args.len() < 3 {
    return "no reason given".into();
  }
  let code = u16::from_be_bytes([args[0], args[1]]);
  let len = usize::from(args[2]);
  let text = args.get(3..3 + len).map(String::from_utf8_lossy).unwrap_or_default();
  format!("{} {}", code, text)
}

/// The mechanisms offered by Connection.Start.
fn parse_start_mechanisms(args: &[u8]) -> Result<String, NotifyError> {
  let malformed = || amqp_error("malformed Connection.Start");
  // Version major/minor, then the server properties table
  let table_len = args.get(2..6).ok_or_else(malformed)?;
  let table_len = u32::from_be_bytes(table_len.try_into().map_err(|_| malformed())?) as usize;
  let pos = 6 + table_len;
  let len = args.get(pos..pos + 4).ok_or_else(malformed)?;
  let len = u32::from_be_bytes(len.try_into().map_err(|_| malformed())?) as usize;
  let mechanisms = args.get(pos + 4..pos + 4 + len).ok_or_else(malformed)?;
  Ok(String::from_utf8_lossy(mechanisms).into_owned())
}

// ── Encoding ─────────────────────────────────────────────────────────────────

/// A value in an AMQP field table.
pub(super) enum Field {
  Str(String),
  Bool(bool),
  Table(Vec<(&'static str, Field)>),
}

fn client_properties() -> Vec<(&'static str, Field)> {
  vec![
    ("product", Field::Str("Apprise".into())),
    ("version", Field::Str(env!("CARGO_PKG_VERSION").into())),
    ("platform", Field::Str("Rust".into())),
    ("information", Field::Str("https://github.com/caronc/apprise".into())),
    (
      "capabilities",
      Field::Table(vec![
        ("authentication_failure_close", Field::Bool(true)),
        ("basic.nack", Field::Bool(true)),
        ("connection.blocked", Field::Bool(true)),
        ("consumer_cancel_notify", Field::Bool(true)),
        ("publisher_confirms", Field::Bool(true)),
      ]),
    ),
  ]
}

/// The content header carrying the message properties.
pub(super) fn content_header(message: &Message) -> Vec<u8> {
  let mut flags = 0u16;
  let mut props = Vec::new();

  // Properties go in this order, flagged from bit 15 down
  flags |= 1 << 15;
  put_shortstr(&mut props, message.content_type);
  if !message.headers.is_empty() {
    flags |= 1 << 13;
    let headers = message.headers.iter().map(|(k, v)| (k.as_str(), Field::Str(v.clone()))).collect::<Vec<_>>();
    put_table(&mut props, &headers);
  }
  // 2 asks the broker to write the message to disk
  flags |= 1 << 12;
  props.push(if message.persistent { 2 } else { 1 });
  if let Some(priority) = message.priority {
    flags |= 1 << 11;
    props.push(priority);
  }
  if let Some(ttl) = message.ttl {
    // AMQP 0-9-1 carries the expiry as text (milliseconds)
    flags |= 1 << 8;
    put_shortstr(&mut props, &ttl.to_string());
  }
  flags |= 1 << 6;
  props.extend_from_slice(&message.timestamp.to_be_bytes());
  flags |= 1 << 3;
  put_shortstr(&mut props, &message.app_id);

  let mut out = Vec::with_capacity(props.len() + 14);
  out.extend_from_slice(&BASIC_CLASS.to_be_bytes());
  // Weight, then the body size
  out.extend_from_slice(&0u16.to_be_bytes());
  out.extend_from_slice(&(message.body.len() as u64).to_be_bytes());
  out.extend_from_slice(&flags.to_be_bytes());
  out.extend_from_slice(&props);
  out
}

pub(super) fn frame(kind: u8, channel: u16, payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(payload.len() + FRAME_OVERHEAD as usize);
  out.push(kind);
  out.extend_from_slice(&channel.to_be_bytes());
  out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
  out.extend_from_slice(payload);
  out.push(FRAME_END);
  out
}

pub(super) fn method_frame(channel: u16, (class, method): (u16, u16), args: &[u8]) -> Vec<u8> {
  let mut payload = Vec::with_capacity(args.len() + 4);
  payload.extend_from_slice(&class.to_be_bytes());
  payload.extend_from_slice(&method.to_be_bytes());
  payload.extend_from_slice(args);
  frame(FRAME_METHOD, channel, &payload)
}

pub(super) fn put_shortstr(out: &mut Vec<u8>, value: &str) {
  // Names were checked against the 255 byte limit when the URL was loaded;
  // anything longer (such as a header value) is cut rather than corrupt the frame
  let bytes = &value.as_bytes()[..value.len().min(255)];
  out.push(bytes.len() as u8);
  out.extend_from_slice(bytes);
}

pub(super) fn put_longstr(out: &mut Vec<u8>, value: &[u8]) {
  out.extend_from_slice(&(value.len() as u32).to_be_bytes());
  out.extend_from_slice(value);
}

pub(super) fn put_table(out: &mut Vec<u8>, fields: &[(&str, Field)]) {
  let mut body = Vec::new();
  for (name, value) in fields {
    put_shortstr(&mut body, name);
    match value {
      Field::Str(s) => {
        body.push(b'S');
        put_longstr(&mut body, s.as_bytes());
      }
      Field::Bool(b) => {
        body.push(b't');
        body.push(u8::from(*b));
      }
      Field::Table(t) => {
        body.push(b'F');
        put_table(&mut body, t);
      }
    }
  }
  put_longstr(out, &body);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use std::collections::BTreeMap;
  use std::sync::{Arc, Mutex};
  use tokio::net::TcpListener;

  fn message() -> Message {
    Message {
      body: b"hello".to_vec(),
      content_type: "text/plain; charset=utf-8",
      app_id: "Apprise".into(),
      timestamp: 1234,
      persistent: true,
      priority: Some(3),
      ttl: Some(60000),
      headers: BTreeMap::from([("a".to_string(), "b".to_string())]),
    }
  }

  #[test]
  fn test_content_header() {
    let header = content_header(&message());
    let mut expected = vec![0, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5];
    // content-type, headers, delivery-mode, priority, expiration, timestamp, app-id
    expected.extend_from_slice(&0b1011_1001_0100_1000u16.to_be_bytes());
    expected.push(25);
    expected.extend_from_slice(b"text/plain; charset=utf-8");
    expected.extend_from_slice(b"\x00\x00\x00\x08\x01aS\x00\x00\x00\x01b");
    expected.extend_from_slice(&[2, 3]);
    expected.extend_from_slice(b"\x0560000");
    expected.extend_from_slice(&1234u64.to_be_bytes());
    expected.extend_from_slice(b"\x07Apprise");
    assert_eq!(header, expected);

    // Without headers, priority or expiry only the always-set ones remain
    let plain = Message { persistent: false, priority: None, ttl: None, headers: BTreeMap::new(), ..message() };
    let header = content_header(&plain);
    assert_eq!(&header[12..14], &0b1001_0000_0100_1000u16.to_be_bytes());
    assert_eq!(header[14 + 26], 1);
  }

  #[test]
  fn test_helpers() {
    assert_eq!(frame(FRAME_BODY, 1, b"ab"), b"\x03\x00\x01\x00\x00\x00\x02ab\xce");
    assert_eq!(method_frame(0, CONNECTION_CLOSE_OK, &[]), b"\x01\x00\x00\x00\x00\x00\x04\x00\x0a\x00\x33\xce");
    assert_eq!(negotiate(131_072, 4096), 4096);
    assert_eq!(negotiate(131_072, 0), 131_072);
    assert_eq!(negotiate(0, 7), 7);
    let mut ack = 5u64.to_be_bytes().to_vec();
    ack.push(0);
    assert!(acknowledges(&ack, 5));
    assert!(!acknowledges(&ack, 4));
    ack[8] = 1;
    assert!(acknowledges(&ack, 4));
    assert!(!acknowledges(&ack, 6));
    assert_eq!(close_reason(b"\x01\x94\x09NOT_FOUND\x00\x28\x00\x0a"), "404 NOT_FOUND");
  }

  // ── A scripted broker ──────────────────────────────────────────────────────

  #[derive(Clone, Default)]
  struct Script {
    frame_max: u32,
    /// Refuse the login with Connection.Close 403.
    refuse_login: bool,
    /// Routing keys answered with Basic.Return (then acked).
    unroutable: Vec<&'static str>,
    /// Routing keys answered with Basic.Nack.
    nacked: Vec<&'static str>,
    /// Routing keys whose exchange is missing (Channel.Close 404).
    missing: Vec<&'static str>,
    /// Accept the connection but never answer.
    silent: bool,
  }

  /// Every frame the client sent, per connection.
  type Log = Arc<Mutex<Vec<Frame>>>;

  async fn broker(script: Script) -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let shared = log.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve(Box::new(stream), script.clone(), shared.clone()));
      }
    });
    (port, log)
  }

  async fn read_client_frame(io: &mut Box<dyn Io>) -> Option<Frame> {
    let mut header = [0u8; 7];
    io.read_exact(&mut header).await.ok()?;
    let size = u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
    let mut payload = vec![0u8; size + 1];
    io.read_exact(&mut payload).await.ok()?;
    assert_eq!(payload.pop(), Some(FRAME_END));
    Some(Frame { kind: header[0], channel: u16::from_be_bytes([header[1], header[2]]), payload })
  }

  fn close_args(code: u16, text: &str) -> Vec<u8> {
    let mut args = code.to_be_bytes().to_vec();
    put_shortstr(&mut args, text);
    args.extend_from_slice(&[0, 0, 0, 0]);
    args
  }

  async fn serve(mut io: Box<dyn Io>, script: Script, log: Log) {
    let mut header = [0u8; 8];
    if io.read_exact(&mut header).await.is_err() {
      return;
    }
    if header != PROTOCOL_HEADER {
      return;
    }
    if script.silent {
      tokio::time::sleep(Duration::from_secs(30)).await;
      return;
    }
    let mut start = vec![0, 9];
    put_table(&mut start, &[("product", Field::Str("RabbitMQ".into()))]);
    put_longstr(&mut start, b"AMQPLAIN PLAIN");
    put_longstr(&mut start, b"en_US");
    let _ = io.write_all(&method_frame(0, CONNECTION_START, &start)).await;

    let mut tag = 0u64;
    let mut routing_key = String::new();
    while let Some(frame) = read_client_frame(&mut io).await {
      log.lock().unwrap().push(frame.clone());
      let reply = match frame.method() {
        Some((CONNECTION_START_OK, _)) if script.refuse_login => method_frame(0, CONNECTION_CLOSE, &close_args(403, "ACCESS_REFUSED")),
        Some((CONNECTION_START_OK, _)) => {
          let mut tune = 2047u16.to_be_bytes().to_vec();
          tune.extend_from_slice(&script.frame_max.to_be_bytes());
          tune.extend_from_slice(&60u16.to_be_bytes());
          method_frame(0, CONNECTION_TUNE, &tune)
        }
        Some((CONNECTION_OPEN, _)) => method_frame(0, CONNECTION_OPEN_OK, &[0]),
        Some((CHANNEL_OPEN, _)) => method_frame(CHANNEL, CHANNEL_OPEN_OK, &[0, 0, 0, 0]),
        Some((CONFIRM_SELECT, _)) => {
          tag = 0;
          method_frame(CHANNEL, CONFIRM_SELECT_OK, &[])
        }
        Some((BASIC_PUBLISH, args)) => {
          let exchange_len = usize::from(args[2]);
          let key_len = usize::from(args[3 + exchange_len]);
          routing_key = String::from_utf8_lossy(&args[4 + exchange_len..4 + exchange_len + key_len]).into_owned();
          vec![]
        }
        Some((CONNECTION_CLOSE, _)) => method_frame(0, CONNECTION_CLOSE_OK, &[]),
        _ if frame.kind == FRAME_HEADER || frame.kind == FRAME_BODY => {
          // Answer once the whole body arrived (an empty one has no frames)
          let frames = log.lock().unwrap().clone();
          let header = frames.iter().rev().find(|f| f.kind == FRAME_HEADER).unwrap();
          let declared = u64::from_be_bytes(header.payload[4..12].try_into().unwrap());
          let received: usize = frames.iter().rev().take_while(|f| f.kind == FRAME_BODY).map(|f| f.payload.len()).sum();
          if (received as u64) < declared {
            continue;
          }
          tag += 1;
          reply_to_publish(&script, &routing_key, tag)
        }
        _ => vec![],
      };
      if !reply.is_empty() && io.write_all(&reply).await.is_err() {
        return;
      }
    }
  }

  fn reply_to_publish(script: &Script, routing_key: &str, tag: u64) -> Vec<u8> {
    let mut ack = tag.to_be_bytes().to_vec();
    ack.push(0);
    if script.missing.contains(&routing_key) {
      method_frame(CHANNEL, CHANNEL_CLOSE, &close_args(404, "NOT_FOUND"))
    } else if script.nacked.contains(&routing_key) {
      ack.push(0);
      method_frame(CHANNEL, BASIC_NACK, &ack)
    } else if script.unroutable.contains(&routing_key) {
      let mut ret = 312u16.to_be_bytes().to_vec();
      put_shortstr(&mut ret, "NO_ROUTE");
      put_shortstr(&mut ret, "");
      put_shortstr(&mut ret, routing_key);
      let mut out = method_frame(CHANNEL, BASIC_RETURN, &ret);
      out.extend(frame_for_return());
      out.extend(method_frame(CHANNEL, BASIC_ACK, &ack));
      out
    } else {
      method_frame(CHANNEL, BASIC_ACK, &ack)
    }
  }

  /// The returned message's content header and body.
  fn frame_for_return() -> Vec<u8> {
    let msg = Message { body: b"x".to_vec(), ..message() };
    let mut out = frame(FRAME_HEADER, CHANNEL, &content_header(&msg));
    out.extend(frame(FRAME_BODY, CHANNEL, b"x"));
    out
  }

  fn methods(log: &Log) -> Vec<(u16, (u16, u16))> {
    log.lock().unwrap().iter().filter_map(|f| f.method().map(|(m, _)| (f.channel, m))).collect()
  }

  fn method_args(log: &Log, method: (u16, u16)) -> Vec<Vec<u8>> {
    log.lock().unwrap().iter().filter_map(|f| f.method().filter(|(m, _)| *m == method).map(|(_, a)| a.to_vec())).collect()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send() {
    let (port, log) = broker(Script { frame_max: 131_072, ..Default::default() }).await;
    let svc = from_url(&format!("amqp://user:pass@127.0.0.1:{}/q1/q2?exchange=ex&vhost=home&priority=4&ttl=100&+source=nas&rto=3", port)).unwrap();
    assert!(svc.send(&ctx("Title\r\nBody")).await.unwrap());

    assert_eq!(
      methods(&log),
      vec![
        (0, CONNECTION_START_OK),
        (0, CONNECTION_TUNE_OK),
        (0, CONNECTION_OPEN),
        (CHANNEL, CHANNEL_OPEN),
        (CHANNEL, CONFIRM_SELECT),
        (CHANNEL, BASIC_PUBLISH),
        (CHANNEL, BASIC_PUBLISH),
        (0, CONNECTION_CLOSE),
      ]
    );

    // PLAIN login with our user
    let start_ok = &method_args(&log, CONNECTION_START_OK)[0];
    let table_len = u32::from_be_bytes(start_ok[..4].try_into().unwrap()) as usize;
    let rest = &start_ok[4 + table_len..];
    assert_eq!(rest, b"\x05PLAIN\x00\x00\x00\x0a\x00user\x00pass\x05en_US");
    assert!(String::from_utf8_lossy(&start_ok[4..4 + table_len]).contains("publisher_confirms"));

    // Our limits, and a heartbeat matching the read timeout
    assert_eq!(method_args(&log, CONNECTION_TUNE_OK)[0], b"\x07\xff\x00\x02\x00\x00\x00\x03");
    assert_eq!(method_args(&log, CONNECTION_OPEN)[0], b"\x04home\x00\x00");

    // Both routing keys, through our exchange, mandatory
    let publishes = method_args(&log, BASIC_PUBLISH);
    assert_eq!(publishes[0], b"\x00\x00\x02ex\x02q1\x01");
    assert_eq!(publishes[1], b"\x00\x00\x02ex\x02q2\x01");

    // The same properties and body for each
    let frames = log.lock().unwrap().clone();
    let headers: Vec<_> = frames.iter().filter(|f| f.kind == FRAME_HEADER).collect();
    let bodies: Vec<_> = frames.iter().filter(|f| f.kind == FRAME_BODY).collect();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].payload, headers[1].payload);
    let header = &headers[0].payload;
    assert_eq!(u64::from_be_bytes(header[4..12].try_into().unwrap()), 11);
    let props = String::from_utf8_lossy(header);
    for expected in ["text/plain; charset=utf-8", "source", "nas", "100", "Apprise"] {
      assert!(props.contains(expected), "{} missing from {:?}", expected, props);
    }
    assert_eq!(bodies[0].payload, b"Title\r\nBody");
    assert_eq!(method_args(&log, CONNECTION_CLOSE)[0], b"\x00\xc8\x0fNormal shutdown\x00\x00\x00\x00");
  }

  #[tokio::test]
  async fn test_send_defaults() {
    let (port, log) = broker(Script { frame_max: 4096, ..Default::default() }).await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/q?rto=0.2", port)).unwrap();
    let body = "x".repeat(10_000);
    assert!(svc.send(&ctx(&body)).await.unwrap());

    // pika's guest login, the default virtual host, a one second heartbeat
    let start_ok = &method_args(&log, CONNECTION_START_OK)[0];
    assert!(start_ok.ends_with(b"\x05PLAIN\x00\x00\x00\x0c\x00guest\x00guest\x05en_US"));
    assert_eq!(method_args(&log, CONNECTION_TUNE_OK)[0], b"\x07\xff\x00\x00\x10\x00\x00\x01");
    assert_eq!(method_args(&log, CONNECTION_OPEN)[0], b"\x01/\x00\x00");
    assert_eq!(method_args(&log, BASIC_PUBLISH)[0], b"\x00\x00\x00\x01q\x01");

    // The body is split to fit the broker's frame size
    let bodies: Vec<usize> = log.lock().unwrap().iter().filter(|f| f.kind == FRAME_BODY).map(|f| f.payload.len()).collect();
    assert_eq!(bodies, vec![4088, 4088, 1824]);
  }

  #[tokio::test]
  async fn test_send_exchange_only() {
    let (port, log) = broker(Script { frame_max: 131_072, ..Default::default() }).await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/?exchange=amq.fanout&mandatory=no", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.unwrap());
    assert_eq!(method_args(&log, BASIC_PUBLISH), vec![b"\x00\x00\x0aamq.fanout\x00\x00".to_vec()]);
  }

  #[tokio::test]
  async fn test_refused_messages() {
    let script = Script { frame_max: 131_072, unroutable: vec!["a"], nacked: vec!["b"], missing: vec!["c"], ..Default::default() };
    let (port, log) = broker(script).await;
    let cfg = from_url_config(&format!("amqp://127.0.0.1:{}/x", port));
    let mut publisher = Publisher::new(cfg);
    publisher.connect().await.unwrap();

    // Unroutable, refused and missing-exchange messages fail...
    assert!(publisher.publish("a", &message()).await.is_err());
    assert!(publisher.publish("b", &message()).await.is_err());
    assert!(publisher.publish("c", &message()).await.is_err());
    // ...and a closed channel is replaced for the next one
    assert!(publisher.publish("d", &message()).await.is_ok());
    publisher.close().await;

    let channel_opens = methods(&log).iter().filter(|(_, m)| *m == CHANNEL_OPEN).count();
    assert_eq!(channel_opens, 2);
    assert!(methods(&log).contains(&(CHANNEL, CHANNEL_CLOSE_OK)));

    // Through the plugin: one bad target fails the send, the rest are still tried
    let (port, log) = broker(Script { frame_max: 131_072, unroutable: vec!["bad"], ..Default::default() }).await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/bad/good", port)).unwrap();
    assert!(!svc.send(&ctx("Body")).await.unwrap());
    assert_eq!(method_args(&log, BASIC_PUBLISH).len(), 2);
  }

  #[tokio::test]
  async fn test_connect_failures() {
    // Login refused
    let (port, _) = broker(Script { frame_max: 131_072, refuse_login: true, ..Default::default() }).await;
    let svc = from_url(&format!("amqp://user:bad@127.0.0.1:{}/q", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.is_err());

    // A broker that never answers is given up on
    let (port, _) = broker(Script { silent: true, ..Default::default() }).await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/q?cto=0.2&rto=0.2", port)).unwrap();
    let started = std::time::Instant::now();
    assert!(svc.send(&ctx("Body")).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));

    // Nothing listening (a freed test port could be taken by a parallel test)
    assert!(from_url("amqp://127.0.0.1:1/q").unwrap().send(&ctx("Body")).await.is_err());
  }

  #[tokio::test]
  async fn test_tls() {
    let acceptor = super::super::tests::test_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let shared = log.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        if let Ok(tls) = acceptor.accept(stream).await {
          tokio::spawn(serve(Box::new(tls), Script { frame_max: 131_072, ..Default::default() }, shared.clone()));
        }
      }
    });

    // The self-signed certificate is refused unless verify=no
    let svc = from_url(&format!("amqps://localhost:{}/q", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.is_err());
    let svc = from_url(&format!("amqps://localhost:{}/q?verify=no", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.unwrap());
    assert_eq!(method_args(&log, BASIC_PUBLISH).len(), 1);
  }

  fn from_url_config(url: &str) -> Config {
    let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
    super::super::Amqp::from_url(&parsed).unwrap().config
  }
}

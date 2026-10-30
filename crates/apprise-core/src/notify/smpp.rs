use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no, parse_phone_no_with_prefix};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEFAULT_PORT: u16 = 2775;
const DEFAULT_SECURE_PORT: u16 = 3550;

/// Connect and read timeout of the SMPP client upstream uses (`smpplib`).
const TIMEOUT: Duration = Duration::from_secs(5);

const BIND_TRANSMITTER: u32 = 0x0000_0002;
const SUBMIT_SM: u32 = 0x0000_0004;
const UNBIND: u32 = 0x0000_0006;
const ENQUIRE_LINK: u32 = 0x0000_0015;
const RESPONSE: u32 = 0x8000_0000;

/// SMPP 3.4.
const INTERFACE_VERSION: u8 = 0x34;
/// International type of number.
const TON_INTL: u8 = 0x01;
/// SMSC default alphabet (GSM 7-bit, one character per octet).
const ENCODING_DEFAULT: u8 = 0x00;
/// UCS-2.
const ENCODING_UCS2: u8 = 0x08;
/// User data starts with a header (concatenated message parts).
const ESM_UDHI: u8 = 0x40;

/// A single message holds 160 GSM characters or 140 octets of UCS-2; parts
/// of a longer one leave room for the user data header.
const SEVENBIT_LENGTH: usize = 160;
const SEVENBIT_PART_SIZE: usize = 153;
const UCS2_LENGTH: usize = 140;
const UCS2_PART_SIZE: usize = 134;

/// The GSM 03.38 table smpplib encodes with. A character's index is its
/// code; indexes from 0x80 are the extension table, sent as ESC + index.
const GSM_CHARACTER_TABLE: &str = concat!(
  "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞ\x1bÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>",
  "?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ`¿abcdefghijklmnopqrstuvwxyzäöñüà",
  "````````````````````^```````````````````{}`````\\````````````[~]`",
  "|````````````````````````````````````€``````````````````````````",
);

pub struct Smpp {
  host: String,
  port: u16,
  user: String,
  password: String,
  source: String,
  targets: Vec<String>,
  tags: Vec<String>,
}

impl Smpp {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = (url.user.clone().unwrap_or_default(), url.password.clone().unwrap_or_default());
    if user.is_empty() || password.is_empty() {
      tracing::warn!("No SMPP user/pass combination was provided");
      return None;
    }

    let secure = url.schema == "smpps";
    // ParsedUrl has applied ?port= already
    let port = url.port.filter(|p| *p > 0).unwrap_or(if secure { DEFAULT_SECURE_PORT } else { DEFAULT_PORT });

    // The path holds the source number followed by the targets, unless
    // ?from= gives the source.
    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_phone_no() also unquotes each entry
      entries.extend(parse_phone_no(&[to]).iter().map(|t| unquote(t)));
    }
    let source = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => Some(unquote(from)),
      None if !entries.is_empty() => Some(entries.remove(0)),
      None => None,
    };
    let Some(source) = source.as_deref().and_then(|s| is_phone_no(s, 10)).map(|p| p.full) else {
      tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source.unwrap_or_default());
      return None;
    };

    let mut targets = Vec::new();
    for target in parse_phone_no_with_prefix(&entries) {
      match is_phone_no(&target, 10) {
        Some(number) => targets.push(number.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { host: url.host.clone().unwrap_or_default(), port, user, password, source, targets, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMPP",
      service_url: Some("https://smpp.org/"),
      setup_url: Some("https://appriseit.com/services/smpp/"),
      protocols: vec!["smpp", "smpps"],
      description: "Send SMS messages through an SMPP server.",
      attachment_support: false,
    }
  }

  fn bind_pdu(&self, sequence: u32) -> Vec<u8> {
    let mut body = Vec::new();
    push_cstring(&mut body, &self.user, 16); // system_id
    push_cstring(&mut body, &self.password, 9);
    push_cstring(&mut body, "", 13); // system_type
    body.push(INTERFACE_VERSION);
    body.push(0); // addr_ton
    body.push(0); // addr_npi
    push_cstring(&mut body, "", 41); // address_range
    pdu(BIND_TRANSMITTER, sequence, &body)
  }

  fn submit_pdu(&self, sequence: u32, target: &str, short_message: &[u8], data_coding: u8, esm_class: u8) -> Vec<u8> {
    let mut body = Vec::new();
    push_cstring(&mut body, "", 6); // service_type
    body.push(TON_INTL); // source_addr_ton
    body.push(0); // source_addr_npi
    push_cstring(&mut body, &self.source, 21);
    body.push(TON_INTL); // dest_addr_ton
    body.push(0); // dest_addr_npi
    push_cstring(&mut body, target, 21);
    body.push(esm_class);
    body.push(0); // protocol_id
    body.push(0); // priority_flag
    push_cstring(&mut body, "", 17); // schedule_delivery_time
    push_cstring(&mut body, "", 17); // validity_period
    body.push(1); // registered_delivery
    body.push(0); // replace_if_present_flag
    body.push(data_coding);
    body.push(0); // sm_default_msg_id
    body.push(short_message.len() as u8);
    body.extend_from_slice(short_message);
    pdu(SUBMIT_SM, sequence, &body)
  }
}

/// A PDU: length, command, status (0) and sequence, all big-endian.
fn pdu(command_id: u32, sequence: u32, body: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(16 + body.len());
  out.extend_from_slice(&((16 + body.len()) as u32).to_be_bytes());
  out.extend_from_slice(&command_id.to_be_bytes());
  out.extend_from_slice(&0u32.to_be_bytes());
  out.extend_from_slice(&sequence.to_be_bytes());
  out.extend_from_slice(body);
  out
}

/// A C-string field cut, as smpplib does, to fit `max` octets with its NUL.
/// Characters are written as latin-1 where they fit.
fn push_cstring(out: &mut Vec<u8>, value: &str, max: usize) {
  for c in value.chars().take(max - 1) {
    match u8::try_from(c as u32) {
      Ok(b) => out.push(b),
      Err(_) => out.extend_from_slice(c.to_string().as_bytes()),
    }
  }
  out.push(0);
}

/// GSM 7-bit encode, one character per octet, or `None` when a character
/// is not in the table.
fn gsm_encode(text: &str) -> Option<Vec<u8>> {
  let table: Vec<char> = GSM_CHARACTER_TABLE.chars().collect();
  let mut out = Vec::new();
  for c in text.chars() {
    let index = table.iter().position(|t| *t == c)?;
    if index < 0x80 {
      out.push(index as u8);
    } else {
      out.push(0x1b);
      out.push((index - 0x80) as u8);
    }
  }
  Some(out)
}

/// smpplib `make_parts()`: encode as GSM 7-bit, or UCS-2 when that is not
/// possible, and split a message that is too long into parts that each
/// start with a concatenation header sharing one reference number.
/// Returns the parts, the data coding and the ESM class.
fn make_parts(text: &str) -> Result<(Vec<Vec<u8>>, u8, u8), String> {
  let (encoded, coding, length, part_size) = match gsm_encode(text) {
    Some(e) => (e, ENCODING_DEFAULT, SEVENBIT_LENGTH, SEVENBIT_PART_SIZE),
    None => (text.encode_utf16().flat_map(u16::to_be_bytes).collect(), ENCODING_UCS2, UCS2_LENGTH, UCS2_PART_SIZE),
  };
  if encoded.len() <= length {
    return Ok((vec![encoded], coding, 0));
  }
  let chunks: Vec<&[u8]> = encoded.chunks(part_size).collect();
  if chunks.len() > 255 {
    return Err("message too long".into());
  }
  let reference: u8 = rand::random();
  let parts = chunks
    .iter()
    .enumerate()
    .map(|(i, chunk)| {
      let mut part = vec![0x05, 0x00, 0x03, reference, chunks.len() as u8, (i + 1) as u8];
      part.extend_from_slice(chunk);
      part
    })
    .collect();
  Ok((parts, coding, ESM_UDHI))
}

/// The message parts of one target as `[parts, data_coding, esm_class]`,
/// for [`crate::delivery::remember`].
fn parts_to_json(built: &(Vec<Vec<u8>>, u8, u8)) -> Value {
  let parts: Vec<String> = built.0.iter().map(|p| base64::engine::general_purpose::STANDARD.encode(p)).collect();
  json!([parts, built.1, built.2])
}

fn parts_from_json(value: &Value) -> Option<(Vec<Vec<u8>>, u8, u8)> {
  let parts =
    value.get(0)?.as_array()?.iter().map(|p| p.as_str().and_then(|p| base64::engine::general_purpose::STANDARD.decode(p).ok())).collect::<Option<_>>()?;
  Some((parts, u8::try_from(value.get(1)?.as_u64()?).ok()?, u8::try_from(value.get(2)?.as_u64()?).ok()?))
}

/// Largest PDU accepted from the server.
const MAX_PDU_LEN: usize = 64 * 1024;

/// Read one PDU and return its command ID, status and sequence.
async fn read_pdu(stream: &mut TcpStream) -> Result<(u32, u32, u32), String> {
  let io = async {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await.map_err(|e| e.to_string())?;
    let word = |i: usize| u32::from_be_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let length = word(0) as usize;
    // The PDUs read here are short responses; refuse a length that would
    // make us allocate whatever the server claims.
    if !(16..=MAX_PDU_LEN).contains(&length) {
      return Err("broken PDU".to_string());
    }
    let mut body = vec![0u8; length - 16];
    stream.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    Ok((word(4), word(8), word(12)))
  };
  tokio::time::timeout(TIMEOUT, io).await.map_err(|_| "timed out".to_string())?
}

/// Read the response to the PDU with `sequence`, answering any keepalive
/// the server sends first. Returns its command status.
async fn read_response(stream: &mut TcpStream, sequence: u32) -> Result<u32, String> {
  loop {
    let (command, status, seq) = read_pdu(stream).await?;
    if command == ENQUIRE_LINK {
      write(stream, &pdu(ENQUIRE_LINK | RESPONSE, seq, &[])).await?;
      continue;
    }
    if seq == sequence || command == RESPONSE {
      return Ok(status);
    }
  }
}

async fn write(stream: &mut TcpStream, data: &[u8]) -> Result<(), String> {
  tokio::time::timeout(TIMEOUT, stream.write_all(data)).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())
}

#[async_trait]
impl Notify for Smpp {
  fn schemas(&self) -> &[&str] {
    &["smpp", "smpps"]
  }
  fn service_name(&self) -> &str {
    "SMPP"
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
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no SMPP targets to notify");
      return Ok(false);
    }

    // SMS has no title; one that reaches us goes in front of the body.
    let body = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\r\n{}", ctx.title, ctx.body) };

    // Upstream connects without TLS for smpps:// as well; only the default
    // port differs.
    let addr = format!("{}:{}", self.host, self.port);
    let mut stream = match tokio::time::timeout(TIMEOUT, TcpStream::connect(&addr)).await {
      Ok(Ok(s)) => s,
      Ok(Err(e)) => {
        tracing::warn!("Failed to establish connection to SMPP server {}: {}", self.host, e);
        return Ok(false);
      }
      Err(_) => {
        tracing::warn!("Failed to establish connection to SMPP server {}: timed out", self.host);
        return Ok(false);
      }
    };

    // smpplib numbers its first PDU 2.
    let mut sequence = 2u32;
    let bound = async {
      write(&mut stream, &self.bind_pdu(sequence)).await?;
      read_response(&mut stream, sequence).await
    }
    .await;
    match bound {
      Ok(0) => {}
      Ok(status) => {
        tracing::warn!("SMPP bind_transmitter was rejected (status {:#x})", status);
        return Ok(false);
      }
      Err(e) => {
        tracing::warn!("Failed to establish connection to SMPP server {}: {}", self.host, e);
        return Ok(false);
      }
    }

    let mut has_error = false;
    for target in &self.targets {
      // A retry skips targets that already accepted the message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      crate::notify::throttle().await;
      let result = async {
        // A retry reuses the parts, so every part keeps the reference
        // number the handset reassembles them by
        let parts_key = format!("parts:{}", target);
        let built = match crate::delivery::recall(&parts_key, false).as_ref().and_then(parts_from_json) {
          Some(built) => built,
          None => {
            let built = make_parts(&body)?;
            crate::delivery::remember(&parts_key, parts_to_json(&built), false);
            built
          }
        };
        let (parts, data_coding, esm_class) = built;
        for (no, part) in parts.iter().enumerate() {
          // Each part is sent on its own, so a retry only repeats the
          // parts that did not make it
          let part_key = format!("part:{}:{}", target, no);
          if crate::delivery::is_delivered(&part_key, false) {
            continue;
          }
          sequence += 1;
          write(&mut stream, &self.submit_pdu(sequence, target, part, data_coding, esm_class)).await?;
          match read_response(&mut stream, sequence).await? {
            0 => crate::delivery::mark_delivered(&part_key, false),
            status => return Err(format!("submit_sm was rejected (status {:#x})", status)),
          }
        }
        Ok::<_, String>(())
      }
      .await;
      match result {
        Ok(()) => {
          tracing::info!("Sent SMPP notification to {}", target);
          crate::delivery::mark_delivered(target, false);
        }
        Err(e) => {
          tracing::warn!("Failed to send SMPP notification: {}", e);
          has_error = true;
        }
      }
    }

    sequence += 1;
    if write(&mut stream, &pdu(UNBIND, sequence, &[])).await.is_ok() {
      let _ = read_response(&mut stream, sequence).await;
    }
    let _ = stream.shutdown().await;
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  fn parse(url: &str) -> Option<Smpp> {
    ParsedUrl::parse(url).and_then(|p| Smpp::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("smpp://user:pass@host:2775/{}/{}", "1".repeat(10), "a".repeat(32)),
      format!("smpp://user:pass@host:2775/{}", "1".repeat(10)),
      format!("smpp://user:pass@host/{}/{}", "1".repeat(10), "1".repeat(10)),
      format!("smpps://_?&from={}&to={},{}&user=user&password=pw", "1".repeat(10), "1".repeat(10), "1".repeat(10)),
      "smpp://user:pass@host/15551234567".to_string(),
      "smpps://user:pass@host/+15551234567/+15559876543".to_string(),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "smpp://",
      "smpp:///",
      "smpp://@/",
      "smpp://user@/",
      "smpp://user:pass/",
      "smpp://user:pass@/",
      "smpp://user@hostname",
      "smpp://user:pass@host:/",
      "smpp://user:pass@host:2775/",
      // The source must be a phone number
      "smpp://user:pass@host/123/15551230000",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let s = parse("smpp://user:pass@host/+1%20(555)%20123-4567/+15551230000,15551231111").unwrap();
    assert_eq!((s.host.as_str(), s.port), ("host", 2775));
    assert_eq!(s.source, "15551234567");
    assert_eq!(s.targets, vec!["15551230000", "15551231111"]);

    // ?from= keeps the first path entry as a target
    let s = parse("smpp://user:pass@host/15551231234/15551230000?to=+1-555-123-9999&from=%2B1%20555%20123%200000").unwrap();
    assert_eq!(s.source, "15551230000");
    assert_eq!(s.targets, vec!["15551231234", "15551230000", "15551239999"]);

    // Invalid targets are dropped; the URL still loads
    let s = parse(&format!("smpp://user:pass@host:2775/{}/{}", "1".repeat(10), "a".repeat(32))).unwrap();
    assert!(s.targets.is_empty());

    let s = parse(&format!("smpps://_?&from={}&to={},{}&user=user&password=pw", "1".repeat(10), "1".repeat(10), "2".repeat(10))).unwrap();
    assert_eq!((s.host.as_str(), s.port), ("_", 3550));
    assert_eq!((s.user.as_str(), s.password.as_str()), ("user", "pw"));
    assert_eq!(s.targets, vec!["1".repeat(10), "2".repeat(10)]);

    let s = parse("smpp://us%2Fer:p%40ss%2Fword@host/1111111111/2222222222").unwrap();
    assert_eq!((s.user.as_str(), s.password.as_str()), ("us/er", "p@ss/word"));
    // ?pass= replaces the password, ?user= turns the URL user into it
    assert_eq!(parse("smpp://user:pass@host/1111111111/2222222222?pass=other").unwrap().password, "other");
    let s = parse("smpp://admin@host/1111111111/2222222222?user=bob").unwrap();
    assert_eq!((s.user.as_str(), s.password.as_str()), ("bob", "admin"));

    // A port in the URL wins over the protocol default
    assert_eq!(parse("smpp://user:pass@host:9999/1111111111/2222222222").unwrap().port, 9999);
    assert_eq!(parse("smpps://user:pass@host:9998/1111111111/2222222222").unwrap().port, 9998);
  }

  #[test]
  fn test_phone_helpers() {
    // The shared helpers give upstream's results
    let full = |s: &str| is_phone_no(s, 10).map(|p| p.full);
    assert_eq!(full("+1 (555) 123-4567").as_deref(), Some("15551234567"));
    assert_eq!(full("123"), None);
    assert_eq!(full("1".repeat(15).as_str()), None);
    assert_eq!(full("tel:15551234567"), None);
    assert_eq!(parse_phone_no_with_prefix(&["+15551230000,15551231111"]), vec!["+15551230000", "15551231111"]);
    assert_eq!(parse_phone_no_with_prefix(&["+1 (555) 123-4567"]), vec!["+1 (555) 123-4567"]);
    assert_eq!(parse_phone_no_with_prefix(&["abc def"]), vec!["abc", "def"]);
  }

  #[test]
  fn test_make_parts() {
    // GSM 7-bit: '@' is 0x00, the euro sign is ESC + 0x65
    let (parts, coding, esm) = make_parts("@A€").unwrap();
    assert_eq!((parts, coding, esm), (vec![vec![0x00, 0x41, 0x1b, 0x65]], ENCODING_DEFAULT, 0));

    // Anything outside the GSM table switches to UCS-2
    let (parts, coding, _) = make_parts("Hi ☃").unwrap();
    assert_eq!(coding, ENCODING_UCS2);
    assert_eq!(parts, vec![vec![0x00, 0x48, 0x00, 0x69, 0x00, 0x20, 0x26, 0x03]]);

    // 160 GSM characters fit one message; 161 need two parts
    assert_eq!(make_parts(&"x".repeat(160)).unwrap().0.len(), 1);
    let (parts, coding, esm) = make_parts(&"x".repeat(400)).unwrap();
    assert_eq!((coding, esm), (ENCODING_DEFAULT, ESM_UDHI));
    assert_eq!(parts.len(), 3);
    for (i, part) in parts.iter().enumerate() {
      assert_eq!(&part[..3], &[0x05, 0x00, 0x03]);
      assert_eq!(part[3], parts[0][3], "every part shares the reference");
      assert_eq!((part[4], part[5]), (3, i as u8 + 1));
    }
    assert_eq!(parts[0].len(), 6 + 153);
    assert_eq!(parts[2].len(), 6 + 400 - 2 * 153);
  }

  #[test]
  fn test_pdu_bytes() {
    let s = parse("smpp://user:pass@host/15551234567/15559876543").unwrap();
    let mut expected = vec![0, 0, 0, 31, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 2];
    expected.extend_from_slice(b"user\0pass\0\0\x34\0\0\0");
    assert_eq!(s.bind_pdu(2), expected);

    let submit = s.submit_pdu(3, "15559876543", b"Hello", 0, 0);
    let mut body = Vec::new();
    body.extend_from_slice(b"\0\x01\x0015551234567\0\x01\x0015559876543\0");
    body.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 5]);
    body.extend_from_slice(b"Hello");
    let mut expected = ((16 + body.len()) as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3]);
    expected.extend_from_slice(&body);
    assert_eq!(submit, expected);

    // The same PDUs as smpplib 2.2.4 generates them
    assert_eq!(hex::encode(s.bind_pdu(2)), "0000001f000000020000000000000002757365720070617373000034000000");
    assert_eq!(
      hex::encode(s.submit_pdu(3, "15559876543", b"Hello", 0, 0)),
      "0000003c00000004000000000000000300010031353535313233343536370001003135353539383736353433000000000000010000000548656c6c6f"
    );

    // smpplib cuts the system_id to 15 and the password to 8 characters
    let s = parse("smpp://abcdefghijklmnopq:123456789012@host/15551234567/15559876543").unwrap();
    assert_eq!(&s.bind_pdu(2)[16..41], b"abcdefghijklmno\x0012345678\0");
  }

  /// A fake SMSC: answers bind, submit_sm and unbind, replying to the
  /// submits with `statuses` in turn (then 0), and records every PDU it
  /// receives over `sessions` SMPP sessions. Other local processes probe
  /// random ports, so a connection that does not start with a bind is
  /// dropped and does not count.
  async fn smsc_sessions(sessions: usize, statuses: Vec<u32>) -> (u16, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
      let mut statuses = statuses.into_iter();
      let mut received = Vec::new();
      let mut served = 0;
      while served < sessions {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bound = false;
        loop {
          let mut header = [0u8; 16];
          if stream.read_exact(&mut header).await.is_err() {
            break;
          }
          let length = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
          let command = u32::from_be_bytes(header[4..8].try_into().unwrap());
          let sequence = u32::from_be_bytes(header[12..16].try_into().unwrap());
          if !bound && (command != BIND_TRANSMITTER || !(16..=1024).contains(&length)) {
            break;
          }
          bound = true;
          let mut body = vec![0u8; length - 16];
          if stream.read_exact(&mut body).await.is_err() {
            break;
          }
          let mut whole = header.to_vec();
          whole.extend_from_slice(&body);
          received.push(whole);
          let status = if command == SUBMIT_SM { statuses.next().unwrap_or(0) } else { 0 };
          let mut reply = 16u32.to_be_bytes().to_vec();
          reply.extend_from_slice(&(command | RESPONSE).to_be_bytes());
          reply.extend_from_slice(&status.to_be_bytes());
          reply.extend_from_slice(&sequence.to_be_bytes());
          if command == SUBMIT_SM {
            // submit_sm_resp carries a message ID
            reply[3] = 19;
            reply.extend_from_slice(b"id\0");
          }
          if stream.write_all(&reply).await.is_err() || command == UNBIND {
            break;
          }
        }
        if bound {
          served += 1;
        }
      }
      received
    });
    (port, handle)
  }

  /// One SMPP session replying `submit_status` to every submit.
  async fn smsc(submit_status: u32) -> (u16, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    smsc_sessions(1, vec![submit_status; 16]).await
  }

  #[tokio::test]
  async fn test_send_protocol_exchange() {
    let (port, server) = smsc(0).await;
    let svc = from_url(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543/15559870000", port)).unwrap();
    let ctx = NotifyContext { title: "".into(), body: "Hello".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());

    let pdus = server.await.unwrap();
    let parsed = parse(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543/15559870000", port)).unwrap();
    assert_eq!(pdus.len(), 4);
    assert_eq!(pdus[0], parsed.bind_pdu(2));
    assert_eq!(pdus[1], parsed.submit_pdu(3, "15559876543", b"Hello", 0, 0));
    assert_eq!(pdus[2], parsed.submit_pdu(4, "15559870000", b"Hello", 0, 0));
    assert_eq!(pdus[3], pdu(UNBIND, 5, &[]));
  }

  #[tokio::test]
  async fn test_send_long_message_in_parts() {
    let (port, server) = smsc(0).await;
    let svc = from_url(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543", port)).unwrap();
    let ctx = NotifyContext { body: "x".repeat(200), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
    let pdus = server.await.unwrap();
    // bind, two parts, unbind
    assert_eq!(pdus.len(), 4);
    for (i, part) in pdus[1..3].iter().enumerate() {
      // esm_class sits after service_type, two addresses and their TON/NPI
      let esm = 16 + 1 + 2 + 12 + 2 + 12;
      assert_eq!(part[esm], ESM_UDHI);
      let short_message = &part[esm + 10..];
      assert_eq!(&short_message[..3], &[0x05, 0x00, 0x03]);
      assert_eq!((short_message[4], short_message[5]), (2, i as u8 + 1));
    }
  }

  #[tokio::test]
  async fn test_send_rejected_submit_fails() {
    let (port, server) = smsc(0x58).await;
    let svc = from_url(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543", port)).unwrap();
    assert!(!svc.send(&NotifyContext { body: "Hello".into(), ..Default::default() }).await.unwrap());
    // It still unbinds
    assert_eq!(server.await.unwrap().last().unwrap()[4..8], UNBIND.to_be_bytes());
  }

  #[tokio::test]
  async fn test_send_bind_rejected_or_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
      // Skip connections from other local processes that are not a bind
      let mut stream = loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = [0u8; 16];
        if stream.read_exact(&mut header).await.is_ok() && header[4..8] == BIND_TRANSMITTER.to_be_bytes() {
          break stream;
        }
      };
      let mut buf = [0u8; 64];
      let _ = stream.read(&mut buf).await;
      // bind_transmitter_resp with ESME_RINVPASWD
      let mut reply = 16u32.to_be_bytes().to_vec();
      reply.extend_from_slice(&(BIND_TRANSMITTER | RESPONSE).to_be_bytes());
      reply.extend_from_slice(&0x0eu32.to_be_bytes());
      reply.extend_from_slice(&2u32.to_be_bytes());
      let _ = stream.write_all(&reply).await;
    });
    let ctx = NotifyContext { body: "Hello".into(), ..Default::default() };
    let svc = from_url(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543", port)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // Port 1 is privileged, so no test can be listening on it; a port
    // freed by dropping a listener may be taken by another test meanwhile.
    let port = 1;
    let svc = from_url(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543", port)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // No valid targets
    let svc = from_url("smpp://user:pass@127.0.0.1/15551234567").unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = Smpp::static_details();
    assert_eq!(details.service_name, "SMPP");
    assert_eq!(details.protocols, vec!["smpp", "smpps"]);
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_retry_resends_only_missing_parts() {
    // Both targets get a two-part message; the second part to the second
    // target is rejected once
    let (port, server) = smsc_sessions(2, vec![0, 0, 0, 0x58]).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("smpp://user:pass@127.0.0.1:{}/15551234567/15559876543/15559870000?retry=1&wait=0", port)));
    let ctx = NotifyContext { body: "x".repeat(200), ..Default::default() };
    assert!(a.notify(&ctx).await.is_success());
    let submits: Vec<Vec<u8>> = server.await.unwrap().into_iter().filter(|p| p[4..8] == SUBMIT_SM.to_be_bytes()).collect();
    assert_eq!(submits.len(), 5);
    // short_message follows esm_class and nine more header bytes
    let esm = 16 + 1 + 2 + 12 + 2 + 12;
    let udh = |pdu: &[u8]| pdu[esm + 10..esm + 16].to_vec();
    // The retry sent only the rejected part, with its original reference
    assert_eq!(udh(&submits[4]), udh(&submits[3]));
    assert_eq!(udh(&submits[4])[5], 2);
  }
}

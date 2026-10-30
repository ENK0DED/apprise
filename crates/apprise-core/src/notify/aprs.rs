use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::url_args::{parse_list, split_path, url_credentials};
use async_trait::async_trait;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// APRS-IS servers by locale. See <https://www.aprs2.net/>.
const APRS_LOCALES: [(&str, &str); 6] = [
  ("NOAM", "noam.aprs2.net"),
  ("SOAM", "soam.aprs2.net"),
  ("EURO", "euro.aprs2.net"),
  ("ASIA", "asia.aprs2.net"),
  ("AUNZ", "aunz.aprs2.net"),
  ("ROTA", "rotate.aprs2.net"),
];

const DEFAULT_LOCALE: &str = "EURO";

/// APRS-IS port, supported by every core server.
const NOTIFY_PORT: u16 = 10152;

/// Apprise's registered APRS device ID (TOCALL). It must not change: it
/// tells the ham radio community which software sent a message.
const DEVICE_ID: &str = "APPRIS";

/// APRS messages are limited to 67 characters.
const BODY_MAXLEN: usize = 67;

/// Seconds between writes to APRS-IS (upstream `request_rate_per_sec`).
/// Servers that are not ready yet otherwise send incomplete responses.
const REQUEST_RATE: f64 = 0.8;

/// `?delay=` must be at least 0 and below this.
const MAX_DELAY: f64 = 5.0;

/// Characters APRS cannot carry and their replacements.
const BAD_CHARMAP: [(char, &str); 7] = [('Ä', "Ae"), ('Ö', "Oe"), ('Ü', "Ue"), ('ä', "ae"), ('ö', "oe"), ('ü', "ue"), ('ß', "ss")];

pub struct Aprs {
  user: String,
  password: String,
  targets: Vec<String>,
  locale: &'static str,
  delay: f64,
  timeout: Duration,
  /// Time of the last write, for the throttle between writes.
  last_io: std::sync::Mutex<Option<Instant>>,
  tags: Vec<String>,
}

impl Aprs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = url_credentials(url);
    let (user, password) = (user.unwrap_or_default(), password.unwrap_or_default());
    if user.is_empty() || password.is_empty() {
      tracing::warn!("An APRS user/pass was not provided.");
      return None;
    }
    // Sending needs write access; -1 is the read-only passcode.
    if password == "-1" {
      tracing::warn!("APRS read-only passwords are not supported.");
      return None;
    }
    if !password.chars().all(char::is_numeric) {
      tracing::warn!("Invalid APRS-IS password");
      return None;
    }

    let locale = match url.get("locale").filter(|l| !l.is_empty()) {
      Some(l) => match APRS_LOCALES.iter().find(|(k, _)| k.eq_ignore_ascii_case(l)) {
        Some((k, _)) => *k,
        None => {
          tracing::warn!("Unsupported APRS-IS server locale. Received: {}.", l);
          return None;
        }
      },
      None => DEFAULT_LOCALE,
    };

    let delay = match url.get("delay").filter(|d| !d.is_empty()) {
      Some(d) => match d.trim().parse::<f64>() {
        Ok(v) if (0.0..MAX_DELAY).contains(&v) => v,
        _ => {
          tracing::warn!("Unsupported APRS-IS delay ({}) specified.", d);
          return None;
        }
      },
      None => 0.0,
    };

    // The host and every path entry are call signs, as is ?to=.
    let mut raw = vec![url.host.clone().unwrap_or_default()];
    raw.extend(split_path(&url.path_parts));
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw.extend(parse_list(&[to]));
    }
    let mut targets = Vec::new();
    for target in parse_call_signs(&raw) {
      if is_call_sign(&target) {
        targets.push(target.to_uppercase());
      } else {
        tracing::warn!("Dropping invalid Amateur radio call sign ({}).", target);
      }
    }

    Some(Self {
      user: user.to_uppercase(),
      password,
      targets,
      locale,
      delay,
      timeout: crate::utils::socket::url_timeouts(url).connect_timeout().unwrap_or(Duration::from_secs(4)),
      last_io: std::sync::Mutex::new(None),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Aprs",
      service_url: Some("https://www.aprs2.net/"),
      setup_url: Some("https://appriseit.com/services/aprs/"),
      protocols: vec!["aprs"],
      description: "Send messages to amateur radio operators through APRS-IS.",
      attachment_support: false,
    }
  }

  fn server(&self) -> String {
    let host = APRS_LOCALES.iter().find(|(k, _)| *k == self.locale).map(|(_, h)| *h).unwrap_or("euro.aprs2.net");
    service_base(&format!("{}:{}", host, NOTIFY_PORT))
  }

  /// Wait until `request_rate + delay` seconds have passed since the last
  /// write (upstream `throttle()`). The first write never waits.
  async fn throttle(&self) {
    let rate = Duration::from_secs_f64(REQUEST_RATE + self.delay);
    let wait = {
      let last = self.last_io.lock().unwrap();
      last.map(|t| rate.saturating_sub(t.elapsed())).unwrap_or_default()
    };
    if !wait.is_zero() {
      tokio::time::sleep(wait).await;
    }
    *self.last_io.lock().unwrap() = Some(Instant::now());
  }

  async fn write(&self, stream: &mut TcpStream, data: &str) -> Result<(), String> {
    self.throttle().await;
    tokio::time::timeout(self.timeout, stream.write_all(data.as_bytes())).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())
  }

  /// Log in to APRS-IS and check the `# logresp CALL verified, ...` line.
  async fn login(&self, stream: &mut TcpStream) -> Result<(), String> {
    let login = format!("user {} pass {} vers apprise {}\r\n", self.user, self.password, env!("CARGO_PKG_VERSION"));
    self.write(stream, &login).await?;

    // The server greets with a banner line and answers the login with a
    // logresp line; wait for the latter.
    let deadline = tokio::time::Instant::now() + self.timeout;
    let mut received: Vec<u8> = Vec::new();
    let logresp = loop {
      // Responses are latin-1; only complete lines count.
      let complete = received.iter().rposition(|&b| b == b'\n').map(|i| &received[..i]).unwrap_or(&[]);
      let text: String = complete.iter().map(|&b| b as char).collect();
      if let Some(line) = text.lines().find(|l| l.starts_with("# logresp")) {
        break line.trim_end().to_string();
      }
      let mut buf = [0u8; 512];
      match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
        Ok(Ok(0)) => return Err("connection closed before the login response".into()),
        Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("timed out waiting for the login response".into()),
      }
    };
    tracing::debug!("Received content: {}", logresp);

    let fields: Vec<&str> = logresp.splitn(5, ' ').collect();
    if fields.len() < 5 {
      return Err("received invalid response from APRS-IS".into());
    }
    let (callsign, status) = (fields[2], fields[3]);
    if callsign != self.user {
      return Err(format!("call signs differ: {}", callsign));
    }
    if status.starts_with("unverified") {
      return Err("invalid APRS-IS password for given call sign".into());
    }
    Ok(())
  }
}

/// Prepare a message body for APRS: drop the characters APRS reserves,
/// replace umlauts, join lines into one (APRS-IS ends a packet at a line
/// break), cut it to 67 characters and end it with CRLF.
fn prepare_payload(body: &str) -> String {
  let mut text: String = body.chars().filter(|c| !matches!(c, '{' | '}' | '|' | '~')).collect();
  for (from, to) in BAD_CHARMAP {
    text = text.replace(from, to);
  }

  // Any run of spaces and tabs, line breaks, then spaces and tabs becomes
  // one space.
  let text = text.trim_matches(['\r', '\n']);
  let mut joined = String::with_capacity(text.len());
  let chars: Vec<char> = text.chars().collect();
  let mut i = 0;
  while i < chars.len() {
    let start = i;
    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
      i += 1;
    }
    if i < chars.len() && matches!(chars[i], '\r' | '\n') {
      while i < chars.len() && matches!(chars[i], '\r' | '\n') {
        i += 1;
      }
      while i < chars.len() && matches!(chars[i], ' ' | '\t') {
        i += 1;
      }
      joined.push(' ');
      continue;
    }
    if i > start {
      joined.extend(&chars[start..i]);
      continue;
    }
    joined.push(chars[i]);
    i += 1;
  }

  let truncated: String = joined.chars().take(BODY_MAXLEN).collect();
  format!("{}\r\n", truncated.trim_end_matches(['\r', '\n']))
}

/// Match a call sign at the start of `chars` (`[0-9a-z]{1,2}[0-9][a-z0-9]{1,3}`
/// with an optional `-` and one or two digit SSID, case-insensitive) and
/// return every length it can end at, in the order a backtracking regex
/// tries them.
fn call_sign_lengths(chars: &[char]) -> Vec<usize> {
  let alnum = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_alphanumeric());
  let digit = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_digit());
  let mut out = Vec::new();
  for prefix in [2, 1] {
    if !(0..prefix).all(alnum) || !digit(prefix) {
      continue;
    }
    for suffix in [3, 2, 1] {
      let core = prefix + 1 + suffix;
      if !(prefix + 1..core).all(alnum) {
        continue;
      }
      if chars.get(core) == Some(&'-') {
        for ssid in [2, 1] {
          if (core + 1..core + 1 + ssid).all(digit) {
            out.push(core + 1 + ssid);
          }
        }
      }
      out.push(core);
    }
  }
  out
}

/// Upstream `is_call_sign()`: the whole entry is one call sign, optionally
/// followed by whitespace.
fn is_call_sign(value: &str) -> bool {
  let chars: Vec<char> = value.chars().collect();
  call_sign_lengths(&chars).into_iter().any(|n| chars[n..].iter().all(|c| c.is_whitespace()))
}

/// Upstream `parse_call_sign()`: find every call sign in each entry, where a
/// call sign must be followed by the end of the entry or a separator. An
/// entry without any is split on the usual delimiters and kept, so it can be
/// reported as invalid.
fn parse_call_signs(entries: &[String]) -> Vec<String> {
  let mut result = Vec::new();
  for entry in entries.iter().filter(|e| !e.is_empty()) {
    let chars: Vec<char> = entry.chars().collect();
    let separator_follows = |end: usize| {
      let rest = &chars[end..];
      rest.iter().all(|c| c.is_whitespace()) || rest.first().is_some_and(|c| c.is_whitespace() || *c == ',' || *c == ';')
    };
    let mut found = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
      let mut start = pos;
      while start < chars.len() && chars[start].is_whitespace() {
        start += 1;
      }
      match call_sign_lengths(&chars[start..]).into_iter().find(|n| separator_follows(start + n)) {
        Some(n) => {
          found.push(chars[start..start + n].iter().collect::<String>());
          pos = start + n;
        }
        None => pos += 1,
      }
    }
    if found.is_empty() {
      result.extend(entry.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).filter(|s| !s.is_empty()).map(str::to_string));
    } else {
      result.extend(found);
    }
  }
  result
}

#[async_trait]
impl Notify for Aprs {
  fn schemas(&self) -> &[&str] {
    &["aprs"]
  }
  fn service_name(&self) -> &str {
    "Aprs"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    BODY_MAXLEN
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no amateur radio call signs to notify");
      return Ok(false);
    }

    // APRS has no title; one that reaches us goes in front of the body.
    let body = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\r\n{}", ctx.title, ctx.body) };
    let payload = prepare_payload(&body);

    let server = self.server();
    tracing::debug!("Creating socket connection with APRS-IS {}", server);
    let mut stream = match tokio::time::timeout(self.timeout, TcpStream::connect(&server)).await {
      Ok(Ok(s)) => s,
      Ok(Err(e)) => {
        tracing::warn!("Could not connect to APRS-IS {}: {}", server, e);
        return Ok(false);
      }
      Err(_) => {
        tracing::warn!("Timed out connecting to APRS-IS {}", server);
        return Ok(false);
      }
    };

    if let Err(e) = self.login(&mut stream).await {
      tracing::warn!("socket_login: Login to APRS-IS unsuccessful: {}", e);
      return Ok(false);
    }

    let mut sent = 0;
    for target in &self.targets {
      // Format: our call sign > device ID :: target (padded to 9) : body
      let buffer = format!("{}>{}::{:<9}:{}", self.user, DEVICE_ID, target, payload);
      tracing::debug!("Sending APRS message: {}", buffer.trim_end());
      if let Err(e) = self.write(&mut stream, &buffer).await {
        tracing::warn!("Socket Exception socket_send: {}", e);
        break;
      }
      sent += 1;
    }
    let _ = stream.shutdown().await;

    tracing::info!("Sent {}/{} APRS-IS notification(s)", sent, self.targets.len());
    Ok(sent == self.targets.len())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use tokio::io::{AsyncBufReadExt, BufReader};

  fn parse(url: &str) -> Option<Aprs> {
    ParsedUrl::parse(url).and_then(|p| Aprs::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "aprs://DF1JSL-15:12345@DF1ABC",
      "aprs://DF1JSL-15:12345@DF1ABC/DF1ABD",
      "aprs://DF1JSL-15:12345@DF1ABC-1/DF1ABD-12/x",
      "aprs://DF1JSL-15:12345@DF1ABC?locale=noam",
      "aprs://DF1JSL-15:12345@DF1ABC?locale=ROTA",
      "aprs://DF1JSL-15:12345@DF1ABC?delay=4.9",
      "aprs://DF1JSL-15:12345@DF1ABC?delay=",
      "aprs://df1jsl-15:12345@df1abc,df1abd?to=DF1ABF%20DF1ABG",
      // Not a call sign; it loads but has no one to notify
      "aprs://DF1JSL-15:12345@invalid_target",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "aprs://",
      "aprs://:@/",
      "aprs://DF1JSL-15@DF1ABC",
      "aprs://DF1JSL-15:-1@DF1ABC",
      "aprs://DF1JSL-15:abc@DF1ABC",
      "aprs://DF1JSL-15:12345@DF1ABC?locale=xxxx",
      "aprs://DF1JSL-15:12345@DF1ABC?delay=5",
      "aprs://DF1JSL-15:12345@DF1ABC?delay=-1",
      "aprs://DF1JSL-15:12345@DF1ABC?delay=abc",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let a = parse("aprs://df1jsl-15:12345@df1abc,df1abd?to=DF1ABF%20df1abg&locale=asia&delay=1.5").unwrap();
    assert_eq!(a.user, "DF1JSL-15");
    assert_eq!(a.password, "12345");
    assert_eq!(a.targets, vec!["DF1ABC", "DF1ABD", "DF1ABF", "DF1ABG"]);
    assert_eq!(a.locale, "ASIA");
    assert_eq!(a.delay, 1.5);

    let a = parse("aprs://DF1JSL-15:12345@DF1ABC").unwrap();
    assert_eq!(a.locale, "EURO");
    assert_eq!(a.delay, 0.0);
    assert_eq!(a.timeout, Duration::from_secs(4));

    // A call sign is searched for inside each entry, as upstream does: only
    // "123" in "DF1ABE-123" qualifies, and "x" is dropped.
    let a = parse("aprs://DF1JSL-15:12345@DF1ABC-1/DF1ABD-12/DF1ABE-123/x").unwrap();
    assert_eq!(a.targets, vec!["DF1ABC-1", "DF1ABD-12", "123"]);
  }

  #[test]
  fn test_call_sign_helpers() {
    assert!(is_call_sign("DF1ABC"));
    assert!(is_call_sign("df1abc-15"));
    assert!(is_call_sign("1A2B"));
    assert!(is_call_sign("DF1ABC "));
    assert!(!is_call_sign("X"));
    assert!(!is_call_sign("DF1ABCDE"));
    assert!(!is_call_sign("DF1ABC-123"));
    assert_eq!(parse_call_signs(&["df1abc,df1abd".to_string()]), vec!["df1abc", "df1abd"]);
    assert_eq!(parse_call_signs(&["DF1ABE-123".to_string()]), vec!["123"]);
    assert_eq!(parse_call_signs(&["a b;c".to_string()]), vec!["a", "b", "c"]);
  }

  #[test]
  fn test_prepare_payload() {
    assert_eq!(prepare_payload("hello"), "hello\r\n");
    // Reserved characters are dropped and umlauts replaced
    assert_eq!(prepare_payload("{a}|b~ Äpfel Grüße"), "ab Aepfel Gruesse\r\n");
    // Lines are joined with one space and surrounding blanks collapse
    assert_eq!(prepare_payload("\r\nline one  \r\n\r\n  line two\nthree\r\n"), "line one line two three\r\n");
    // Blanks that do not touch a line break are kept
    assert_eq!(prepare_payload("a \t b\t\n\tc"), "a \t b c\r\n");
    // 67 characters at most
    assert_eq!(prepare_payload(&"x".repeat(100)), format!("{}\r\n", "x".repeat(67)));
  }

  /// A minimal APRS-IS server: greets, answers the login with `logresp`,
  /// then records every line it receives.
  async fn aprs_server(verified: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.unwrap();
      let (reader, mut writer) = stream.into_split();
      writer.write_all(b"# aprsc 2.1.4\r\n").await.unwrap();
      let mut reader = BufReader::new(reader);
      let mut lines = Vec::new();
      let mut line = String::new();
      while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        if line.starts_with("user ") {
          let call = line.split(' ').nth(1).unwrap_or("").to_string();
          writer.write_all(format!("# logresp {} {}, server T2TEST\r\n", call, verified).as_bytes()).await.unwrap();
        }
        lines.push(std::mem::take(&mut line));
      }
      lines
    });
    (addr, handle)
  }

  #[tokio::test]
  async fn test_send_protocol_exchange() {
    let (addr, server) = aprs_server("verified").await;
    let svc = from_url("aprs://df1jsl-15:12345@DF1ABC/DF1ABD-12").unwrap();
    let ctx = NotifyContext { title: "Alert".into(), body: "Disk {full} on\nserver".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(addr, svc.send(&ctx)).await.unwrap());

    let lines = server.await.unwrap();
    assert_eq!(
      lines,
      vec![
        format!("user DF1JSL-15 pass 12345 vers apprise {}\r\n", env!("CARGO_PKG_VERSION")),
        "DF1JSL-15>APPRIS::DF1ABC   :Alert Disk full on server\r\n".to_string(),
        "DF1JSL-15>APPRIS::DF1ABD-12:Alert Disk full on server\r\n".to_string(),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_unverified_login_fails() {
    let (addr, server) = aprs_server("unverified").await;
    let svc = from_url("aprs://DF1JSL-15:12345@DF1ABC").unwrap();
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(addr, svc.send(&ctx)).await.unwrap());
    // Nothing but the login was sent
    assert_eq!(server.await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn test_send_callsign_mismatch_fails() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = [0u8; 256];
      let _ = stream.read(&mut buf).await;
      let _ = stream.write_all(b"# aprsc 2.1.4\r\n# logresp OTHER verified, server T2TEST\r\n").await;
      let _ = stream.read(&mut buf).await;
    });
    let svc = from_url("aprs://DF1JSL-15:12345@DF1ABC").unwrap();
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(addr, svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets_or_connection() {
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    let svc = from_url("aprs://DF1JSL-15:12345@invalid_target").unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // Nothing listens on this port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let svc = from_url("aprs://DF1JSL-15:12345@DF1ABC").unwrap();
    assert!(!crate::notify::with_test_base(addr, svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_server_by_locale() {
    assert_eq!(parse("aprs://DF1JSL-15:12345@DF1ABC").unwrap().server(), "euro.aprs2.net:10152");
    assert_eq!(parse("aprs://DF1JSL-15:12345@DF1ABC?locale=rota").unwrap().server(), "rotate.aprs2.net:10152");
  }

  #[test]
  fn test_static_details() {
    let details = Aprs::static_details();
    assert_eq!(details.service_name, "Aprs");
    assert_eq!(details.protocols, vec!["aprs"]);
    assert!(!details.attachment_support);
  }
}

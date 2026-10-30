//! A minimal SMTP client (upstream `apprise/plugins/email/smtp.py`, which
//! drives Python's `smtplib`).
//!
//! One [`SmtpConnection`] carries every message of a notification, as
//! upstream's `AppriseSMTPController` does: connect (implicit TLS for
//! `ssl`), upgrade with STARTTLS for `starttls`, log in when both a user and
//! a password are set, then one `MAIL`/`RCPT`/`DATA` exchange per message.
//! Commands and replies follow `smtplib` (`ehlo`, `mail from:<...> size=N`,
//! dot-stuffing, the AUTH mechanism order), so servers see what upstream
//! sends.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use hmac::{KeyInit, Mac};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Longest reply line accepted (`smtplib._MAXLINE`).
const MAX_LINE: usize = 8192;

/// Most AUTH challenges answered before giving up (`smtplib._MAXCHALLENGE`).
const MAX_CHALLENGE: usize = 5;

/// Transport security (upstream `SMTPSecureMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureMode {
  /// No transport security at all.
  Insecure,
  /// Implicit TLS from the first byte of the connection.
  Ssl,
  /// A plaintext connection upgraded to TLS with STARTTLS.
  StartTls,
}

impl SecureMode {
  /// Parse a `?mode=` value (already lowercased by the caller).
  pub fn parse(value: &str) -> Option<Self> {
    match value {
      "insecure" => Some(Self::Insecure),
      "ssl" => Some(Self::Ssl),
      "starttls" => Some(Self::StartTls),
      _ => None,
    }
  }

  /// The port used when the URL gives none (upstream `SMTP_DEFAULT_PORTS`).
  pub fn default_port(self) -> u16 {
    match self {
      Self::StartTls => 587,
      Self::Ssl => 465,
      Self::Insecure => 25,
    }
  }
}

/// Why an SMTP operation failed. The text is for debug logs only; it never
/// holds credentials.
#[derive(Debug)]
pub struct SmtpError(pub String);

impl std::fmt::Display for SmtpError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

fn err<T>(msg: impl Into<String>) -> Result<T, SmtpError> {
  Err(SmtpError(msg.into()))
}

enum Stream {
  Plain(TcpStream),
  Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
  Closed,
}

/// Where and how to connect.
pub struct SmtpConfig<'a> {
  pub host: &'a str,
  pub port: u16,
  pub mode: SecureMode,
  pub user: Option<&'a str>,
  pub password: Option<&'a str>,
  /// The TLS client for `ssl` and `starttls` (it decides whether the
  /// certificate is verified).
  pub tls: tokio_rustls::TlsConnector,
  /// Applied to the connect and to every read and write (upstream passes
  /// `socket_connect_timeout` to `smtplib`, which uses it for the socket).
  pub timeout: Duration,
}

/// An open, authenticated SMTP session.
pub struct SmtpConnection {
  stream: Stream,
  buffer: Vec<u8>,
  timeout: Duration,
  host: String,
  local_hostname: String,
  /// EHLO extensions (lowercase name to parameters), as `smtplib` keeps them.
  features: HashMap<String, String>,
  does_esmtp: bool,
  greeted: bool,
}

impl SmtpConnection {
  /// Connect, secure and log in (upstream `AppriseSMTPController.__enter__`).
  pub async fn open(config: SmtpConfig<'_>) -> Result<Self, SmtpError> {
    tracing::debug!("Connecting to remote SMTP server...");
    let addr = (config.host.trim_matches(|c| c == '[' || c == ']'), config.port);
    #[cfg(test)]
    let addr = super::tests::smtp_target(addr.0, addr.1);
    let tcp = match tokio::time::timeout(config.timeout, TcpStream::connect(addr)).await {
      Ok(Ok(tcp)) => tcp,
      Ok(Err(e)) => return err(format!("connect failed: {}", e)),
      Err(_) => return err("connect timed out"),
    };
    let mut conn = Self {
      stream: Stream::Plain(tcp),
      buffer: Vec::new(),
      timeout: config.timeout,
      host: config.host.to_string(),
      local_hostname: local_hostname(),
      features: HashMap::new(),
      does_esmtp: false,
      greeted: false,
    };
    let result = conn.establish(&config).await;
    if let Err(e) = result {
      // Close a connection that failed during STARTTLS or login
      conn.quit().await;
      return Err(e);
    }
    Ok(conn)
  }

  async fn establish(&mut self, config: &SmtpConfig<'_>) -> Result<(), SmtpError> {
    if config.mode == SecureMode::Ssl {
      tracing::debug!("Securing connection with SSL...");
      self.upgrade(&config.tls).await?;
    }
    let (code, msg) = self.reply().await?;
    if code != 220 {
      self.close();
      return err(format!("connect error {}: {}", code, msg));
    }
    if config.mode == SecureMode::StartTls {
      tracing::debug!("Securing connection with STARTTLS...");
      self.ehlo_or_helo_if_needed().await?;
      if !self.features.contains_key("starttls") {
        return err("STARTTLS extension not supported by server.");
      }
      let (code, msg) = self.command("STARTTLS").await?;
      if code != 220 {
        return err(format!("STARTTLS refused {}: {}", code, msg));
      }
      self.upgrade(&config.tls).await?;
      // RFC 3207: forget what the server said before the TLS negotiation
      self.features.clear();
      self.does_esmtp = false;
      self.greeted = false;
    }
    if let (Some(user), Some(password)) = (config.user.filter(|u| !u.is_empty()), config.password.filter(|p| !p.is_empty())) {
      tracing::debug!("Applying user credentials...");
      self.login(user, password).await?;
    }
    Ok(())
  }

  async fn upgrade(&mut self, tls: &tokio_rustls::TlsConnector) -> Result<(), SmtpError> {
    // Bytes read past the STARTTLS reply came in the clear; treating them as
    // TLS replies would let a man in the middle inject responses.
    if !self.buffer.is_empty() {
      self.close();
      return err("unexpected data after the STARTTLS reply");
    }
    let Stream::Plain(tcp) = std::mem::replace(&mut self.stream, Stream::Closed) else {
      return err("connection is not plaintext");
    };
    let host = self.host.trim_matches(|c| c == '[' || c == ']');
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| SmtpError(format!("invalid server name: {}", e)))?;
    match tokio::time::timeout(self.timeout, tls.connect(name, tcp)).await {
      Ok(Ok(stream)) => {
        self.stream = Stream::Tls(Box::new(stream));
        Ok(())
      }
      Ok(Err(e)) => err(format!("TLS handshake failed: {}", e)),
      Err(_) => err("TLS handshake timed out"),
    }
  }

  /// Send one message (upstream `AppriseSMTPController.sendmail`). Returns
  /// false when the server refused it or any of its recipients.
  pub async fn sendmail(&mut self, from: &str, to: &[String], message: &str) -> bool {
    match self.try_sendmail(from, to, message).await {
      Ok(refused) if refused.is_empty() => true,
      Ok(refused) => {
        for (addr, code, resp) in refused {
          tracing::warn!("SMTP recipient {} was refused: {} {}", addr, code, resp);
        }
        false
      }
      Err(e) => {
        tracing::debug!("Socket Exception: {}", e);
        false
      }
    }
  }

  /// `smtplib.SMTP.sendmail()`: the refused recipients, or an error when
  /// the message was not accepted at all.
  async fn try_sendmail(&mut self, from: &str, to: &[String], message: &str) -> Result<Vec<(String, i32, String)>, SmtpError> {
    self.ehlo_or_helo_if_needed().await?;
    let data = to_wire(message);
    let mut options = String::new();
    if self.does_esmtp && self.features.contains_key("size") {
      // The size counts the CRLF message before dot-stuffing, as smtplib does
      options = format!(" size={}", crlf_len(message));
    }
    let (code, resp) = self.command(&format!("mail from:<{}>{}", from, options)).await?;
    if code != 250 {
      if code == 421 {
        self.close();
      } else {
        self.rset().await;
      }
      return err(format!("sender refused {}: {}", code, resp));
    }
    let mut refused = Vec::new();
    for rcpt in to {
      let (code, resp) = self.command(&format!("rcpt to:<{}>", rcpt)).await?;
      if code != 250 && code != 251 {
        refused.push((rcpt.clone(), code, resp));
      }
      if code == 421 {
        self.close();
        return err("recipients refused");
      }
    }
    if refused.len() == to.len() {
      // The server refused every recipient
      self.rset().await;
      for (addr, code, resp) in &refused {
        tracing::warn!("SMTP recipient {} was refused: {} {}", addr, code, resp);
      }
      return err("all recipients refused");
    }
    let (code, resp) = self.command("data").await?;
    if code != 354 {
      return err(format!("data refused {}: {}", code, resp));
    }
    self.write(&data).await?;
    let (code, resp) = self.reply().await?;
    if code != 250 {
      if code == 421 {
        self.close();
      } else {
        self.rset().await;
      }
      return err(format!("data error {}: {}", code, resp));
    }
    Ok(refused)
  }

  /// End the session (upstream `__exit__`); failures are ignored, the
  /// connection may already be gone.
  pub async fn quit(&mut self) {
    if !matches!(self.stream, Stream::Closed) {
      let _ = self.command("quit").await;
    }
    self.close();
  }

  fn close(&mut self) {
    self.stream = Stream::Closed;
    self.buffer.clear();
  }

  async fn rset(&mut self) {
    let _ = self.command("rset").await;
  }

  async fn ehlo_or_helo_if_needed(&mut self) -> Result<(), SmtpError> {
    if self.greeted {
      return Ok(());
    }
    let (code, _) = self.ehlo().await?;
    if !(200..=299).contains(&code) {
      let (code, msg) = self.command(&format!("helo {}", self.local_hostname)).await?;
      if !(200..=299).contains(&code) {
        return err(format!("helo error {}: {}", code, msg));
      }
    }
    self.greeted = true;
    Ok(())
  }

  async fn ehlo(&mut self) -> Result<(i32, String), SmtpError> {
    self.features.clear();
    let (code, msg) = self.command(&format!("ehlo {}", self.local_hostname)).await?;
    if code != 250 {
      return Ok((code, msg));
    }
    self.does_esmtp = true;
    for line in msg.split('\n').skip(1) {
      if line.len() >= 5 && line[..5].eq_ignore_ascii_case("auth=") {
        // An old-style "AUTH=LOGIN PLAIN" line
        let entry = self.features.entry("auth".to_string()).or_default();
        entry.push(' ');
        entry.push_str(&line[5..]);
        continue;
      }
      let name_len = line.bytes().enumerate().take_while(|(i, b)| b.is_ascii_alphanumeric() || (*i > 0 && *b == b'-')).count();
      if name_len == 0 {
        continue;
      }
      let feature = line[..name_len].to_ascii_lowercase();
      let params = line[name_len..].trim().to_string();
      if feature == "auth" {
        let entry = self.features.entry(feature).or_default();
        entry.push(' ');
        entry.push_str(&params);
      } else {
        self.features.insert(feature, params);
      }
    }
    Ok((code, msg))
  }

  /// `smtplib.SMTP.login()`: try CRAM-MD5, PLAIN and LOGIN, in that order,
  /// among the mechanisms the server offers.
  async fn login(&mut self, user: &str, password: &str) -> Result<(), SmtpError> {
    self.ehlo_or_helo_if_needed().await?;
    tracing::trace!("Login ID: {}", user);
    let Some(advertised) = self.features.get("auth").cloned() else {
      return err("SMTP AUTH extension not supported by server.");
    };
    let advertised: Vec<&str> = advertised.split_whitespace().collect();
    let methods: Vec<&str> = ["CRAM-MD5", "PLAIN", "LOGIN"].into_iter().filter(|m| advertised.contains(m)).collect();
    if methods.is_empty() {
      return err("No suitable authentication method found.");
    }
    let mut last = SmtpError("authentication failed".to_string());
    for method in methods {
      match self.auth(method, user, password).await? {
        (235 | 503, _) => return Ok(()),
        (code, _) => last = SmtpError(format!("authentication failed with {} ({})", method, code)),
      }
    }
    Err(last)
  }

  async fn auth(&mut self, method: &str, user: &str, password: &str) -> Result<(i32, String), SmtpError> {
    let b64 = |s: &[u8]| base64::engine::general_purpose::STANDARD.encode(s);
    let (mut code, mut resp) = match method {
      // CRAM-MD5 has no initial response
      "CRAM-MD5" => self.command("AUTH CRAM-MD5").await?,
      "PLAIN" => self.command(&format!("AUTH PLAIN {}", b64(format!("\0{}\0{}", user, password).as_bytes()))).await?,
      _ => self.command(&format!("AUTH LOGIN {}", b64(user.as_bytes()))).await?,
    };
    let mut challenges = usize::from(method != "CRAM-MD5");
    while code == 334 {
      challenges += 1;
      let challenge = base64::engine::general_purpose::STANDARD.decode(resp.trim()).unwrap_or_default();
      let answer = match method {
        "CRAM-MD5" => {
          let mut mac = hmac::Hmac::<md5::Md5>::new_from_slice(password.as_bytes()).expect("HMAC takes any key length");
          mac.update(&challenge);
          format!("{} {}", user, hex::encode(mac.finalize().into_bytes()))
        }
        "PLAIN" => format!("\0{}\0{}", user, password),
        _ if challenges < 2 => user.to_string(),
        _ => password.to_string(),
      };
      (code, resp) = self.command(&b64(answer.as_bytes())).await?;
      if challenges > MAX_CHALLENGE {
        return err("Server AUTH mechanism infinite loop.");
      }
    }
    Ok((code, resp))
  }

  /// Send one command line and read the reply.
  async fn command(&mut self, line: &str) -> Result<(i32, String), SmtpError> {
    self.write(format!("{}\r\n", line).as_bytes()).await?;
    self.reply().await
  }

  async fn write(&mut self, data: &[u8]) -> Result<(), SmtpError> {
    let result = match &mut self.stream {
      Stream::Plain(s) => {
        tokio::time::timeout(self.timeout, async {
          s.write_all(data).await?;
          s.flush().await
        })
        .await
      }
      Stream::Tls(s) => {
        tokio::time::timeout(self.timeout, async {
          s.write_all(data).await?;
          s.flush().await
        })
        .await
      }
      Stream::Closed => return err("please run connect() first"),
    };
    match result {
      Ok(Ok(())) => Ok(()),
      Ok(Err(e)) => {
        self.close();
        err(format!("write failed: {}", e))
      }
      Err(_) => {
        self.close();
        err("write timed out")
      }
    }
  }

  /// Read one (possibly multi-line) reply (`smtplib.SMTP.getreply()`).
  async fn reply(&mut self) -> Result<(i32, String), SmtpError> {
    let mut lines = Vec::new();
    let code = loop {
      let line = self.read_line().await?;
      let text = String::from_utf8_lossy(line.get(4..).unwrap_or_default()).trim_matches([' ', '\t', '\r', '\n']).to_string();
      lines.push(text);
      let code = std::str::from_utf8(line.get(..3).unwrap_or(&line)).ok().and_then(|c| c.trim().parse::<i32>().ok());
      let Some(code) = code else { break -1 };
      if line.get(3) != Some(&b'-') {
        break code;
      }
    };
    Ok((code, lines.join("\n")))
  }

  async fn read_line(&mut self) -> Result<Vec<u8>, SmtpError> {
    loop {
      if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
        if pos + 1 > MAX_LINE {
          self.close();
          return err("500 Line too long.");
        }
        return Ok(self.buffer.drain(..=pos).collect());
      }
      if self.buffer.len() > MAX_LINE {
        self.close();
        return err("500 Line too long.");
      }
      let mut chunk = [0u8; 4096];
      let result = match &mut self.stream {
        Stream::Plain(s) => tokio::time::timeout(self.timeout, s.read(&mut chunk)).await,
        Stream::Tls(s) => tokio::time::timeout(self.timeout, s.read(&mut chunk)).await,
        Stream::Closed => return err("please run connect() first"),
      };
      match result {
        Ok(Ok(0)) | Ok(Err(_)) => {
          self.close();
          return err("Connection unexpectedly closed");
        }
        Ok(Ok(n)) => self.buffer.extend_from_slice(&chunk[..n]),
        Err(_) => {
          self.close();
          return err("timed out reading the reply");
        }
      }
    }
  }
}

/// Python's `re.sub(r'(?:\r\n|\n|\r(?!\n))', '\r\n', text)`.
pub fn to_crlf(text: &str) -> String {
  let mut out = String::with_capacity(text.len() + text.len() / 16);
  let mut chars = text.chars().peekable();
  while let Some(c) = chars.next() {
    match c {
      '\r' => {
        if chars.peek() == Some(&'\n') {
          chars.next();
        }
        out.push_str("\r\n");
      }
      '\n' => out.push_str("\r\n"),
      _ => out.push(c),
    }
  }
  out
}

fn crlf_len(message: &str) -> usize {
  to_crlf(message).len()
}

/// The DATA payload: CRLF line endings, leading dots doubled, and the
/// terminating `.` line (`smtplib.SMTP.data()`).
fn to_wire(message: &str) -> Vec<u8> {
  let crlf = to_crlf(message);
  let mut out = Vec::with_capacity(crlf.len() + 8);
  for (i, line) in crlf.split("\r\n").enumerate() {
    if i > 0 {
      out.extend_from_slice(b"\r\n");
    }
    if line.starts_with('.') {
      out.push(b'.');
    }
    out.extend_from_slice(line.as_bytes());
  }
  if !out.ends_with(b"\r\n") {
    out.extend_from_slice(b"\r\n");
  }
  out.extend_from_slice(b".\r\n");
  out
}

/// The name sent with EHLO. Python's `socket.getfqdn()` is not available
/// without a DNS lookup, so this is the machine's host name when it is a
/// valid one, otherwise `localhost`.
fn local_hostname() -> String {
  let candidates = [std::fs::read_to_string("/proc/sys/kernel/hostname").ok(), std::env::var("HOSTNAME").ok(), std::env::var("COMPUTERNAME").ok()];
  candidates
    .into_iter()
    .flatten()
    .map(|h| h.trim().to_string())
    .find(|h| crate::utils::validate::verify_hostname(h, false, false, false).is_some())
    .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_secure_modes() {
    assert_eq!(SecureMode::parse("starttls"), Some(SecureMode::StartTls));
    assert_eq!(SecureMode::parse("ssl"), Some(SecureMode::Ssl));
    assert_eq!(SecureMode::parse("insecure"), Some(SecureMode::Insecure));
    assert_eq!(SecureMode::parse("plain"), None);
    assert_eq!(SecureMode::StartTls.default_port(), 587);
    assert_eq!(SecureMode::Ssl.default_port(), 465);
    assert_eq!(SecureMode::Insecure.default_port(), 25);
  }

  #[test]
  fn test_wire_format() {
    assert_eq!(to_crlf("a\nb\r\nc\rd"), "a\r\nb\r\nc\r\nd");
    assert_eq!(to_wire("a\n.b\n..c\n"), b"a\r\n..b\r\n...c\r\n.\r\n");
    assert_eq!(to_wire(".x"), b"..x\r\n.\r\n");
    assert_eq!(crlf_len("a\nb"), 4);
  }
}

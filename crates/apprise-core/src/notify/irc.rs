//! IRC notifications: a small client that registers, optionally joins the
//! target channels, sends one PRIVMSG per message line and quits.
//!
//! Mirrors upstream's `apprise/plugins/irc/` package (base, client,
//! protocol, state and templates).

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use crate::utils::url_args::{parse_bool_with_default, parse_list, split_path, url_credentials};
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

const DEFAULT_INSECURE_PORT: u16 = 6667;
const DEFAULT_SECURE_PORT: u16 = 6697;

/// Registering with a large network can take 20 seconds.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to keep reading (and answering PINGs) after a command.
#[cfg(not(test))]
const PUMP_INTERVAL: Duration = Duration::from_millis(750);
#[cfg(test)]
const PUMP_INTERVAL: Duration = Duration::from_millis(100);

/// EFnet allows 9-character nicknames, so generated ones stay within that.
const NICKNAME_MAX_LENGTH: usize = 9;

/// How many nickname collisions (432/433) to work around before failing.
const NICKNAME_COLLISION_MAX: u32 = 3;

/// RFC 2812 limits a line to 512 bytes; leave room for the prefix.
const BODY_MAXLEN: usize = 380;

#[derive(Debug, Clone, Copy, PartialEq)]
enum AuthMode {
  /// No authentication.
  None,
  /// PASS during registration.
  Server,
  /// NickServ IDENTIFY after registration.
  NickServ,
  /// A ZNC bouncer: PASS user:password, then a liveness check.
  Znc,
}

/// Upstream `IRC_AUTH_MODES`, in the order a prefix is matched.
const AUTH_MODES: [(&str, AuthMode); 4] = [("znc", AuthMode::Znc), ("server", AuthMode::Server), ("nickserv", AuthMode::NickServ), ("none", AuthMode::None)];

/// Defaults for well-known networks (upstream `IRC_TEMPLATES`), applied when
/// the URL gives no port: the domain suffix, TLS, port and auth mode.
const NETWORK_TEMPLATES: [(&str, &str, bool, u16, AuthMode); 3] = [
  ("SynIRC", ".synirc.net", true, 6697, AuthMode::NickServ),
  ("Libera.Chat", ".libera.chat", true, 6697, AuthMode::NickServ),
  ("EFnet", ".efnet.org", false, 6667, AuthMode::Server),
];

/// Registration failures (upstream `REGISTER_ERRORS`).
const REGISTER_ERRORS: [(u16, &str); 3] = [(464, "Password incorrect"), (465, "Banned from server"), (468, "Only registered users allowed")];

/// Join failures (upstream `JOIN_ERRORS`).
const JOIN_ERRORS: [(u16, &str); 8] = [
  (403, "No such channel"),
  (471, "Channel is full"),
  (473, "Invite only channel"),
  (474, "Banned from channel"),
  (475, "Bad channel key"),
  (476, "Bad channel mask"),
  (477, "Need to be registered"),
  (489, "Cannot join channel"),
];

pub struct Irc {
  host: String,
  port: u16,
  secure: bool,
  verify: bool,
  user: Option<String>,
  password: Option<String>,
  /// `?nick=` or the URL user; a random one is generated when empty.
  nickname: String,
  /// `?name=`; the asset description is used when empty.
  fullname: String,
  auth_mode: AuthMode,
  join: bool,
  /// Channel names (without `#`) and their keys.
  channels: Vec<(String, Option<String>)>,
  users: Vec<String>,
  timeout: Duration,
  join_timeout: Duration,
  send_timeout: Duration,
  tags: Vec<String>,
}

impl Irc {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let (user, password) = url_credentials(url);
    let mut secure = url.schema == "ircs";

    let mut entries = Vec::new();
    // With ?host=, the URL's host is a target too (it does not replace the
    // host).
    if url.get("host").is_some_and(|h| !h.is_empty()) {
      entries.push(urlencoding::decode(&host).map(|h| h.into_owned()).unwrap_or_else(|_| host.clone()));
    }
    entries.extend(split_path(url));
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.push(to.to_string());
    }

    let mut port = match url.get("port") {
      Some(p) => Some(p.trim().parse::<u16>().ok().filter(|p| *p > 0)?),
      None => url.port,
    };
    let mut auth_mode = AuthMode::Server;
    if port.is_none() {
      let lower = host.to_lowercase();
      if let Some((name, _, tls, p, mode)) = NETWORK_TEMPLATES.iter().find(|(_, suffix, ..)| lower.ends_with(suffix) && lower.len() > suffix.len()) {
        tracing::info!("Applying {} Defaults", name);
        secure = *tls;
        port = Some(*p);
        auth_mode = *mode;
      }
    }

    if let Some(mode) = url.get("mode").map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty()) {
      match AUTH_MODES.iter().find(|(name, _)| name.starts_with(&mode)) {
        Some((_, m)) => auth_mode = *m,
        None => {
          tracing::warn!("The IRC auth mode specified ({}) is invalid.", mode);
          return None;
        }
      }
    }

    let mut channels: Vec<(String, Option<String>)> = Vec::new();
    let mut users = Vec::new();
    for target in parse_list(&entries) {
      if let Some((channel, key)) = parse_channel(&target) {
        match channels.iter_mut().find(|(c, _)| *c == channel) {
          Some(existing) => existing.1 = key,
          None => channels.push((channel, key)),
        }
      } else if let Some(nick) = parse_user(&target) {
        users.push(nick);
      } else {
        tracing::warn!("Dropped invalid IRC target ({}).", target);
      }
    }

    let nickname = url
      .get("nick")
      .map(str::trim)
      .filter(|n| !n.is_empty())
      .map(str::to_string)
      .or_else(|| user.as_deref().map(|u| u.trim().to_string()))
      .unwrap_or_default();
    let fullname = url.get("name").map(|n| n.trim().to_string()).unwrap_or_default();
    // A value that is not a recognised boolean reads as false.
    let join = url.get("join").map(|v| parse_bool_with_default(v, false)).unwrap_or(true);

    let timeouts = crate::utils::socket::url_timeouts(url);
    let rto = timeouts.read_timeout().unwrap_or(Duration::from_secs(4)).as_secs_f64();

    Some(Self {
      port: port.unwrap_or(if secure { DEFAULT_SECURE_PORT } else { DEFAULT_INSECURE_PORT }),
      host,
      secure,
      verify: url.verify_certificate(),
      user,
      password,
      nickname,
      fullname,
      auth_mode,
      join,
      channels,
      users,
      timeout: Duration::from_secs_f64(rto),
      join_timeout: Duration::from_secs_f64(rto.clamp(6.0, 12.0)),
      send_timeout: Duration::from_secs_f64(rto.clamp(4.0, 10.0)),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "IRC",
      service_url: Some("https://ircv3.net/"),
      setup_url: Some("https://appriseit.com/services/irc/"),
      protocols: vec!["irc", "ircs"],
      description: "Send messages to IRC channels and users.",
      attachment_support: false,
    }
  }

  /// The PASS value: the password, or `user:password` for a ZNC bouncer.
  /// Only the server and ZNC modes send one.
  fn pass(&self) -> Option<String> {
    let password = self.password.as_deref().filter(|p| !p.is_empty())?;
    match self.auth_mode {
      AuthMode::Server => Some(password.to_string()),
      AuthMode::Znc => Some(format!("{}:{}", self.user.as_deref().unwrap_or(""), password)),
      AuthMode::None | AuthMode::NickServ => None,
    }
  }

  async fn deliver<S: AsyncRead + AsyncWrite + Unpin>(&self, client: &mut Client<S>, message: &str) -> Result<(), String> {
    client.register(self.pass().as_deref(), REGISTER_TIMEOUT, self.send_timeout).await?;

    // Upstream clears the password before identifying, so it never sends
    // IDENTIFY; apprise-go sends it, and so does this port.
    if self.auth_mode == AuthMode::NickServ
      && let Some(password) = self.password.as_deref().filter(|p| !p.is_empty())
    {
      client.write(&format!("PRIVMSG NickServ :IDENTIFY {}", password), self.send_timeout).await?;
      client.pump(PUMP_INTERVAL).await?;
    }

    // A bouncer is already registered upstream; check that it answers.
    if self.auth_mode == AuthMode::Znc && !client.check_connection(self.send_timeout.min(Duration::from_secs(5))).await? {
      return Err("ZNC connection check failed".into());
    }

    for (channel, key) in &self.channels {
      let chan = format!("#{}", channel);
      if self.join || key.is_some() {
        client.join(&chan, key.as_deref(), self.join_timeout, self.send_timeout).await?;
      }
      client.privmsg(&chan, message, self.send_timeout).await?;
      tracing::info!("Sent IRC notification to #{} as @{}", channel, client.accepted_nick);
    }
    for user in &self.users {
      client.privmsg(user, message, self.send_timeout).await?;
      tracing::info!("Sent IRC notification to @{} as @{}", user, client.accepted_nick);
    }
    Ok(())
  }

  /// Connect, optionally over TLS, run the exchange and quit.
  async fn run(&self, nickname: String, fullname: String, ctx: &NotifyContext, message: &str) -> Result<(), String> {
    let addr = format!("{}:{}", self.host, self.port);
    let tcp = tokio::time::timeout(self.timeout, TcpStream::connect(&addr)).await.map_err(|_| "connect timed out".to_string())?.map_err(|e| e.to_string())?;
    let _ = crate::utils::socket::set_keepalive(&tcp, 60);
    let stream: Box<dyn IrcStream> = if self.secure {
      let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| format!("invalid TLS server name: {}", e))?;
      let tls = tokio::time::timeout(self.timeout, crate::utils::socket::tls_connector(self.verify).connect(name, tcp))
        .await
        .map_err(|_| "TLS handshake timed out".to_string())?
        .map_err(|e| format!("TLS handshake failed: {}", e))?;
      Box::new(tls)
    } else {
      Box::new(tcp)
    };

    let mut client = Client::new(stream, nickname, fullname, &ctx.asset.app_id);
    let result = async {
      self.deliver(&mut client, message).await?;
      client.write(&format!("QUIT :{}", ctx.asset.app_desc), self.send_timeout).await
    }
    .await;
    let _ = client.stream.shutdown().await;
    result
  }
}

/// Upstream `IS_CHANNEL`: `#name` or `#name:key`.
fn parse_channel(target: &str) -> Option<(String, Option<String>)> {
  let rest = target.trim_start();
  let rest = rest.strip_prefix('#').or_else(|| rest.strip_prefix("%23"))?.trim_end();
  let end = rest.find(|c: char| " \t\r\n@#:".contains(c)).unwrap_or(rest.len());
  let (channel, tail) = rest.split_at(end);
  if channel.is_empty() {
    return None;
  }
  match tail.strip_prefix(':') {
    None if tail.is_empty() => Some((channel.to_string(), None)),
    Some(key) if !key.is_empty() && !key.contains([' ', '\t', '\r', '\n']) => Some((channel.to_string(), Some(key.to_string()))),
    _ => None,
  }
}

/// Upstream `IS_USER`: an optional `@`, then a nickname.
fn parse_user(target: &str) -> Option<String> {
  let rest = target.trim_start();
  let rest = rest.strip_prefix('@').or_else(|| rest.strip_prefix("%40")).unwrap_or(rest);
  (!rest.is_empty() && !rest.contains([' ', '\t', '\r', '\n', '@', '#'])).then(|| rest.to_string())
}

/// Upstream `IRCClient.nick_generation()`: up to 6 characters of the prefix,
/// lowercased, then random characters; a collision count replaces the last.
fn generate_nick(prefix: &str, collision: u32) -> String {
  const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
  let base: String = prefix.chars().take(NICKNAME_MAX_LENGTH - 3).collect::<String>().trim().to_lowercase();
  let suffix: String = (0..NICKNAME_MAX_LENGTH).map(|_| CHARSET[rand::random_range(0..CHARSET.len())] as char).collect();
  let mut nick: String = format!("{}{}", base, suffix);
  if collision > 0 {
    nick = format!("{}{}", nick.chars().take(NICKNAME_MAX_LENGTH - 1).collect::<String>(), collision % 10);
  }
  nick.chars().take(NICKNAME_MAX_LENGTH).collect()
}

/// Python `str.splitlines()`: every kind of line break ends a line.
fn split_lines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((i, c)) = chars.next() {
    if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
      lines.push(&text[start..i]);
      let mut end = i + c.len_utf8();
      if c == '\r' && chars.peek().is_some_and(|(_, n)| *n == '\n') {
        chars.next();
        end += 1;
      }
      start = end;
    }
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

/// One parsed IRC line: `[:prefix] command params [:trailing]`.
#[derive(Debug, PartialEq)]
struct Message {
  command: String,
  params: Vec<String>,
  trailing: Option<String>,
}

impl Message {
  fn parse(line: &str) -> Self {
    let mut rest = line.trim_end_matches(['\r', '\n']);
    if let Some(stripped) = rest.strip_prefix(':') {
      rest = stripped.split_once(' ').map(|(_, r)| r).unwrap_or("");
    }
    let mut trailing = None;
    if let Some((before, after)) = rest.split_once(" :") {
      trailing = Some(after.to_string());
      rest = before;
    }
    let mut words = rest.split_whitespace().map(str::to_string);
    let command = words.next().unwrap_or_default();
    Self { command, params: words.collect(), trailing }
  }

  fn numeric(&self) -> Option<u16> {
    (self.command.len() == 3 && self.command.chars().all(|c| c.is_ascii_digit())).then(|| self.command.parse().ok()).flatten()
  }

  /// Upstream `_err()`: the trailing text, else the parameters.
  fn error_text(&self) -> String {
    self.trailing.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| if self.params.is_empty() { "IRC error".into() } else { self.params.join(" ") })
  }
}

/// A plain or TLS connection.
trait IrcStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IrcStream for T {}

/// What the client is waiting for, which decides how replies are read.
enum Phase {
  Registering,
  Joining,
  Idle,
}

struct Client<S> {
  stream: S,
  inbuf: Vec<u8>,
  fullname: String,
  desired_nick: String,
  accepted_nick: String,
  prefix: String,
  collisions: u32,
  registered: bool,
  joined: Vec<String>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
  /// `prefix` seeds the nicknames generated after a collision.
  fn new(stream: S, nickname: String, fullname: String, prefix: &str) -> Self {
    Self {
      stream,
      inbuf: Vec::new(),
      fullname,
      accepted_nick: nickname.clone(),
      desired_nick: nickname,
      prefix: prefix.to_string(),
      collisions: 0,
      registered: false,
      joined: Vec::new(),
    }
  }

  async fn write(&mut self, line: &str, timeout: Duration) -> Result<(), String> {
    let payload = format!("{}\r\n", line);
    tokio::time::timeout(timeout, self.stream.write_all(payload.as_bytes()))
      .await
      .map_err(|_| "timeout while writing IRC commands".to_string())?
      .map_err(|e| e.to_string())
  }

  /// The next line, or `None` once `deadline` passes.
  async fn read_line(&mut self, deadline: Instant) -> Result<Option<String>, String> {
    loop {
      if let Some(i) = self.inbuf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = self.inbuf.drain(..=i).collect();
        let line = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
        tracing::trace!("IRC read: {}", line);
        return Ok(Some(line));
      }
      let mut buf = [0u8; 4096];
      match tokio::time::timeout_at(deadline, self.stream.read(&mut buf)).await {
        Err(_) => return Ok(None),
        Ok(Ok(0)) => return Err("connection closed by the server".into()),
        Ok(Ok(n)) => self.inbuf.extend_from_slice(&buf[..n]),
        Ok(Err(e)) => return Err(e.to_string()),
      }
    }
  }

  /// React to one line from the server.
  async fn handle(&mut self, line: &str, phase: &Phase, timeout: Duration) -> Result<(), String> {
    let msg = Message::parse(line);
    if msg.command.eq_ignore_ascii_case("PING") {
      let payload = msg.trailing.clone().or_else(|| msg.params.first().cloned()).unwrap_or_default();
      return self.write(&format!("PONG :{}", payload), timeout).await;
    }
    let numeric = msg.numeric();
    if matches!(numeric, Some(432 | 433)) {
      if self.collisions >= NICKNAME_COLLISION_MAX {
        return Err("Nickname is already in use".into());
      }
      self.collisions += 1;
      self.desired_nick = generate_nick(&self.prefix, self.collisions);
      let nick = format!("NICK {}", self.desired_nick);
      return self.write(&nick, timeout).await;
    }
    match phase {
      Phase::Registering => {
        if let Some((_, reason)) = REGISTER_ERRORS.iter().find(|(n, _)| Some(*n) == numeric) {
          return Err(format!("{}: {}", reason, msg.error_text()));
        }
        if numeric == Some(1) {
          if let Some(nick) = msg.params.first() {
            self.accepted_nick = nick.clone();
          }
          self.registered = true;
        }
      }
      Phase::Joining => {
        if let Some((_, reason)) = JOIN_ERRORS.iter().find(|(n, _)| Some(*n) == numeric) {
          return Err(format!("{}: {}", reason, msg.error_text()));
        }
        if matches!(numeric, Some(443 | 366)) && msg.params.len() >= 2 {
          self.joined.push(msg.params[1].clone());
        } else if msg.command.eq_ignore_ascii_case("JOIN") {
          let chan = msg.trailing.clone().filter(|t| !t.is_empty()).or_else(|| msg.params.first().cloned()).unwrap_or_default();
          if !chan.is_empty() {
            self.joined.push(chan);
          }
        }
      }
      Phase::Idle => {}
    }
    Ok(())
  }

  /// Keep reading for `duration`, answering PINGs.
  async fn pump(&mut self, duration: Duration) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    while let Some(line) = self.read_line(deadline).await? {
      self.handle(&line, &Phase::Idle, duration).await?;
    }
    Ok(())
  }

  async fn register(&mut self, pass: Option<&str>, timeout: Duration, write_timeout: Duration) -> Result<(), String> {
    if let Some(pass) = pass {
      self.write(&format!("PASS {}", pass), write_timeout).await?;
    }
    let nick = format!("NICK {}", self.desired_nick);
    self.write(&nick, write_timeout).await?;
    let user = format!("USER {} 0 * :{}", self.desired_nick, self.fullname);
    self.write(&user, write_timeout).await?;

    let deadline = Instant::now() + timeout;
    while !self.registered {
      match self.read_line(deadline).await? {
        Some(line) => self.handle(&line, &Phase::Registering, write_timeout).await?,
        None => return Err("IRC registration timeout".into()),
      }
    }
    Ok(())
  }

  /// Send a PING and wait for any PONG (bouncers do not always echo the
  /// token).
  async fn check_connection(&mut self, timeout: Duration) -> Result<bool, String> {
    self.write("PING :apprise", timeout).await?;
    let deadline = Instant::now() + timeout;
    while let Some(line) = self.read_line(deadline).await? {
      if Message::parse(&line).command.eq_ignore_ascii_case("PONG") {
        return Ok(true);
      }
    }
    Ok(false)
  }

  async fn join(&mut self, channel: &str, key: Option<&str>, timeout: Duration, write_timeout: Duration) -> Result<(), String> {
    let is_joined = |joined: &[String]| joined.iter().any(|c| c.eq_ignore_ascii_case(channel));
    if is_joined(&self.joined) {
      return Ok(());
    }
    let line = match key {
      Some(key) => format!("JOIN {} {}", channel, key),
      None => format!("JOIN {}", channel),
    };
    self.write(&line, write_timeout).await?;
    let deadline = Instant::now() + timeout;
    while !is_joined(&self.joined) {
      match self.read_line(deadline).await? {
        Some(line) => self.handle(&line, &Phase::Joining, write_timeout).await?,
        None => {
          tracing::debug!("IRC join confirmation not observed for {}", channel);
          break;
        }
      }
    }
    Ok(())
  }

  /// One PRIVMSG per line; IRC ends every command at a line break, and
  /// blank lines are skipped.
  async fn privmsg(&mut self, target: &str, message: &str, timeout: Duration) -> Result<(), String> {
    for line in split_lines(message).into_iter().filter(|l| !l.trim().is_empty()) {
      self.write(&format!("PRIVMSG {} :{}", target, line), timeout).await?;
    }
    self.pump(PUMP_INTERVAL.min(timeout)).await
  }
}

#[async_trait]
impl Notify for Irc {
  fn schemas(&self) -> &[&str] {
    &["irc", "ircs"]
  }
  fn service_name(&self) -> &str {
    "IRC"
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
    if self.channels.is_empty() && self.users.is_empty() {
      tracing::warn!("No IRC targets specified.");
      return Ok(false);
    }

    let nickname = if self.nickname.is_empty() { generate_nick(&ctx.asset.app_id, 0) } else { self.nickname.clone() };
    let fullname = if self.fullname.is_empty() { ctx.asset.app_desc.clone() } else { self.fullname.clone() };
    let message = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{} {}", ctx.title, ctx.body).trim().to_string() };

    match self.run(nickname.clone(), fullname, ctx, &message).await {
      Ok(()) => Ok(true),
      Err(e) => {
        tracing::warn!("Failed to send IRC notification to {} as @{}.", self.host, nickname);
        tracing::debug!("IRC Exception: {}", e);
        Ok(false)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use tokio::io::{AsyncBufReadExt, BufReader};

  fn parse(url: &str) -> Option<Irc> {
    ParsedUrl::parse(url).and_then(|p| Irc::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      // These load; sending fails as there is no one to notify
      "irc://",
      "irc://irc.example.com",
      "irc://irc.example.com/chan",
      "irc://irc.example.com/#chan",
      "irc://irc.example.com:6667/#chan",
      "ircs://irc.example.com/#chan:key/@bob/%40alice?join=no",
      "irc://me:pw@irc.libera.chat/#chan",
      "irc://irc.example.com/%23a/@b?to=%23c,@d&join=no&name=Z&nick=N&mode=none",
      "irc://irc.example.com/#chan?mode=n",
      "irc://irc.example.com/#chan?mode=z",
      "irc://irc.example.com/#chan?mode=",
      "irc://%23chan?host=irc.example.com",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["irc://irc.example.com/#chan?mode=invalid", "irc://irc.example.com/#chan?mode=bad"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_targets() {
    // Targets are sorted and deduplicated as upstream parse_list() does
    let irc = parse("ircs://irc.example.com/@bob/%23chan2:key/%40alice/#chan/@bob/bad%23?to=carol").unwrap();
    assert_eq!(irc.channels, vec![("chan".to_string(), None), ("chan2".to_string(), Some("key".to_string()))]);
    assert_eq!(irc.users, vec!["alice", "bob", "carol"]);

    // A bare entry is a user, not a channel
    let irc = parse("irc://irc.example.com/chan").unwrap();
    assert!(irc.channels.is_empty());
    assert_eq!(irc.users, vec!["chan"]);

    let irc = parse("irc://irc.example.com/%23a/@b?to=%23c,@d&join=no&name=Z&nick=N&mode=none").unwrap();
    assert_eq!(irc.channels.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), vec!["a", "c"]);
    assert_eq!(irc.users, vec!["b", "d"]);
    assert!(!irc.join);
    assert_eq!((irc.fullname.as_str(), irc.nickname.as_str(), irc.auth_mode), ("Z", "N", AuthMode::None));

    // ?host= makes the URL's host a target; it stays the host
    let irc = parse("irc://%23chan?host=irc.example.com").unwrap();
    assert_eq!(irc.channels, vec![("chan".to_string(), None)]);
  }

  #[test]
  fn test_modes_and_defaults() {
    let irc = parse("irc://irc.example.com/#chan").unwrap();
    assert_eq!((irc.port, irc.secure, irc.auth_mode), (6667, false, AuthMode::Server));
    assert_eq!((irc.timeout, irc.join_timeout, irc.send_timeout), (Duration::from_secs(4), Duration::from_secs(6), Duration::from_secs(4)));
    assert!(irc.join);
    assert_eq!(parse("ircs://irc.example.com/#chan").unwrap().port, 6697);

    // A prefix picks the first mode in upstream order (znc, server,
    // nickserv, none), case-insensitively
    for (mode, expected) in [("n", AuthMode::NickServ), ("NICKServ", AuthMode::NickServ), ("z", AuthMode::Znc), ("s", AuthMode::Server), ("no", AuthMode::None)]
    {
      assert_eq!(parse(&format!("irc://irc.example.com/#c?mode={}", mode)).unwrap().auth_mode, expected, "mode={}", mode);
    }

    // Known networks apply their defaults unless a port is given
    let irc = parse("irc://me:pw@irc.libera.chat/#chan").unwrap();
    assert_eq!((irc.port, irc.secure, irc.auth_mode), (6697, true, AuthMode::NickServ));
    let irc = parse("irc://irc.synirc.net/#c?mode=none").unwrap();
    assert_eq!((irc.port, irc.secure, irc.auth_mode), (6697, true, AuthMode::None));
    let irc = parse("ircs://irc.efnet.org/#c").unwrap();
    assert_eq!((irc.port, irc.secure), (6667, false));
    let irc = parse("irc://me:pw@irc.libera.chat:6667/#chan").unwrap();
    assert_eq!((irc.port, irc.secure, irc.auth_mode), (6667, false, AuthMode::Server));
    assert!(!parse("irc://irc.unknown.tld/#c").unwrap().secure);

    // ?rto= scales the join and send timeouts within their bounds
    let irc = parse("irc://irc.example.com/#chan?rto=20").unwrap();
    assert_eq!((irc.join_timeout, irc.send_timeout), (Duration::from_secs(12), Duration::from_secs(10)));

    assert_eq!(parse("irc://user@irc.example.com/#chan").unwrap().nickname, "user");
    assert_eq!(parse("irc://user@irc.example.com/#chan?nick=Other").unwrap().nickname, "Other");
    assert!(!parse("irc://irc.example.com/#chan?verify=no").unwrap().verify);
    assert!(!parse("irc://irc.example.com/#chan?join=maybe").unwrap().join);
  }

  #[test]
  fn test_pass_by_mode() {
    assert_eq!(parse("irc://me:pw@h/#c").unwrap().pass().as_deref(), Some("pw"));
    assert_eq!(parse("irc://me:pw@h/#c?mode=znc").unwrap().pass().as_deref(), Some("me:pw"));
    assert_eq!(parse("irc://me:pw@h/#c?mode=nickserv").unwrap().pass(), None);
    assert_eq!(parse("irc://me:pw@h/#c?mode=none").unwrap().pass(), None);
    assert_eq!(parse("irc://me@h/#c").unwrap().pass(), None);
  }

  #[test]
  fn test_protocol_helpers() {
    let msg = Message::parse(":srv 001 nick :welcome to IRC");
    assert_eq!(msg, Message { command: "001".into(), params: vec!["nick".into()], trailing: Some("welcome to IRC".into()) });
    assert_eq!(msg.numeric(), Some(1));
    let ping = Message::parse("PING :abc");
    assert_eq!((ping.command.as_str(), ping.trailing.as_deref()), ("PING", Some("abc")));
    assert_eq!(Message::parse(":srv 366 nick #chan :End").params, vec!["nick", "#chan"]);
    assert_eq!(Message::parse("").command, "");

    assert_eq!(parse_channel("#chan"), Some(("chan".into(), None)));
    assert_eq!(parse_channel("%23chan2:key"), Some(("chan2".into(), Some("key".into()))));
    assert_eq!(parse_channel("bad#"), None);
    assert_eq!(parse_channel("#"), None);
    assert_eq!(parse_user("@bob"), Some("bob".into()));
    assert_eq!(parse_user("%40alice"), Some("alice".into()));
    assert_eq!(parse_user("bad#"), None);

    assert_eq!(split_lines("a\nb\r\nc\rd\n\ne\u{2028}f\n"), vec!["a", "b", "c", "d", "", "e", "f"]);

    let nick = generate_nick("Apprise", 0);
    assert_eq!(nick.len(), 9);
    assert!(nick.starts_with("appris"));
    let nick = generate_nick("Apprise", 2);
    assert_eq!(nick.len(), 9);
    assert!(nick.ends_with('2'));
  }

  type Responder = Box<dyn FnMut(&str) -> Vec<String> + Send>;

  /// A responder that registers any nickname and confirms every JOIN.
  fn welcoming() -> Responder {
    let mut nick = String::new();
    Box::new(move |line: &str| {
      let msg = Message::parse(line);
      match msg.command.as_str() {
        "NICK" => {
          nick = msg.params[0].clone();
          vec![]
        }
        "USER" => vec![format!(":irc.test 001 {} :Welcome", nick), format!(":irc.test 376 {} :End of MOTD", nick)],
        "JOIN" => vec![format!(":{}!u@h JOIN :{}", nick, msg.params[0]), format!(":irc.test 366 {} {} :End of /NAMES list.", nick, msg.params[0])],
        "PING" => vec![format!("PONG :{}", msg.trailing.unwrap_or_default())],
        _ => vec![],
      }
    })
  }

  /// A scripted IRC server for one connection. Returns every line the
  /// client sent, up to its QUIT or disconnect.
  async fn irc_server(tls: bool, mut respond: Responder) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
      let (tcp, _) = listener.accept().await.unwrap();
      let stream: Box<dyn IrcStream> = if tls {
        match crate::utils::socket::test_tls_acceptor().accept(tcp).await {
          Ok(s) => Box::new(s),
          Err(_) => return vec![],
        }
      } else {
        Box::new(tcp)
      };
      let (reader, mut writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let mut lines = Vec::new();
      let mut line = String::new();
      while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        let received = line.trim_end_matches(['\r', '\n']).to_string();
        assert!(line.ends_with("\r\n"), "line without CRLF: {:?}", line);
        line.clear();
        for reply in respond(&received) {
          writer.write_all(format!("{}\r\n", reply).as_bytes()).await.unwrap();
        }
        let quit = received.starts_with("QUIT");
        lines.push(received);
        if quit {
          break;
        }
      }
      lines
    });
    (port, handle)
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_server_mode() {
    let (port, server) = irc_server(false, welcoming()).await;
    let svc = from_url(&format!("irc://me:pw@127.0.0.1:{}/#chan:key/@bob?name=Real%20Name", port)).unwrap();
    assert!(svc.send(&ctx("", "line one\nline two\n\nline three")).await.unwrap());
    assert_eq!(
      server.await.unwrap(),
      vec![
        "PASS pw",
        "NICK me",
        "USER me 0 * :Real Name",
        "JOIN #chan key",
        "PRIVMSG #chan :line one",
        "PRIVMSG #chan :line two",
        "PRIVMSG #chan :line three",
        "PRIVMSG bob :line one",
        "PRIVMSG bob :line two",
        "PRIVMSG bob :line three",
        "QUIT :Apprise Notifications",
      ]
    );
  }

  #[tokio::test]
  async fn test_send_nickserv_without_join() {
    let (port, server) = irc_server(false, welcoming()).await;
    let svc = from_url(&format!("irc://me:pw@127.0.0.1:{}/#chan?mode=nickserv&join=no", port)).unwrap();
    assert!(svc.send(&ctx("Title", "Body")).await.unwrap());
    assert_eq!(
      server.await.unwrap(),
      vec!["NICK me", "USER me 0 * :Apprise Notifications", "PRIVMSG NickServ :IDENTIFY pw", "PRIVMSG #chan :Title Body", "QUIT :Apprise Notifications"]
    );
  }

  #[tokio::test]
  async fn test_send_answers_ping_and_nick_collision() {
    let mut welcome = welcoming();
    let mut first_nick = true;
    let responder: Responder = Box::new(move |line: &str| {
      if line.starts_with("NICK") && first_nick {
        first_nick = false;
        return vec!["PING :tok123".into(), ":irc.test 433 * taken :Nickname is already in use".into()];
      }
      if line.starts_with("USER") {
        return vec![];
      }
      let mut out = welcome(line);
      if line.starts_with("NICK") {
        out.extend(welcome("USER x 0 * :x"));
      }
      out
    });
    let (port, server) = irc_server(false, responder).await;
    let svc = from_url(&format!("irc://127.0.0.1:{}/@bob", port)).unwrap();
    assert!(svc.send(&ctx("", "Body")).await.unwrap());

    let lines = server.await.unwrap();
    let first = lines[0].strip_prefix("NICK ").unwrap().to_string();
    assert_eq!(first.len(), 9);
    assert!(first.starts_with("appris"), "{}", first);
    assert_eq!(lines[1], format!("USER {} 0 * :Apprise Notifications", first));
    assert_eq!(lines[2], "PONG :tok123");
    let second = lines[3].strip_prefix("NICK ").unwrap();
    assert_eq!(second.len(), 9);
    assert!(second.ends_with('1'));
    assert_eq!(&lines[4..], &["PRIVMSG bob :Body", "QUIT :Apprise Notifications"]);
  }

  #[tokio::test]
  async fn test_send_register_and_join_errors() {
    let responder: Responder = Box::new(|line: &str| if line.starts_with("USER") { vec![":irc.test 464 * :Password incorrect".into()] } else { vec![] });
    let (port, server) = irc_server(false, responder).await;
    let svc = from_url(&format!("irc://me:bad@127.0.0.1:{}/#chan", port)).unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());
    assert_eq!(server.await.unwrap(), vec!["PASS bad", "NICK me", "USER me 0 * :Apprise Notifications"]);

    let mut welcome = welcoming();
    let responder: Responder =
      Box::new(move |line: &str| if line.starts_with("JOIN") { vec![":irc.test 474 me #chan :Cannot join channel (+b)".into()] } else { welcome(line) });
    let (port, server) = irc_server(false, responder).await;
    let svc = from_url(&format!("irc://me@127.0.0.1:{}/#chan", port)).unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());
    assert_eq!(server.await.unwrap().last().unwrap(), "JOIN #chan");
  }

  #[tokio::test]
  async fn test_send_znc_checks_connection() {
    let (port, server) = irc_server(false, welcoming()).await;
    let svc = from_url(&format!("irc://zncuser:zncpass@127.0.0.1:{}/#chan?mode=znc&join=no", port)).unwrap();
    assert!(svc.send(&ctx("", "Body")).await.unwrap());
    assert_eq!(
      server.await.unwrap(),
      vec![
        "PASS zncuser:zncpass",
        "NICK zncuser",
        "USER zncuser 0 * :Apprise Notifications",
        "PING :apprise",
        "PRIVMSG #chan :Body",
        "QUIT :Apprise Notifications"
      ]
    );
  }

  #[tokio::test]
  async fn test_send_tls_verify() {
    let (port, server) = irc_server(true, welcoming()).await;
    let svc = from_url(&format!("ircs://me@127.0.0.1:{}/@bob?verify=no", port)).unwrap();
    assert!(svc.send(&ctx("", "Body")).await.unwrap());
    assert_eq!(server.await.unwrap(), vec!["NICK me", "USER me 0 * :Apprise Notifications", "PRIVMSG bob :Body", "QUIT :Apprise Notifications"]);

    // The self-signed certificate is rejected when verifying
    let (port, server) = irc_server(true, welcoming()).await;
    let svc = from_url(&format!("ircs://me@127.0.0.1:{}/@bob", port)).unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());
    assert!(server.await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("irc://irc.example.com").unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());

    // Port 1 is privileged, so no test can be listening on it; a port
    // freed by dropping a listener may be taken by another test meanwhile.
    let port = 1;
    let svc = from_url(&format!("irc://127.0.0.1:{}/#chan", port)).unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());

    // The server hangs up during registration
    let responder: Responder = Box::new(|_: &str| vec![]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
      let (stream, _) = listener.accept().await.unwrap();
      drop(responder);
      drop(stream);
    });
    let svc = from_url(&format!("irc://127.0.0.1:{}/#chan", port)).unwrap();
    assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = Irc::static_details();
    assert_eq!(details.service_name, "IRC");
    assert_eq!(details.protocols, vec!["irc", "ircs"]);
    assert!(!details.attachment_support);
  }
}

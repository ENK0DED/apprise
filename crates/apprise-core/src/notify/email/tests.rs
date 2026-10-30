use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use hmac::{KeyInit, Mac};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::notify::Attachment;

tokio::task_local! {
  /// The TLS client the send under test uses instead of the real one.
  pub(super) static TEST_TLS: tokio_rustls::TlsConnector;
  /// Where the send under test connects, whatever its SMTP host, and the
  /// (host, port) pairs it asked for.
  static TEST_SMTP: (SocketAddr, Arc<Mutex<Vec<(String, u16)>>>);
}

/// The address an SMTP connection goes to (see [`TEST_SMTP`]).
pub(super) fn smtp_target(host: &str, port: u16) -> (String, u16) {
  TEST_SMTP
    .try_with(|(addr, requested)| {
      requested.lock().unwrap().push((host.to_string(), port));
      (addr.ip().to_string(), addr.port())
    })
    .unwrap_or_else(|_| (host.to_string(), port))
}

// ── A fake SMTP server ────────────────────────────────────────────────────────

/// One message the fake server accepted.
#[derive(Debug, Clone, Default)]
pub(crate) struct Mail {
  pub from: String,
  pub rcpt: Vec<String>,
  /// The DATA payload as sent (CRLF, dot-stuffed, without the final dot).
  pub raw: String,
  /// The message with dots unstuffed and `\n` line ends.
  pub text: String,
}

/// One SMTP session that said EHLO or HELO.
#[derive(Debug, Clone, Default)]
pub(crate) struct Session {
  pub implicit_tls: bool,
  pub starttls: bool,
  /// Every command line, AUTH answers included.
  pub commands: Vec<String>,
  pub auth_mechanism: Option<String>,
  pub login: Option<(String, String)>,
  pub mails: Vec<Mail>,
}

#[derive(Clone)]
pub(crate) struct ServerConfig {
  pub implicit_tls: bool,
  pub offer_starttls: bool,
  /// The AUTH mechanisms advertised ("" for none).
  pub auth: &'static str,
  /// The password a CRAM-MD5 answer is checked against.
  pub password: &'static str,
  pub reject_auth: bool,
  pub refuse_rcpt: Vec<String>,
  pub mail_code: u16,
  /// DATA fails (once) for messages to these recipients.
  pub fail_data_once: Vec<String>,
  pub acceptor: tokio_rustls::TlsAcceptor,
}

impl Default for ServerConfig {
  fn default() -> Self {
    Self {
      implicit_tls: false,
      offer_starttls: true,
      auth: "PLAIN LOGIN",
      password: "pass",
      reject_auth: false,
      refuse_rcpt: Vec::new(),
      mail_code: 250,
      fail_data_once: Vec::new(),
      acceptor: crate::utils::socket::test_tls_acceptor(),
    }
  }
}

pub(crate) struct FakeSmtp {
  pub addr: SocketAddr,
  sessions: Arc<Mutex<Vec<Session>>>,
}

impl FakeSmtp {
  pub async fn start(config: ServerConfig) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let failed_once = Arc::new(Mutex::new(Vec::<String>::new()));
    let store = sessions.clone();
    tokio::spawn(async move {
      while let Ok((tcp, _)) = listener.accept().await {
        let (config, store, failed_once) = (config.clone(), store.clone(), failed_once.clone());
        tokio::spawn(async move {
          let session = handle(tcp, &config, &failed_once).await;
          // Stray connections that never greet are not SMTP clients
          if session.commands.iter().any(|c| c.to_ascii_lowercase().starts_with("ehlo") || c.to_ascii_lowercase().starts_with("helo")) {
            store.lock().unwrap().push(session);
          }
        });
      }
    });
    Self { addr, sessions }
  }

  pub fn sessions(&self) -> Vec<Session> {
    self.sessions.lock().unwrap().clone()
  }

  pub fn mails(&self) -> Vec<Mail> {
    self.sessions().into_iter().flat_map(|s| s.mails).collect()
  }
}

async fn handle(tcp: TcpStream, config: &ServerConfig, failed_once: &Mutex<Vec<String>>) -> Session {
  let mut session = Session { implicit_tls: config.implicit_tls, ..Default::default() };
  if config.implicit_tls {
    if let Ok(tls) = config.acceptor.accept(tcp).await {
      converse(tls, config, &mut session, false, failed_once).await;
    }
  } else if let Some(tcp) = converse(tcp, config, &mut session, config.offer_starttls, failed_once).await {
    session.starttls = true;
    if let Ok(tls) = config.acceptor.accept(tcp).await {
      let mut reader = BufReader::new(tls);
      converse_after_tls(&mut reader, config, &mut session, failed_once).await;
    }
  }
  session
}

async fn converse<S: AsyncRead + AsyncWrite + Unpin>(
  stream: S,
  config: &ServerConfig,
  session: &mut Session,
  offer_starttls: bool,
  failed_once: &Mutex<Vec<String>>,
) -> Option<S> {
  let mut io = BufReader::new(stream);
  io.get_mut().write_all(b"220 fake.test ESMTP ready\r\n").await.ok()?;
  run_commands(&mut io, config, session, offer_starttls, failed_once).await.map(|Upgrade| io.into_inner())
}

async fn converse_after_tls<S: AsyncRead + AsyncWrite + Unpin>(
  io: &mut BufReader<S>,
  config: &ServerConfig,
  session: &mut Session,
  failed_once: &Mutex<Vec<String>>,
) {
  run_commands(io, config, session, false, failed_once).await;
}

struct Upgrade;

async fn read_line<S: AsyncRead + AsyncWrite + Unpin>(io: &mut BufReader<S>) -> Option<String> {
  let mut line = String::new();
  match io.read_line(&mut line).await {
    Ok(0) | Err(_) => None,
    Ok(_) => Some(line),
  }
}

/// Serve commands until QUIT or EOF; `Some(Upgrade)` after STARTTLS.
async fn run_commands<S: AsyncRead + AsyncWrite + Unpin>(
  io: &mut BufReader<S>,
  config: &ServerConfig,
  session: &mut Session,
  offer_starttls: bool,
  failed_once: &Mutex<Vec<String>>,
) -> Option<Upgrade> {
  let b64 = base64::engine::general_purpose::STANDARD;
  let mut mail = Mail::default();
  loop {
    let line = read_line(io).await?;
    let cmd = line.trim_end_matches(['\r', '\n']).to_string();
    session.commands.push(cmd.clone());
    let lower = cmd.to_ascii_lowercase();
    let reply: String = if lower.starts_with("ehlo") {
      let mut lines = vec!["fake.test greets you".to_string(), "SIZE 10240000".to_string()];
      if !config.auth.is_empty() {
        lines.push(format!("AUTH {}", config.auth));
      }
      if offer_starttls {
        lines.push("STARTTLS".to_string());
      }
      lines.push("8BITMIME".to_string());
      let last = lines.len() - 1;
      lines.iter().enumerate().map(|(i, l)| format!("250{}{}\r\n", if i == last { ' ' } else { '-' }, l)).collect()
    } else if lower.starts_with("helo") {
      "250 fake.test\r\n".to_string()
    } else if lower == "starttls" {
      if offer_starttls {
        io.get_mut().write_all(b"220 Ready to start TLS\r\n").await.ok()?;
        return Some(Upgrade);
      }
      "502 not offered\r\n".to_string()
    } else if let Some(rest) = lower.strip_prefix("auth ") {
      let mechanism = rest.split(' ').next().unwrap_or_default().to_ascii_uppercase();
      let arg = cmd.split(' ').nth(2).map(str::to_string);
      session.auth_mechanism = Some(mechanism.clone());
      let decode = |s: &str| String::from_utf8(b64.decode(s.trim()).unwrap_or_default()).unwrap_or_default();
      let login = match mechanism.as_str() {
        "PLAIN" => {
          let answer = match arg {
            Some(a) => a,
            None => {
              io.get_mut().write_all(b"334 \r\n").await.ok()?;
              read_line(io).await?.trim().to_string()
            }
          };
          let decoded = decode(&answer);
          let mut parts = decoded.split('\0').skip(1);
          Some((parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string()))
        }
        "LOGIN" => {
          let user = match arg {
            Some(a) => decode(&a),
            None => {
              io.get_mut().write_all(b"334 VXNlcm5hbWU6\r\n").await.ok()?;
              decode(&read_line(io).await?)
            }
          };
          io.get_mut().write_all(b"334 UGFzc3dvcmQ6\r\n").await.ok()?;
          let password = read_line(io).await?;
          session.commands.push(password.trim().to_string());
          Some((user, decode(&password)))
        }
        "CRAM-MD5" => {
          let challenge = "<1896.697170952@fake.test>";
          io.get_mut().write_all(format!("334 {}\r\n", b64.encode(challenge)).as_bytes()).await.ok()?;
          let answer = decode(&read_line(io).await?);
          let (user, digest) = answer.split_once(' ').unwrap_or_default();
          let mut mac = hmac::Hmac::<md5::Md5>::new_from_slice(config.password.as_bytes()).unwrap();
          mac.update(challenge.as_bytes());
          let expected = hex::encode(mac.finalize().into_bytes());
          Some((user.to_string(), if digest == expected { config.password.to_string() } else { format!("bad digest {}", digest) }))
        }
        _ => None,
      };
      match login {
        Some(login) if !config.reject_auth => {
          session.login = Some(login);
          "235 Authentication successful\r\n".to_string()
        }
        _ => "535 Authentication failed\r\n".to_string(),
      }
    } else if let Some(rest) = lower.strip_prefix("mail from:") {
      mail = Mail { from: rest.split('>').next().unwrap_or_default().trim_start_matches('<').to_string(), ..Default::default() };
      format!("{} sender\r\n", config.mail_code)
    } else if let Some(rest) = lower.strip_prefix("rcpt to:") {
      let rcpt = cmd[8..].trim().trim_start_matches('<').split('>').next().unwrap_or_default().to_string();
      let _ = rest;
      if config.refuse_rcpt.contains(&rcpt) {
        "550 no such user\r\n".to_string()
      } else {
        mail.rcpt.push(rcpt);
        "250 recipient ok\r\n".to_string()
      }
    } else if lower == "data" {
      io.get_mut().write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n").await.ok()?;
      let mut raw = String::new();
      loop {
        let line = read_line(io).await?;
        if line == ".\r\n" {
          break;
        }
        raw.push_str(&line);
      }
      mail.text = raw.split("\r\n").map(|l| l.strip_prefix('.').unwrap_or(l)).collect::<Vec<_>>().join("\n");
      mail.raw = raw;
      let fail = {
        let mut failed = failed_once.lock().unwrap();
        let target = mail.rcpt.first().cloned().unwrap_or_default();
        if config.fail_data_once.contains(&target) && !failed.contains(&target) {
          failed.push(target);
          true
        } else {
          false
        }
      };
      if fail {
        "451 try again later\r\n".to_string()
      } else {
        session.mails.push(std::mem::take(&mut mail));
        "250 queued\r\n".to_string()
      }
    } else if lower == "rset" {
      mail = Mail::default();
      "250 reset\r\n".to_string()
    } else if lower == "quit" {
      let _ = io.get_mut().write_all(b"221 bye\r\n").await;
      return None;
    } else {
      "500 unrecognised command\r\n".to_string()
    };
    io.get_mut().write_all(reply.as_bytes()).await.ok()?;
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// A TLS client that accepts the fake server's certificate.
fn accept_any_tls() -> tokio_rustls::TlsConnector {
  crate::utils::socket::tls_connector(false)
}

/// Run `fut` with SMTP connections going to `server` (TLS unverified).
pub(crate) async fn against<F: std::future::Future>(server: &FakeSmtp, fut: F) -> (F::Output, Vec<(String, u16)>) {
  let requested = Arc::new(Mutex::new(Vec::new()));
  let out = TEST_SMTP.scope((server.addr, requested.clone()), TEST_TLS.scope(accept_any_tls(), fut)).await;
  let requested = requested.lock().unwrap().clone();
  (out, requested)
}

/// Like [`against`], with the production TLS client.
async fn against_verified<F: std::future::Future>(server: &FakeSmtp, fut: F) -> F::Output {
  TEST_SMTP.scope((server.addr, Arc::new(Mutex::new(Vec::new()))), fut).await
}

fn email(url: &str) -> Email {
  let parsed = ParsedUrl::parse(url).unwrap_or_else(|| panic!("unparsable: {}", url));
  Email::from_url(&parsed).unwrap_or_else(|| panic!("rejected: {}", url))
}

pub(crate) fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
  NotifyContext { title: title.to_string(), body: body.to_string(), body_format: format, ..Default::default() }
}

pub(crate) async fn notify_url(url: &str, ctx: &NotifyContext) -> bool {
  let mut apprise = crate::Apprise::new();
  assert!(apprise.add(url), "{}", url);
  apprise.notify(ctx).await.is_success()
}

/// Boundaries, Message-IDs and dates replaced, Cc/Reply-To entries sorted
/// (upstream keeps them in a set, so their order is arbitrary).
fn normalise(msg: &str) -> String {
  use regex_lite::Regex;
  let boundary = Regex::new(r"===============\d{19}==(?:\.\d+)?").unwrap();
  let mut seen: Vec<String> = Vec::new();
  let msg = boundary.replace_all(msg, |c: &regex_lite::Captures| {
    let b = c[0].to_string();
    let i = seen.iter().position(|s| *s == b).unwrap_or_else(|| {
      seen.push(b);
      seen.len() - 1
    });
    format!("BOUNDARY{}", i + 1)
  });
  let msg = Regex::new(r"<\d+\.\d+\.\d+@").unwrap().replace_all(&msg, "<MSGID@");
  let msg = Regex::new(r"(?m)^Date: .*$").unwrap().replace_all(&msg, "Date: DATE");
  msg
    .split('\n')
    .map(|line| match line.split_once(": ") {
      Some((name, value)) if name == "Cc" || name == "Reply-To" => {
        let mut entries: Vec<&str> = value.split(',').collect();
        entries.sort();
        format!("{}: {}", name, entries.join(","))
      }
      _ => line.to_string(),
    })
    .collect::<Vec<_>>()
    .join("\n")
}

// ── URL parsing against upstream ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ParseCase {
  url: String,
  ok: bool,
  #[serde(default)]
  user: Option<String>,
  #[serde(default)]
  password: Option<String>,
  #[serde(default)]
  host: Option<String>,
  #[serde(default)]
  smtp_host: Option<String>,
  #[serde(default)]
  port: Option<u16>,
  #[serde(default)]
  secure_mode: Option<String>,
  #[serde(default)]
  secure: Option<bool>,
  #[serde(default)]
  from_name: Option<String>,
  #[serde(default)]
  from_addr: Option<String>,
  #[serde(default)]
  targets: Vec<(Option<String>, String)>,
  #[serde(default)]
  cc: Vec<String>,
  #[serde(default)]
  bcc: Vec<String>,
  #[serde(default)]
  reply_to: Vec<String>,
  #[serde(default)]
  names: std::collections::BTreeMap<String, Option<String>>,
  #[serde(default)]
  headers: std::collections::BTreeMap<String, String>,
  #[serde(default)]
  pgp_mode: Option<String>,
  #[serde(default)]
  inline: Option<bool>,
}

fn mode_str(mode: SecureMode) -> &'static str {
  match mode {
    SecureMode::Insecure => "insecure",
    SecureMode::Ssl => "ssl",
    SecureMode::StartTls => "starttls",
  }
}

fn name_str(name: &FromName) -> Option<String> {
  match name {
    FromName::AppId => Some("Apprise".to_string()),
    FromName::Name(n) => Some(n.clone()),
    FromName::None => None,
  }
}

/// Upstream's own parse results for every URL in its email and Delta Chat
/// tests (and more), dumped from Apprise 2.1.0 (`tz=` cases are left out:
/// the core does not know IANA zone names).
#[test]
fn test_parse_matches_upstream() {
  let cases: Vec<ParseCase> = serde_json::from_str(include_str!("testdata/parse_state.json")).unwrap();
  let mut failures = Vec::new();
  for case in &cases {
    let parsed = ParsedUrl::parse(&case.url);
    let secure = parsed.as_ref().is_some_and(|p| p.schema == "mailtos" || p.schema == "deltachats");
    let result = parsed.as_ref().and_then(|p| Email::parse(p, secure));
    let Some(e) = result else {
      if case.ok {
        failures.push(format!("{}: rejected, upstream accepts", case.url));
      }
      continue;
    };
    if !case.ok {
      failures.push(format!("{}: accepted, upstream rejects", case.url));
      continue;
    }
    let mut cc = e.cc.clone();
    cc.sort();
    let mut bcc = e.bcc.clone();
    bcc.sort();
    let mut reply = e.reply_to.clone();
    reply.sort();
    let names: std::collections::BTreeMap<String, Option<String>> = e.names.iter().map(|(k, v)| (k.clone(), name_str(v))).collect();
    let headers: std::collections::BTreeMap<String, String> = e.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let got = (
      (
        e.user.clone(),
        e.password.clone(),
        e.host.clone(),
        e.smtp_host.clone(),
        e.port,
        mode_str(e.secure_mode).to_string(),
        e.secure,
        name_str(&e.from_name),
        e.from_addr.clone(),
      ),
      e.targets.clone(),
      (cc, bcc, reply),
      names,
      headers,
      e.pgp_mode.as_str().to_string(),
      e.inline,
    );
    let empty_none = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    let want = (
      (
        empty_none(&case.user),
        empty_none(&case.password),
        case.host.clone().unwrap_or_default(),
        case.smtp_host.clone().unwrap_or_default(),
        case.port.unwrap_or_default(),
        case.secure_mode.clone().unwrap_or_default(),
        case.secure.unwrap_or_default(),
        case.from_name.clone(),
        case.from_addr.clone().unwrap_or_default(),
      ),
      case.targets.clone(),
      (case.cc.clone(), case.bcc.clone(), case.reply_to.clone()),
      case.names.clone(),
      case.headers.clone(),
      case.pgp_mode.clone().unwrap_or_default(),
      case.inline.unwrap_or_default(),
    );
    if got != want {
      failures.push(format!("{}:\n  got  {:?}\n  want {:?}", case.url, got, want));
    }
  }
  assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

// ── Messages against upstream ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct GoldenAttachment {
  name: String,
  mime: String,
  data: String,
}

#[derive(Deserialize)]
struct GoldenMail {
  from: String,
  to: Vec<String>,
  msg: String,
}

#[derive(Deserialize)]
struct GoldenSession {
  host: String,
  port: u16,
  ssl: bool,
  starttls: bool,
  login: Option<(String, String)>,
  mails: Vec<GoldenMail>,
}

#[derive(Deserialize)]
struct Golden {
  name: String,
  url: String,
  title: String,
  body: String,
  #[serde(default)]
  format: Option<String>,
  #[serde(default)]
  attachments: Vec<GoldenAttachment>,
  success: bool,
  sessions: Vec<GoldenSession>,
}

/// What upstream sends (captured from Apprise 2.1.0 with `smtplib`
/// replaced by a recorder) against what this port puts on the wire:
/// connection security, login, envelope and the complete MIME message.
#[tokio::test]
async fn test_messages_match_upstream() {
  let goldens: Vec<Golden> = serde_json::from_str(include_str!("testdata/golden_mime.json")).unwrap();
  let mut failures = Vec::new();
  for golden in &goldens {
    let expected = &golden.sessions[0];
    let server = FakeSmtp::start(ServerConfig { implicit_tls: expected.ssl, ..Default::default() }).await;
    let format: NotifyFormat = golden.format.as_deref().unwrap_or("text").parse().unwrap();
    let mut context = ctx(&golden.title, &golden.body, format);
    context.attachments = golden
      .attachments
      .iter()
      .map(|a| Attachment {
        // Upstream's in-memory attachments get a random name when given none
        name: if a.name.is_empty() { "file001.dat".to_string() } else { a.name.clone() },
        data: base64::engine::general_purpose::STANDARD.decode(&a.data).unwrap(),
        mime_type: a.mime.clone(),
      })
      .collect();
    let (ok, requested) = against(&server, notify_url(&golden.url, &context)).await;
    let name = &golden.name;
    if ok != golden.success {
      failures.push(format!("{}: success {} != {}", name, ok, golden.success));
      continue;
    }
    if requested.first() != Some(&(expected.host.clone(), expected.port)) {
      failures.push(format!("{}: connected to {:?}, upstream {}:{}", name, requested, expected.host, expected.port));
    }
    let sessions = server.sessions();
    let Some(session) = sessions.first() else {
      failures.push(format!("{}: no SMTP session", name));
      continue;
    };
    if (session.implicit_tls, session.starttls) != (expected.ssl, expected.starttls) {
      failures.push(format!("{}: tls {:?} != {:?}", name, (session.implicit_tls, session.starttls), (expected.ssl, expected.starttls)));
    }
    if session.login != expected.login {
      failures.push(format!("{}: login {:?} != {:?}", name, session.login, expected.login));
    }
    if session.mails.len() != expected.mails.len() {
      failures.push(format!("{}: {} messages, upstream {}", name, session.mails.len(), expected.mails.len()));
      continue;
    }
    for (got, want) in session.mails.iter().zip(&expected.mails) {
      let (mut got_to, mut want_to) = (got.rcpt.clone(), want.to.clone());
      got_to.sort();
      want_to.sort();
      if got.from != want.from || got_to != want_to {
        failures.push(format!("{}: envelope {} {:?} != {} {:?}", name, got.from, got_to, want.from, want_to));
      }
      let mut want_msg =
        want.msg.replace(regex_lite::Regex::new(r#"[0-9a-f-]{36}\.dat"#).unwrap().find(&want.msg).map(|m| m.as_str()).unwrap_or("\u{0}"), "file001.dat");
      if !want_msg.ends_with('\n') {
        want_msg.push('\n');
      }
      let (got_msg, want_msg) = (normalise(&got.text), normalise(&want_msg));
      if got_msg != want_msg {
        failures.push(format!("{}: message differs\n--- port ---\n{}\n--- upstream ---\n{}", name, got_msg, want_msg));
      }
    }
  }
  assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ── The SMTP exchange ─────────────────────────────────────────────────────────

fn verbs(session: &Session) -> Vec<String> {
  session.commands.iter().map(|c| c.split([' ', ':']).next().unwrap_or_default().to_ascii_lowercase()).collect()
}

/// Upstream test_plugin_email_smtplib_send_multiple_recipients: one session
/// carries every message; Cc and Bcc ride along with each one.
#[tokio::test]
async fn test_one_session_for_all_recipients() {
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let url = "mailto://user:pass@mail.example.org?to=foo@example.net,bar@example.com&cc=baz@example.org&bcc=qux@example.org";
  let (ok, requested) = against(&server, email(url).send(&ctx("test", "body", NotifyFormat::Html))).await;
  assert!(ok.unwrap());
  assert_eq!(requested, vec![("mail.example.org".to_string(), 25)]);
  let sessions = server.sessions();
  assert_eq!(sessions.len(), 1);
  let s = &sessions[0];
  assert!(!s.starttls && !s.implicit_tls);
  assert_eq!(s.login, Some(("user".to_string(), "pass".to_string())));
  assert_eq!(s.auth_mechanism.as_deref(), Some("PLAIN"));
  assert_eq!(verbs(s), ["ehlo", "auth", "mail", "rcpt", "rcpt", "rcpt", "data", "mail", "rcpt", "rcpt", "rcpt", "data", "quit"]);
  assert!(s.commands[0].starts_with("ehlo "));
  assert!(s.commands[2].starts_with("mail from:<user@mail.example.org> size="));
  assert_eq!(s.commands[3..6], ["rcpt to:<foo@example.net>", "rcpt to:<baz@example.org>", "rcpt to:<qux@example.org>"]);
  assert_eq!(s.mails[0].rcpt, ["foo@example.net", "baz@example.org", "qux@example.org"]);
  assert_eq!(s.mails[1].rcpt, ["bar@example.com", "baz@example.org", "qux@example.org"]);
  // Bcc never appears in the message
  assert!(s.mails.iter().all(|m| !m.text.contains("qux@example.org") && m.text.contains("\nCc: baz@example.org\n")));
  // SIZE is the CRLF message before dot-stuffing
  let size: usize = s.commands[2].rsplit('=').next().unwrap().parse().unwrap();
  assert_eq!(size, s.mails[0].raw.len());
}

#[tokio::test]
async fn test_starttls_and_auth_mechanisms() {
  // STARTTLS, then EHLO again and AUTH LOGIN when only LOGIN is offered
  let server = FakeSmtp::start(ServerConfig { auth: "LOGIN", ..Default::default() }).await;
  let (ok, _) = against(&server, email("mailtos://user:secret@example.com/to@example.org").send(&ctx("t", "b", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  let s = &server.sessions()[0];
  assert!(s.starttls);
  assert_eq!(verbs(s), ["ehlo", "starttls", "ehlo", "auth", "c2vjcmv0", "mail", "rcpt", "data", "quit"]);
  assert_eq!(s.commands[3], "AUTH LOGIN dXNlcg==");
  assert_eq!(s.login, Some(("user".to_string(), "secret".to_string())));

  // CRAM-MD5 is preferred when offered (smtplib's order)
  let server = FakeSmtp::start(ServerConfig { auth: "LOGIN PLAIN CRAM-MD5", password: "secret", ..Default::default() }).await;
  let (ok, _) = against(&server, email("mailtos://user:secret@example.com/to@example.org").send(&ctx("t", "b", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  let s = &server.sessions()[0];
  assert_eq!(s.auth_mechanism.as_deref(), Some("CRAM-MD5"));
  assert_eq!(s.login, Some(("user".to_string(), "secret".to_string())));

  // Implicit TLS for ssl mode: no STARTTLS
  let server = FakeSmtp::start(ServerConfig { implicit_tls: true, ..Default::default() }).await;
  let (ok, requested) = against(&server, email("mailto://user:pass@example.com/to@example.org?mode=ssl").send(&ctx("t", "b", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  assert_eq!(requested, vec![("example.com".to_string(), 465)]);
  let s = &server.sessions()[0];
  assert!(s.implicit_tls && !s.starttls);
  assert_eq!(verbs(s), ["ehlo", "auth", "mail", "rcpt", "data", "quit"]);
}

/// Upstream test_smtp_controller_skips_partial_login: no AUTH without both
/// a user and a password.
#[tokio::test]
async fn test_login_needs_user_and_password() {
  for url in
    ["mailto://user@example.com/to@example.org", "mailto://user:@example.com/to@example.org", "mailto://example.com?from=me@example.com&to=to@example.org"]
  {
    let server = FakeSmtp::start(ServerConfig::default()).await;
    let (ok, _) = against(&server, email(url).send(&ctx("t", "b", NotifyFormat::Text))).await;
    assert!(ok.unwrap(), "{}", url);
    let s = &server.sessions()[0];
    assert!(s.auth_mechanism.is_none(), "{}", url);
    assert_eq!(verbs(s), ["ehlo", "mail", "rcpt", "data", "quit"], "{}", url);
  }
}

#[tokio::test]
async fn test_connection_and_login_failures() {
  let body = ctx("t", "b", NotifyFormat::Text);
  // Rejected credentials: nothing is sent and the session is closed
  let server = FakeSmtp::start(ServerConfig { reject_auth: true, ..Default::default() }).await;
  let (ok, _) = against(&server, email("mailto://user:pass@example.com/to@example.org").send(&body)).await;
  assert!(!ok.unwrap());
  let s = &server.sessions()[0];
  assert!(s.mails.is_empty());
  assert_eq!(verbs(s).last().map(String::as_str), Some("quit"));

  // No AUTH offered
  let server = FakeSmtp::start(ServerConfig { auth: "", ..Default::default() }).await;
  let (ok, _) = against(&server, email("mailto://user:pass@example.com/to@example.org").send(&body)).await;
  assert!(!ok.unwrap());

  // STARTTLS required but not offered
  let server = FakeSmtp::start(ServerConfig { offer_starttls: false, ..Default::default() }).await;
  let (ok, _) = against(&server, email("mailtos://user:pass@example.com/to@example.org").send(&body)).await;
  assert!(!ok.unwrap());
  assert!(server.mails().is_empty());

  // Nothing listening
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  let e = email(&format!("mailto://user:pass@localhost:{}/to@example.org?cto=2", port));
  assert!(!e.send(&body).await.unwrap());
}

/// Upstream test_smtp_controller_sendmail_partial_rejection and the
/// SMTPRecipientsRefused/SMTPSenderRefused cases: a refusal fails the
/// notification, and the session goes on with the next message.
#[tokio::test]
async fn test_refusals() {
  let body = ctx("t", "b", NotifyFormat::Text);
  let url = "mailto://user:pass@example.com/a@example.org/b@example.org?cc=cc@example.org";

  // One refused Cc: the message still reaches the others
  let server = FakeSmtp::start(ServerConfig { refuse_rcpt: vec!["cc@example.org".to_string()], ..Default::default() }).await;
  let (ok, _) = against(&server, email(url).send(&body)).await;
  assert!(!ok.unwrap());
  let mails = server.mails();
  assert_eq!(mails.len(), 2);
  assert_eq!(mails[0].rcpt, ["a@example.org"]);

  // Every recipient refused: RSET, no DATA, the next message is tried
  let server = FakeSmtp::start(ServerConfig { refuse_rcpt: vec!["a@example.org".to_string(), "cc@example.org".to_string()], ..Default::default() }).await;
  let (ok, _) = against(&server, email(url).send(&body)).await;
  assert!(!ok.unwrap());
  let s = &server.sessions()[0];
  assert_eq!(verbs(s), ["ehlo", "auth", "mail", "rcpt", "rcpt", "rset", "mail", "rcpt", "rcpt", "data", "quit"]);
  assert_eq!(s.mails.len(), 1);
  assert_eq!(s.mails[0].rcpt, ["b@example.org"]);

  // The sender refused
  let server = FakeSmtp::start(ServerConfig { mail_code: 553, ..Default::default() }).await;
  let (ok, _) = against(&server, email(url).send(&body)).await;
  assert!(!ok.unwrap());
  assert!(server.mails().is_empty());
}

/// Upstream test_plugin_email_retry_skips_delivered_recipients: a retry
/// sends only to the recipients that did not accept the message.
#[tokio::test]
async fn test_retry_skips_delivered_recipients() {
  let server = FakeSmtp::start(ServerConfig { fail_data_once: vec!["b@example.org".to_string()], ..Default::default() }).await;
  let url = "mailto://user:pass@example.com/a@example.org/b@example.org?retry=1&wait=0";
  let (ok, _) = against(&server, notify_url(url, &ctx("t", "b", NotifyFormat::Text))).await;
  assert!(ok);
  let to: Vec<String> = server.mails().iter().map(|m| m.rcpt[0].clone()).collect();
  assert_eq!(to, ["a@example.org", "b@example.org"]);
  assert_eq!(server.sessions().len(), 2);
}

/// Upstream test_plugin_email_reply_to_multiple_recipients (82b2589d):
/// Reply-To stays on every message.
#[tokio::test]
async fn test_reply_to_on_every_recipient() {
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let url = "mailto://user:pass@example.com/a@example.org/b@example.org/c@example.org?reply=Help%20Desk%20%3Chelp@example.com%3E";
  let (ok, _) = against(&server, email(url).send(&ctx("t", "b", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  let mails = server.mails();
  assert_eq!(mails.len(), 3);
  assert!(mails.iter().all(|m| m.text.contains("\nReply-To: Help Desk <help@example.com>\n")), "{:?}", mails);
}

#[tokio::test]
async fn test_no_recipients() {
  // to=@ leaves nothing valid to send to
  let e = email("mailtos://user:pass@nuxref.com?to=@");
  assert!(e.targets.is_empty());
  assert!(!e.send(&ctx("t", "b", NotifyFormat::Text)).await.unwrap());
}

#[tokio::test]
async fn test_attachment_fallback_name_and_type() {
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let mut context = ctx("t", "b", NotifyFormat::Text);
  context.attachments = vec![Attachment { name: String::new(), data: b"x".to_vec(), mime_type: String::new() }];
  let (ok, _) = against(&server, email("mailto://user:pass@example.com/to@example.org").send(&context)).await;
  assert!(ok.unwrap());
  let text = &server.mails()[0].text;
  assert!(text.contains("Content-Type: application/octet-stream\nContent-Disposition: attachment; filename=\"file001.dat\"\n"), "{}", text);
}

#[tokio::test]
async fn test_non_ascii_address_fails() {
  // Python's formataddr() refuses non-ASCII addresses
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let (ok, _) = against(&server, email("mailto://user:pass@example.com/%C4%B0user@example.org").send(&ctx("t", "b", NotifyFormat::Text))).await;
  assert!(!ok.unwrap());
  assert!(server.mails().is_empty());
}

// ── TLS certificate verification (upstream 6c9b054b) ─────────────────────────

/// A test CA and a `localhost`/`127.0.0.1` certificate it signed.
const TEST_CA_DER: &str = "MIIBmzCCAUGgAwIBAgIUQ5gCtXySxDGi61Kcc6KW4tyaMWkwCgYIKoZIzj0EAwIwGjEYMBYGA1UEAwwPQXBwcmlzZSBUZXN0IENBMCAXDTI2MTAzMDExMjY1NloYDzIxMjYxMDA2MTEyNjU2WjAaMRgwFgYDVQQDDA9BcHByaXNlIFRlc3QgQ0EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQm5hf0uByGqALjbQAc5Hq40uyHl69ywSel210rRYunnLGn0SBCNvSORVYb8E9nsHYo380TI0E4X91D1KY9O7Pro2MwYTAdBgNVHQ4EFgQUi4Ab7oSnMCm/V+LX4MbKWcxouM0wHwYDVR0jBBgwFoAUi4Ab7oSnMCm/V+LX4MbKWcxouM0wDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZIzj0EAwIDSAAwRQIhAOcqQQlac5TOoh9ESboIw+FO3sbDJD21pr0gH+RUgKsSAiB1Pv38DpMPW/MN4YU0dqSvQ5nDpWtl/TzjkeHTQtPnlg==";
const TEST_LEAF_DER: &str = "MIIBxjCCAWugAwIBAgIUDNQf6wuQGqDZccJd6E4HgWbQ7WswCgYIKoZIzj0EAwIwGjEYMBYGA1UEAwwPQXBwcmlzZSBUZXN0IENBMCAXDTI2MTAzMDExMjY1NloYDzIxMjYxMDA2MTEyNjU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQbiQtKPAqR1S4QOddLj5fd7/vkwEOqvj9w1ArGyFA1pTcdkxxTceJOkDt8YNm9CrcKYec5JPkhf+UXaWaEzSl/o4GSMIGPMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATAdBgNVHQ4EFgQUkGFXVr08FR/+tENJ61EQpRNyP/wwHwYDVR0jBBgwFoAUi4Ab7oSnMCm/V+LX4MbKWcxouM0wCgYIKoZIzj0EAwIDSQAwRgIhAP98xHVAcV9wr4Tuv7EpP02VQHZiud+BJreQxffOSpzCAiEAiU4HVlufNVrOwjEiO7OZKnqlggElDlV7cO0cImcqZmI=";
const TEST_LEAF_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg2eCBGjfPBHPm7zPJcrlullIDOvgcq88M41X6UQU7/XWhRANCAAQbiQtKPAqR1S4QOddLj5fd7/vkwEOqvj9w1ArGyFA1pTcdkxxTceJOkDt8YNm9CrcKYec5JPkhf+UXaWaEzSl/";

fn der(s: &str) -> Vec<u8> {
  base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn ca_signed_acceptor() -> tokio_rustls::TlsAcceptor {
  crate::ensure_crypto_provider();
  let cert = rustls::pki_types::CertificateDer::from(der(TEST_LEAF_DER));
  let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(der(TEST_LEAF_KEY)));
  let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert], key).unwrap();
  tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// A verifying client that trusts the test CA only.
fn test_ca_client() -> tokio_rustls::TlsConnector {
  crate::ensure_crypto_provider();
  let mut roots = rustls::RootCertStore::empty();
  roots.add(rustls::pki_types::CertificateDer::from(der(TEST_CA_DER))).unwrap();
  let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
  tokio_rustls::TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn test_tls_certificates_are_verified() {
  let body = ctx("t", "b", NotifyFormat::Text);
  for implicit_tls in [false, true] {
    let mode = if implicit_tls { "ssl" } else { "starttls" };
    // A self-signed certificate fails verification...
    let server = FakeSmtp::start(ServerConfig { implicit_tls, ..Default::default() }).await;
    let url = format!("mailtos://user:pass@localhost/to@example.org?mode={}", mode);
    assert!(!against_verified(&server, email(&url).send(&body)).await.unwrap(), "{}", mode);
    assert!(server.mails().is_empty());
    // ...unless verify=no
    let url = format!("mailtos://user:pass@localhost/to@example.org?mode={}&verify=no", mode);
    assert!(against_verified(&server, email(&url).send(&body)).await.unwrap(), "{}", mode);
    assert_eq!(server.mails().len(), 1);

    // A trusted certificate passes, but only for the name it was issued to
    let server = FakeSmtp::start(ServerConfig { implicit_tls, acceptor: ca_signed_acceptor(), ..Default::default() }).await;
    let url = format!("mailtos://user:pass@localhost/to@example.org?mode={}", mode);
    let ok = TEST_TLS.scope(test_ca_client(), against_verified(&server, email(&url).send(&body))).await;
    assert!(ok.unwrap(), "{}", mode);
    let url = format!("mailtos://user:pass@example.com/to@example.org?mode={}&smtp=wrong.example", mode);
    let ok = TEST_TLS.scope(test_ca_client(), against_verified(&server, email(&url).send(&body))).await;
    assert!(!ok.unwrap(), "{}", mode);
    assert_eq!(server.mails().len(), 1);
  }
}

// ── Format, inline and options ────────────────────────────────────────────────

#[test]
fn test_service_details() {
  let e = email("mailto://user:pass@gmail.com");
  assert_eq!(e.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Text]);
  assert_eq!(e.notify_format(), NotifyFormat::Html);
  assert!(e.attachment_support());
  assert_eq!(e.schemas(), &["mailto", "mailtos"]);
  assert_eq!(Email::static_details().service_name, "E-Mail");
  assert_eq!(e.url_identifier().unwrap(), ["mailtos", "user@gmail.com", "pass", "gmail.com", "smtp.gmail.com", "587"]);
  assert_eq!(email("mailto://user:pass@example.com").url_identifier().unwrap(), ["mailto", "user", "pass", "example.com", "example.com", "25"]);
}

#[test]
fn test_pgp_mode_prefixes() {
  for (value, mode) in [
    ("n", PgpMode::None),
    ("no", PgpMode::None),
    ("none", PgpMode::None),
    ("NONE", PgpMode::None),
    ("s", PgpMode::Sign),
    ("sig", PgpMode::Sign),
    ("E", PgpMode::Encrypt),
    ("encrypt", PgpMode::Encrypt),
  ] {
    assert_eq!(PgpMode::parse(value), Some(mode), "{}", value);
  }
  for value in ["yes", "true", "encrpyt", "signs", "x"] {
    assert_eq!(PgpMode::parse(value), None, "{}", value);
  }
  // wkd=yes implies encrypt unless pgp= says otherwise
  assert_eq!(email("mailto://user:pass@example.com?wkd=yes").pgp_mode, PgpMode::Encrypt);
  assert!(email("mailto://user:pass@example.com?wkd=yes").pgp.wkd().is_some());
  assert_eq!(email("mailto://user:pass@example.com?wkd=yes&pgp=no").pgp_mode, PgpMode::None);
  assert_eq!(email("mailto://user:pass@example.com?wkd=yes&pgp=sign").pgp_mode, PgpMode::Sign);
  assert!(email("mailto://user:pass@example.com?wkd=no").pgp.wkd().is_none());
}

#[test]
fn test_timeout_follows_cto() {
  assert_eq!(email("mailto://user:pass@example.com").timeout, Duration::from_secs(15));
  assert_eq!(email("mailto://user:pass@example.com?cto=2.5").timeout, Duration::from_secs_f64(2.5));
}

// ── PGP/MIME ──────────────────────────────────────────────────────────────────

use pgp::composed::{Deserializable, DetachedSignature, Message, SignedSecretKey};

pub(crate) const VALID_PRV: &str = include_str!("../../utils/pgp/testdata/valid-prv.asc");
/// A key pair made by upstream's own `ApprisePGPController.keygen()`
/// (PGPy): RSA signing primary key and RSA encryption subkey.
pub(crate) const PGPY_PRV: &str = include_str!("../../utils/pgp/testdata/pgpy-prv.asc");
pub(crate) const PGPY_PUB: &str = include_str!("../../utils/pgp/testdata/pgpy-pub.asc");

fn write_key(dir: &tempfile::TempDir, name: &str, data: &str) -> String {
  let path = dir.path().join(name);
  std::fs::write(&path, data).unwrap();
  path.to_string_lossy().to_string()
}

/// The part of `text` between the first boundary pair of the top-level
/// multipart, and the parts after it.
fn top_parts(text: &str) -> Vec<String> {
  let ctype = text.lines().find(|l| l.starts_with("Content-Type: multipart/")).unwrap();
  let boundary = ctype.split("boundary=\"").nth(1).unwrap().trim_end_matches('"');
  let body = text.split_once("\n\n").unwrap().1;
  let mut parts: Vec<String> = body.split(&format!("--{}", boundary)).map(str::to_string).collect();
  parts.remove(0);
  parts.pop();
  // Each part starts after its delimiter's line break and ends before the
  // line break that precedes the next delimiter
  parts.into_iter().map(|p| p.strip_prefix('\n').unwrap_or(&p).strip_suffix('\n').unwrap_or(&p).to_string()).collect()
}

fn decrypt(armored_part: &str, key: &SignedSecretKey) -> String {
  let armored = armored_part.split_once("\n\n").unwrap().1;
  let (message, _) = Message::from_string(armored).unwrap();
  let decrypted = message.decrypt(&pgp::types::Password::empty(), key).unwrap();
  let mut decompressed = if decrypted.is_compressed() { decrypted.decompress().unwrap() } else { decrypted };
  String::from_utf8(decompressed.as_data_vec().unwrap()).unwrap()
}

/// pgp=sign: RFC 3156 multipart/signed whose detached signature verifies
/// over the CRLF form of the signed part (upstream
/// test_plugin_email_pgp_sign_crlf_roundtrip / _wire_content_crlf).
#[tokio::test]
async fn test_pgp_sign() {
  let dir = tempfile::tempdir().unwrap();
  let prv = write_key(&dir, "prv.asc", VALID_PRV);
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let url = format!("mailto://user:pass@nuxref.com?pgp=sign&pgpprv={}&+Autocrypt=addr%3Dattacker%40example.com", prv);
  let (ok, _) = against(&server, email(&url).send(&ctx("signed", "test body", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  let mail = &server.mails()[0];
  let text = &mail.text;
  assert!(text.starts_with("Content-Type: multipart/signed; micalg=\"pgp-sha256\"; protocol=\"application/pgp-signature\"; boundary=\""), "{}", text);
  // No recipient key: signed only; the fixture key is not Autocrypt-shaped,
  // and the URL's own Autocrypt header is kept (the first one)
  assert!(!text.contains("multipart/encrypted"));
  assert_eq!(text.to_lowercase().matches("autocrypt:").count(), 1);

  let parts = top_parts(text);
  assert_eq!(parts.len(), 2);
  assert!(parts[1].starts_with(
    "Content-Type: application/pgp-signature\nMIME-Version: 1.0\nContent-Disposition: attachment; filename=\"signature.asc\"\n\n-----BEGIN PGP SIGNATURE-----"
  ));
  let (signature, _) = DetachedSignature::from_string(parts[1].split_once("\n\n").unwrap().1).unwrap();
  let (secret, _) = SignedSecretKey::from_string(VALID_PRV).unwrap();
  let public = secret.to_public_key();
  let crlf = parts[0].replace('\n', "\r\n");
  signature.verify(&public, crlf.as_bytes()).expect("signature over the CRLF part");
  assert!(signature.verify(&public, parts[0].as_bytes()).is_err());
  // What the server received is exactly what was signed
  assert!(mail.raw.contains(&crlf));
}

/// pgp=encrypt with pgppub=: multipart/encrypted whose ciphertext opens with
/// the recipient's private key and holds From/To/Subject and the body.
#[tokio::test]
async fn test_pgp_encrypt() {
  let dir = tempfile::tempdir().unwrap();
  let public = write_key(&dir, "pub.asc", PGPY_PUB);
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let url = format!("mailto://user:pass@nuxref.com/recipient@example.com?pgp=encrypt&pgppub={}", public);
  let (ok, _) = against(&server, email(&url).send(&ctx("secret subject", "secret body", NotifyFormat::Text))).await;
  assert!(ok.unwrap());
  let text = &server.mails()[0].text;
  assert!(text.starts_with("Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\""), "{}", text);
  assert!(!text.contains("secret body"));
  let parts = top_parts(text);
  assert_eq!(parts[0], "Content-Transfer-Encoding: 7bit\nMIME-Version: 1.0\nContent-Type: application/pgp-encrypted; charset=\"us-ascii\"\n\nVersion: 1");
  assert!(parts[1].starts_with("Content-Type: application/octet-stream\nMIME-Version: 1.0\n\n-----BEGIN PGP MESSAGE-----"));
  let (secret, _) = SignedSecretKey::from_string(PGPY_PRV).unwrap();
  let inner = decrypt(&parts[1], &secret);
  assert!(inner.starts_with("Content-Type: text/plain; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nFrom: user@nuxref.com\nTo: recipient@example.com\nSubject: secret subject\n\nsecret body"), "{}", inner);
  // No usable sender key for an Autocrypt header
  assert!(!text.contains("Autocrypt:"));
}

/// Upstream test_plugin_email_pgp_encrypt_external_recipient_no_autogen and
/// test_plugin_email_pgp_sign_no_privkey_fails.
#[tokio::test]
async fn test_pgp_without_keys_fails() {
  let server = FakeSmtp::start(ServerConfig::default()).await;
  for url in [
    "mailto://user:pass@nuxref.com/other@example.org?pgp=encrypt",
    "mailto://user:pass@nuxref.com?pgp=sign",
    "mailto://user:pass@nuxref.com?pgp=sign&pgpprv=/nonexistent/key.asc",
  ] {
    let (ok, _) = against(&server, email(url).send(&ctx("t", "b", NotifyFormat::Text))).await;
    assert!(!ok.unwrap(), "{}", url);
  }
  assert!(server.mails().is_empty());
}

/// pgp=sign with WKD (upstream test_plugin_email_pgp_sign_opportunistic_encrypt):
/// signed and encrypted when WKD has the recipient's key, signed otherwise.
#[tokio::test]
async fn test_pgp_sign_encrypts_with_wkd_key() {
  use wiremock::matchers::{method, path_regex};
  use wiremock::{Mock, MockServer, ResponseTemplate};
  let wkd = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path_regex(r"^/openpgpkey\.example\.com/\.well-known/openpgpkey/example\.com/hu/[a-z0-9]{32}$"))
    .respond_with(ResponseTemplate::new(200).set_body_string(PGPY_PUB))
    .mount(&wkd)
    .await;
  let dir = tempfile::tempdir().unwrap();
  let prv = write_key(&dir, "prv.asc", VALID_PRV);
  let server = FakeSmtp::start(ServerConfig::default()).await;
  for (to, encrypted) in [("friend@example.com", true), ("friend@other.test", false)] {
    let mut e = email(&format!("mailto://user:pass@nuxref.com/{}?pgp=sign&wkd=yes&pgpprv={}", to, prv));
    e.pgp.set_wkd(Some(crate::utils::wkd::WkdController::for_test(&wkd.uri())));
    let (ok, _) = against(&server, e.send(&ctx("t", "wkd body", NotifyFormat::Text))).await;
    assert!(ok.unwrap(), "{}", to);
    let text = server.mails().last().unwrap().text.clone();
    assert_eq!(text.starts_with("Content-Type: multipart/encrypted;"), encrypted, "{}", text);
    if encrypted {
      let (secret, _) = SignedSecretKey::from_string(PGPY_PRV).unwrap();
      let inner = decrypt(&top_parts(&text)[1], &secret);
      assert!(inner.starts_with("Content-Type: multipart/signed; micalg=\"pgp-sha256\""), "{}", inner);
      assert!(inner.contains("wkd body"));
    } else {
      assert!(text.starts_with("Content-Type: multipart/signed;"));
    }
  }
}

/// A self-send may generate the sender's key pair in the service's storage
/// (upstream test_plugin_email_pgp_encrypt_self_send_autogens_key); the
/// generated key is Autocrypt-shaped, so the header is added, once, and a
/// custom Autocrypt header is dropped (test_plugin_email_autocrypt_deduplication).
#[tokio::test]
async fn test_pgp_encrypt_self_send_generates_keys() {
  let store = tempfile::tempdir().unwrap();
  let server = FakeSmtp::start(ServerConfig::default()).await;
  let url = "mailto://user:pass@nuxref.com?pgp=encrypt&+autocrypt=addr%3Dattacker%40example.com%3B%20keydata%3Dbogus";
  let asset = crate::asset::AppriseAsset { storage_path: Some(store.path().to_path_buf()), ..Default::default() };
  let mut apprise = crate::Apprise::with_asset(asset);
  assert!(apprise.add(url));
  let (result, _) = against(&server, apprise.notify(&ctx("t", "self body", NotifyFormat::Text))).await;
  assert!(result.is_success());

  let text = &server.mails()[0].text;
  assert!(text.starts_with("Content-Type: multipart/encrypted;"));
  assert_eq!(text.to_lowercase().matches("autocrypt:").count(), 1);
  assert!(!text.contains("attacker@example.com"));
  let autocrypt = text.split("Autocrypt: ").nth(1).unwrap();
  assert!(autocrypt.starts_with("addr=user@nuxref.com; prefer-encrypt=mutual; keydata="));

  // The pair was written to the service's storage directory
  let files: Vec<std::path::PathBuf> = walk(store.path());
  let private = files.iter().find(|p| p.ends_with("user-prv.asc")).expect("private key written");
  assert!(files.iter().any(|p| p.ends_with("user-pub.asc")));
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(private).unwrap().permissions().mode() & 0o777, 0o600);
  }
  let (secret, _) = SignedSecretKey::from_string(&std::fs::read_to_string(private).unwrap()).unwrap();
  let inner = decrypt(&top_parts(text)[2 - 1], &secret);
  assert!(inner.contains("self body"));
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).unwrap().flatten() {
    let path = entry.path();
    if path.is_dir() { out.extend(walk(&path)) } else { out.push(path) }
  }
  out
}

/// Writes what this port produces (a generated key pair, signed and
/// encrypted messages, an Autocrypt header) to `$APPRISE_PGP_DUMP`, so
/// GnuPG or PGPy can check it independently:
/// `APPRISE_PGP_DUMP=/tmp/out cargo test -p apprise-core --lib dump_pgp_artifacts -- --ignored`
#[tokio::test]
#[ignore]
async fn dump_pgp_artifacts() {
  let out = std::path::PathBuf::from(std::env::var("APPRISE_PGP_DUMP").expect("set APPRISE_PGP_DUMP"));
  std::fs::create_dir_all(&out).unwrap();
  let keys = out.join("keys");
  let pgp = PgpController::new(None, None, Some("user@nuxref.com"), "Apprise", None).with_path(Some(keys.clone()));
  assert!(pgp.keygen(None, None, false).await);
  std::fs::write(out.join("autocrypt.txt"), pgp.autocrypt_header().await.unwrap()).unwrap();
  let pgpy_pub = out.join("pgpy-pub.asc");
  std::fs::write(&pgpy_pub, PGPY_PUB).unwrap();
  let own_prv = keys.join("user-prv.asc").display().to_string();
  let own_pub = keys.join("user-pub.asc").display().to_string();
  let cases = [
    ("signed.eml", format!("mailto://user:pass@nuxref.com/other@example.org?pgp=sign&pgpprv={}", own_prv)),
    ("signed-encrypted.eml", format!("mailto://user:pass@nuxref.com/recipient@example.com?pgp=sign&pgpprv={}&pgppub={}", own_prv, pgpy_pub.display())),
    ("encrypted-pgpy.eml", format!("mailto://user:pass@nuxref.com/recipient@example.com?pgp=encrypt&pgppub={}&pgpprv={}", pgpy_pub.display(), own_prv)),
    ("encrypted-own.eml", format!("mailto://user:pass@nuxref.com?pgp=encrypt&pgppub={}", own_pub)),
  ];
  for (name, url) in cases {
    let server = FakeSmtp::start(ServerConfig::default()).await;
    let mut context = ctx("Ünïcode subject", "Line one\n.dot line\nHéllo wörld", NotifyFormat::Text);
    context.attachments = vec![Attachment { name: "notes.txt".to_string(), data: b"attached\n".to_vec(), mime_type: "text/plain".to_string() }];
    let (ok, _) = against(&server, email(&url).send(&context)).await;
    assert!(ok.unwrap(), "{}", name);
    // The exact bytes the server received, dot-stuffing undone
    let raw = server.mails()[0].raw.split("\r\n").map(|l| l.strip_prefix('.').unwrap_or(l)).collect::<Vec<_>>().join("\r\n");
    std::fs::write(out.join(name), raw).unwrap();
  }
}

//! XMPP notifications: a small client that logs in, sends one message per
//! target (joining Multi-User Chat rooms first) and closes the stream.
//!
//! Upstream drives slixmpp; this follows what slixmpp puts on the wire for
//! upstream's configuration (`apprise/plugins/xmpp/`).

mod sasl;
#[cfg(test)]
mod tests;
mod xml;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::email::parse_list;
use crate::utils::parse::ParsedUrl;
use crate::utils::parse::{parse_bool, parse_bool_or, unquote};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use xml::{Element, Event, StreamReader, escape_attr, escape_text};

const NS_TLS: &str = "urn:ietf:params:xml:ns:xmpp-tls";
const NS_SASL: &str = "urn:ietf:params:xml:ns:xmpp-sasl";
const NS_BIND: &str = "urn:ietf:params:xml:ns:xmpp-bind";
const NS_SESSION: &str = "urn:ietf:params:xml:ns:xmpp-session";
const NS_ROSTER: &str = "jabber:iq:roster";
const NS_MUC: &str = "http://jabber.org/protocol/muc";
const NS_MUC_USER: &str = "http://jabber.org/protocol/muc#user";
const NS_PING: &str = "urn:xmpp:ping";
const NS_SASL_CB: &str = "urn:xmpp:sasl-cb:0";

/// How long to wait for a Multi-User Chat join to complete.
const MUC_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the server to close its side of the stream.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Upstream `SECURE_MODES`, in the order a `?mode=` prefix is matched.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SecureMode {
  StartTls,
  Tls,
  None,
}

impl SecureMode {
  const ALL: [(&'static str, SecureMode); 3] = [("starttls", SecureMode::StartTls), ("tls", SecureMode::Tls), ("none", SecureMode::None)];

  fn default_port(self) -> u16 {
    if self == SecureMode::Tls { 5223 } else { 5222 }
  }
}

#[derive(Debug, Clone, PartialEq)]
struct Target {
  jid: String,
  /// A `#room` target: a Multi-User Chat room (`groupchat` messages).
  muc: bool,
}

pub struct Xmpp {
  /// The URL host: the domain of JIDs given without one.
  host: String,
  /// `?xmpp=`: the server to connect to when it differs from the host.
  xmpp_host: String,
  port: Option<u16>,
  jid: String,
  /// The URL user, the nickname fallback.
  user: String,
  password: String,
  targets: Vec<Target>,
  mode: SecureMode,
  verify: bool,
  roster: bool,
  subject: bool,
  keepalive: bool,
  scramplus: bool,
  /// `?name=`: the Multi-User Chat nickname.
  name: Option<String>,
  /// The whole exchange must finish within this (`?cto=`, at least 5s).
  timeout: Duration,
  /// The session kept open with `?keepalive=yes`.
  session: tokio::sync::Mutex<Option<Session>>,
  tags: Vec<String>,
}

impl Xmpp {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parses this URL with host verification, which also refuses
    // a port that is not a number
    let host = url.verified_host()?;
    let (user, password) = (url.user.clone(), url.password.clone());
    let user = user.unwrap_or_default();

    let Some((jid, _)) = normalize_jid(&user, &host) else {
      tracing::warn!("An invalid XMPP JID ({}) was specified.", user);
      return None;
    };

    // Upstream unquotes the path entries and ?to= once more (and its
    // URLBase.parse_list() each entry of ?to= again)
    let mut entries: Vec<String> = url.path_parts.iter().map(|t| unquote(t)).collect();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_list(&[unquote(to)]).iter().map(|t| unquote(t)));
    }
    let mut targets = Vec::new();
    for entry in parse_list(&entries) {
      match normalize_jid(&entry, &host) {
        Some((jid, muc)) => targets.push(Target { jid, muc }),
        None => tracing::warn!("Dropped invalid XMPP target ({}).", entry),
      }
    }

    // xmpp:// is plaintext and xmpps:// negotiates STARTTLS, unless ?mode=
    // says otherwise; a secure schema never runs in plaintext.
    let secure = url.schema == "xmpps";
    let mode = match url.get("mode").map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty()) {
      Some(m) => match SecureMode::ALL.iter().find(|(name, _)| name.starts_with(&m)) {
        Some((_, mode)) => *mode,
        None => {
          tracing::warn!("The XMPP secure mode specified ({}) is invalid.", m);
          return None;
        }
      },
      None if secure => SecureMode::StartTls,
      None => SecureMode::None,
    };
    let mode = if secure && mode == SecureMode::None {
      tracing::warn!("Ambiguous XMPP configuration: secure=True and mode=None; secure setting prevails; setting mode=starttls");
      SecureMode::StartTls
    } else {
      mode
    };

    let flag = |key: &str, default: bool| url.get(key).filter(|v| !v.is_empty()).map(parse_bool).unwrap_or(default);
    let name = url.get("name").filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).map(str::to_string);
    // ParsedUrl has applied ?port= already
    let port = url.port.filter(|p| *p > 0);
    let cto = crate::utils::socket::url_timeouts(url).connect_timeout().unwrap_or(Duration::from_secs(4));

    Some(Self {
      xmpp_host: url.get("xmpp").map(|h| h.trim().to_string()).unwrap_or_default(),
      host,
      port,
      jid,
      user,
      password: password.unwrap_or_default(),
      targets,
      mode,
      verify: url.verify_certificate(),
      roster: flag("roster", false),
      subject: flag("subject", false),
      keepalive: flag("keepalive", false),
      scramplus: url.get("scramplus").filter(|v| !v.is_empty()).map(|v| parse_bool_or(v, true)).unwrap_or(true),
      name,
      timeout: cto.max(Duration::from_secs(5)),
      session: tokio::sync::Mutex::new(None),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "XMPP",
      service_url: Some("https://xmpp.org/"),
      setup_url: Some("https://appriseit.com/services/xmpp/"),
      protocols: vec!["xmpp", "xmpps"],
      description: "Send messages to XMPP (Jabber) users and Multi-User Chat rooms.",
      attachment_support: false,
    }
  }

  /// The domain of our JID, which names the server in the stream header
  /// and in the TLS handshake.
  fn domain(&self) -> &str {
    let bare = self.jid.split('/').next().unwrap_or(&self.jid);
    bare.rsplit('@').next().unwrap_or(bare)
  }

  fn connect_timeout(&self) -> Duration {
    Duration::from_secs_f64((self.timeout.as_secs_f64() / 3.0).clamp(3.0, 15.0))
  }

  fn roster_timeout(&self) -> Duration {
    Duration::from_secs_f64((self.timeout.as_secs_f64() / 3.0).clamp(2.0, 10.0))
  }

  async fn tls(&self, tcp: TcpStream) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, Option<Vec<u8>>), Failure> {
    let name = rustls::pki_types::ServerName::try_from(self.domain().to_string()).map_err(|e| Failure::other(format!("invalid TLS server name: {}", e)))?;
    let tls = crate::utils::socket::tls_connector(self.verify).connect(name, tcp).await.map_err(|e| Failure::other(format!("TLS handshake failed: {}", e)))?;
    // tls-exporter (RFC 9266) channel binding, defined for TLS 1.3 only.
    let (_, connection) = tls.get_ref();
    let binding = (connection.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3))
      .then(|| connection.export_keying_material([0u8; 32], b"EXPORTER-Channel-Binding", None).ok().map(|b| b.to_vec()))
      .flatten();
    Ok((tls, binding))
  }

  /// Connect, secure the stream, log in and bind a resource.
  async fn establish(&self) -> Result<Session, Failure> {
    let host = if self.xmpp_host.is_empty() { &self.host } else { &self.xmpp_host };
    let port = self.port.unwrap_or(self.mode.default_port());
    let addr = format!("{}:{}", host, port);
    let tcp = tokio::time::timeout(self.connect_timeout(), TcpStream::connect(&addr))
      .await
      .map_err(|_| Failure::other(format!("XMPP connect timed out after {:.2}s", self.connect_timeout().as_secs_f64())))?
      .map_err(|e| Failure::other(format!("XMPP connect to {} failed: {}", addr, e)))?;

    let domain = self.domain().to_string();
    let (mut link, features, binding, encrypted) = match self.mode {
      SecureMode::Tls => {
        let (tls, binding) = tokio::time::timeout(self.connect_timeout(), self.tls(tcp)).await.map_err(|_| Failure::other("TLS handshake timed out"))??;
        let mut link = Link::new(Box::new(tls) as Box<dyn XmppStream>);
        let features = link.open(&domain).await?;
        (link, features, binding, true)
      }
      SecureMode::StartTls => {
        let mut plain = Link::new(tcp);
        let features = plain.open(&domain).await?;
        if features.child_ns("starttls", NS_TLS).is_none() {
          return Err(Failure::other("the server does not offer STARTTLS"));
        }
        plain.write(&format!("<starttls xmlns='{}' />", NS_TLS)).await?;
        let reply = plain.stanza().await?;
        if !reply.is("proceed") {
          return Err(Failure::other("the server refused STARTTLS"));
        }
        let (tls, binding) = self.tls(plain.stream).await?;
        let mut link = Link::new(Box::new(tls) as Box<dyn XmppStream>);
        let features = link.open(&domain).await?;
        (link, features, binding, true)
      }
      SecureMode::None => {
        let mut link = Link::new(Box::new(tcp) as Box<dyn XmppStream>);
        let features = link.open(&domain).await?;
        (link, features, None, false)
      }
    };

    self.authenticate(&mut link, &features, binding, encrypted).await?;
    let features = link.open(&domain).await?;
    let bound_jid = self.bind(&mut link, &features).await?;
    tracing::debug!("XMPP session bound as {}", bound_jid);
    Ok(Session { link, greeted: false })
  }

  /// SASL: the strongest mechanism the server offers, as slixmpp picks it.
  async fn authenticate<S: XmppStream>(&self, link: &mut Link<S>, features: &Element, binding: Option<Vec<u8>>, encrypted: bool) -> Result<(), Failure> {
    let offered: Vec<String> = features
      .child_ns("mechanisms", NS_SASL)
      .map(|m| m.children.iter().filter(|c| c.is("mechanism")).map(|c| c.text.trim().to_string()).collect())
      .unwrap_or_default();
    if offered.is_empty() {
      return Err(Failure::other("the server offers no SASL mechanisms"));
    }
    // slixmpp refuses PLAIN and SCRAM on an unencrypted stream.
    if !encrypted {
      return Err(Failure::other("refusing to send credentials over an unencrypted connection (use xmpps:// or ?mode=starttls)"));
    }

    // ?scramplus=no leaves the -PLUS mechanisms out. XEP-0440 lets the
    // server list its channel binding types; tls-exporter must be one.
    let server_plus = offered.iter().any(|m| m.ends_with("-PLUS"));
    let cb_types: Option<Vec<&str>> = features.child_ns("sasl-channel-binding", NS_SASL_CB).map(|e| e.children.iter().filter_map(|c| c.attr("type")).collect());
    let can_bind = self.scramplus && binding.is_some() && cb_types.as_ref().is_none_or(|t| t.contains(&"tls-exporter"));
    let mechanism = sasl::choose(&offered, can_bind).ok_or_else(|| Failure::other("no appropriate login method"))?;
    tracing::debug!("XMPP SASL mechanism: {}", mechanism.name());

    let username = self.jid.split('@').next().unwrap_or(&self.jid);
    let auth = |payload: &[u8]| format!("<auth xmlns='{}' mechanism='{}'>{}</auth>", NS_SASL, mechanism.name(), encode_sasl(payload));
    match mechanism {
      sasl::Mechanism::Plain => {
        link.write(&auth(&sasl::plain(username, &self.password))).await?;
        let reply = link.stanza().await?;
        sasl_outcome(&reply)?;
      }
      sasl::Mechanism::Scram { hash, plus } => {
        // RFC 5802 gs2 flag: bind, or say whether we could have.
        let flag = if plus {
          "p=tls-exporter"
        } else if server_plus || !can_bind {
          "n"
        } else {
          "y"
        };
        let data = if plus { binding.unwrap_or_default() } else { Vec::new() };
        let mut scram = sasl::Scram::new(hash, username, &self.password, flag, data);
        link.write(&auth(&scram.client_first())).await?;
        let mut verified = false;
        loop {
          let reply = link.stanza().await?;
          if reply.is("challenge") {
            let challenge = B64.decode(reply.text.trim()).map_err(|_| Failure::other("invalid SASL challenge"))?;
            let response = if challenge.starts_with(b"v=") || challenge.starts_with(b"e=") {
              scram.verify_server_final(&challenge).map_err(Failure::other)?;
              verified = true;
              Vec::new()
            } else {
              scram.client_final(&challenge).await.map_err(Failure::other)?
            };
            link.write(&format!("<response xmlns='{}'>{}</response>", NS_SASL, encode_sasl(&response))).await?;
            continue;
          }
          sasl_outcome(&reply)?;
          let data = B64.decode(reply.text.trim()).unwrap_or_default();
          if !data.is_empty() {
            scram.verify_server_final(&data).map_err(Failure::other)?;
          } else if !verified {
            return Err(Failure::other("the server did not prove it knows the password"));
          }
          break;
        }
      }
    }
    Ok(())
  }

  /// Bind a resource (the JID's own, or one the server picks) and start
  /// the legacy session when the server requires one.
  async fn bind<S: XmppStream>(&self, link: &mut Link<S>, features: &Element) -> Result<String, Failure> {
    if features.child_ns("bind", NS_BIND).is_none() {
      return Err(Failure::other("the server does not offer resource binding"));
    }
    let resource = self.jid.split_once('/').map(|(_, r)| format!("<resource>{}</resource>", escape_text(r))).unwrap_or_default();
    let id = new_id();
    let request = if resource.is_empty() {
      format!("<iq type='set' id='{}'><bind xmlns='{}' /></iq>", id, NS_BIND)
    } else {
      format!("<iq type='set' id='{}'><bind xmlns='{}'>{}</bind></iq>", id, NS_BIND, resource)
    };
    link.write(&request).await?;
    let reply = link.iq_reply(&id).await?;
    if reply.attr("type") != Some("result") {
      return Err(Failure::other("resource binding failed"));
    }
    let bound = reply.child("bind").and_then(|b| b.child("jid")).map(|j| j.text.trim().to_string()).unwrap_or_else(|| self.jid.clone());

    if let Some(session) = features.child_ns("session", NS_SESSION)
      && session.child("optional").is_none()
    {
      let id = new_id();
      link.write(&format!("<iq type='set' id='{}'><session xmlns='{}' /></iq>", id, NS_SESSION)).await?;
      if link.iq_reply(&id).await?.attr("type") != Some("result") {
        return Err(Failure::other("session establishment failed"));
      }
    }
    Ok(bound)
  }

  /// Send the notification on an established session.
  async fn deliver(&self, session: &mut Session, targets: &[Target], app_id: &str, subject: &str, body: &str) -> Result<(), Failure> {
    let link = &mut session.link;
    if !session.greeted {
      session.greeted = true;
      link.write(&format!("<presence id='{}' />", new_id())).await?;
      if self.roster {
        let id = new_id();
        link.write(&format!("<iq type='get' id='{}'><query xmlns='{}' /></iq>", id, NS_ROSTER)).await?;
        let _ = tokio::time::timeout(self.roster_timeout(), link.iq_reply(&id)).await;
      }
    }

    let own = [Target { jid: self.jid.clone(), muc: false }];
    let targets = if self.targets.is_empty() { &own[..] } else { targets };
    let nick = self.name.clone().filter(|n| !n.is_empty()).or_else(|| Some(self.user.clone()).filter(|u| !u.is_empty())).unwrap_or_else(|| app_id.to_string());
    for target in targets {
      if target.muc {
        let occupant = format!("{}/{}", target.jid, nick);
        link
          .write(&format!(
            "<presence to='{}' id='{}'><show>chat</show><x xmlns='{}'><history maxchars='9' /></x></presence>",
            escape_attr(&occupant),
            new_id(),
            NS_MUC
          ))
          .await?;
        // Upstream sends the message whether or not the join completes.
        if tokio::time::timeout(MUC_JOIN_TIMEOUT, link.muc_joined(&target.jid)).await.is_err() {
          tracing::debug!("XMPP join of {} was not confirmed", target.jid);
        }
      }

      let mut stanza = format!("<message to='{}' type='{}' id='{}'>", escape_attr(&target.jid), if target.muc { "groupchat" } else { "chat" }, new_id());
      if !body.is_empty() {
        stanza.push_str(&format!("<body>{}</body>", escape_text(body)));
      }
      if !subject.is_empty() {
        stanza.push_str(&format!("<subject>{}</subject>", escape_text(subject)));
      }
      stanza.push_str("</message>");
      link.write(&stanza).await?;
      tracing::info!("Sent XMPP notification to {}", target.jid);
    }
    Ok(())
  }

  async fn send_once(&self, targets: &[Target], app_id: &str, subject: &str, body: &str) -> Result<(), Failure> {
    let mut session = self.establish().await?;
    let result = self.deliver(&mut session, targets, app_id, subject, body).await;
    // Close the stream only after everything was written, and give the
    // server a moment to process it before the socket goes away.
    session.link.close().await;
    result
  }

  async fn send_keepalive(&self, targets: &[Target], app_id: &str, subject: &str, body: &str) -> Result<(), Failure> {
    let mut held = self.session.lock().await;
    if let Some(session) = held.as_mut()
      && !session.link.still_open().await
    {
      *held = None;
    }
    if held.is_none() {
      *held = Some(self.establish().await?);
    }
    let session = held.as_mut().expect("session was just established");
    let result = self.deliver(session, targets, app_id, subject, body).await;
    if result.is_err() {
      *held = None;
    }
    result
  }
}

/// Upstream `NotifyXMPP.normalize_jid()`: `local[@domain][/resource]`, where
/// a leading `#` marks a Multi-User Chat room and a missing domain is
/// `default_host`. Returns the JID and whether it is a room.
fn normalize_jid(value: &str, default_host: &str) -> Option<(String, bool)> {
  let raw = value.trim();
  let (is_room, rest) = match raw.strip_prefix('#').or_else(|| raw.strip_prefix("%23")) {
    Some(rest) => (true, rest),
    None => (false, raw),
  };
  let stop = |c: char| c == '@' || c == '/' || c.is_whitespace();
  let local_end = rest.find(stop).unwrap_or(rest.len());
  let (local, mut rest) = rest.split_at(local_end);
  if local.is_empty() {
    return None;
  }

  let mut domain = None;
  if let Some(after) = rest.strip_prefix('@') {
    let end = after.find(stop).unwrap_or(after.len());
    if end == 0 {
      return None;
    }
    domain = Some(&after[..end]);
    rest = &after[end..];
  }

  let mut resource = None;
  if let Some(after) = rest.strip_prefix('/').or_else(|| rest.strip_prefix("%2F")) {
    let end = after.find(|c: char| c == '%' || c == '/' || c.is_whitespace()).unwrap_or(after.len());
    let tail = &after[end..];
    if end == 0 || !(tail.trim().is_empty() || tail.starts_with('/') || tail.starts_with("%2F")) {
      return None;
    }
    resource = Some(&after[..end]);
    rest = "";
  }
  if !rest.trim().is_empty() {
    return None;
  }

  let mut jid = format!("{}@{}", local, domain.unwrap_or(default_host));
  if let Some(resource) = resource {
    jid = format!("{}/{}", jid, resource);
  }
  Some((jid, is_room))
}

/// SASL payloads are base64; an empty one is sent as `=`.
fn encode_sasl(payload: &[u8]) -> String {
  if payload.is_empty() { "=".into() } else { B64.encode(payload) }
}

/// A random stanza ID (slixmpp uses a UUID's hex digits).
fn new_id() -> String {
  hex::encode(rand::random::<[u8; 16]>())
}

/// The outcome of a SASL exchange: `<success/>` or `<failure/>`.
fn sasl_outcome(reply: &Element) -> Result<(), Failure> {
  if reply.is("success") {
    return Ok(());
  }
  if reply.is("failure") {
    let condition = reply.children.iter().find(|c| !c.is("text")).map(|c| c.local().to_string()).unwrap_or_default();
    let text = reply.child("text").map(|t| t.text.clone()).unwrap_or_default();
    let detail = format!("{} {}", condition, text).to_lowercase();
    return Err(Failure::Auth {
      channel_binding: detail.contains("channel") && detail.contains("binding"),
      reason: format!("{} {}", condition, text).trim().to_string(),
    });
  }
  Err(Failure::other(format!("unexpected <{}> during authentication", reply.name)))
}

/// Why a send failed.
#[derive(Debug)]
enum Failure {
  /// The server rejected our credentials (or, with `channel_binding`, the
  /// SCRAM-PLUS channel binding).
  Auth {
    channel_binding: bool,
    reason: String,
  },
  Other(String),
}

impl Failure {
  fn other(reason: impl Into<String>) -> Self {
    Self::Other(reason.into())
  }
}

impl From<String> for Failure {
  fn from(reason: String) -> Self {
    Self::Other(reason)
  }
}

/// A plain or TLS connection.
trait XmppStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> XmppStream for T {}

/// An established, bound session.
struct Session {
  link: Link<Box<dyn XmppStream>>,
  /// Initial presence (and the roster request) went out.
  greeted: bool,
}

/// The XML stream over one connection.
struct Link<S> {
  stream: S,
  reader: StreamReader,
}

impl<S: XmppStream> Link<S> {
  fn new(stream: S) -> Self {
    Self { stream, reader: StreamReader::default() }
  }

  /// Write and flush, so nothing waits in a TLS buffer.
  async fn write(&mut self, data: &str) -> Result<(), String> {
    tracing::trace!("XMPP SEND: {}", data);
    self.stream.write_all(data.as_bytes()).await.map_err(|e| e.to_string())?;
    self.stream.flush().await.map_err(|e| e.to_string())
  }

  async fn event(&mut self) -> Result<Event, String> {
    loop {
      if let Some(event) = self.reader.next_event()? {
        return Ok(event);
      }
      let mut buf = [0u8; 8192];
      let n = self.stream.read(&mut buf).await.map_err(|e| e.to_string())?;
      if n == 0 {
        return Err("connection closed by the server".into());
      }
      self.reader.feed(&buf[..n]);
    }
  }

  /// The next stanza. A stream error or the end of the stream fails.
  async fn stanza(&mut self) -> Result<Element, String> {
    match self.event().await? {
      // Only a stream error is a top-level <error>.
      Event::Stanza(stanza) if stanza.is("error") => {
        let condition = stanza.children.first().map(|c| c.local().to_string()).unwrap_or_default();
        Err(format!("stream error: {}", condition))
      }
      Event::Stanza(stanza) => Ok(stanza),
      Event::StreamClose => Err("the server closed the stream".into()),
      Event::StreamOpen(_) => Err("unexpected stream header".into()),
    }
  }

  /// Open (or restart) the stream and return the server's features.
  async fn open(&mut self, domain: &str) -> Result<Element, String> {
    self
      .write(&format!(
        "<stream:stream to='{}' xmlns:stream='http://etherx.jabber.org/streams' xmlns='jabber:client' xml:lang='en' version='1.0'>",
        escape_attr(domain)
      ))
      .await?;
    loop {
      match self.event().await? {
        Event::StreamOpen(_) => break,
        Event::Stanza(s) if s.is("error") => return Err(format!("stream error: {}", s.children.first().map(|c| c.local()).unwrap_or(""))),
        Event::StreamClose => return Err("the server closed the stream".into()),
        Event::Stanza(_) => {}
      }
    }
    let features = self.stanza().await?;
    if !features.is("features") {
      return Err(format!("expected stream features, got <{}>", features.name));
    }
    Ok(features)
  }

  /// Answer an XEP-0199 ping (the server drops clients that do not).
  async fn answer_ping(&mut self, stanza: &Element) -> Result<bool, String> {
    if stanza.is("iq") && stanza.attr("type") == Some("get") && stanza.child_ns("ping", NS_PING).is_some() {
      let to = stanza.attr("from").map(|f| format!(" to='{}'", escape_attr(f))).unwrap_or_default();
      let reply = format!("<iq type='result' id='{}'{} />", escape_attr(stanza.attr("id").unwrap_or("")), to);
      self.write(&reply).await?;
      return Ok(true);
    }
    Ok(false)
  }

  /// Wait for the reply to the IQ with `id`.
  async fn iq_reply(&mut self, id: &str) -> Result<Element, String> {
    loop {
      let stanza = self.stanza().await?;
      if self.answer_ping(&stanza).await? {
        continue;
      }
      if stanza.is("iq") && stanza.attr("id") == Some(id) {
        return Ok(stanza);
      }
    }
  }

  /// Wait until we are in `room`: our own presence (status 110) and the
  /// room subject, or a presence error from the room.
  async fn muc_joined(&mut self, room: &str) -> Result<(), String> {
    let from_room = |s: &Element| s.attr("from").is_some_and(|f| f.split('/').next().is_some_and(|bare| bare.eq_ignore_ascii_case(room)));
    let (mut presence, mut subject) = (false, false);
    while !(presence && subject) {
      let stanza = self.stanza().await?;
      if self.answer_ping(&stanza).await? || !from_room(&stanza) {
        continue;
      }
      if stanza.is("presence") {
        if stanza.attr("type") == Some("error") {
          return Ok(());
        }
        let own = stanza.child_ns("x", NS_MUC_USER).is_some_and(|x| x.children.iter().any(|c| c.is("status") && c.attr("code") == Some("110")));
        presence |= own;
      } else if stanza.is("message") && stanza.child("subject").is_some() {
        subject = true;
      }
    }
    Ok(())
  }

  /// With keepalive, check the held connection before reusing it: answer
  /// what arrived meanwhile and notice when the server went away.
  async fn still_open(&mut self) -> bool {
    loop {
      while let Ok(Some(event)) = self.reader.next_event() {
        match event {
          Event::Stanza(stanza) => {
            if stanza.is("error") || self.answer_ping(&stanza).await.is_err() {
              return false;
            }
          }
          Event::StreamClose | Event::StreamOpen(_) => return false,
        }
      }
      let mut buf = [0u8; 8192];
      match tokio::time::timeout(Duration::ZERO, self.stream.read(&mut buf)).await {
        Err(_) => return true,
        Ok(Ok(0)) | Ok(Err(_)) => return false,
        Ok(Ok(n)) => self.reader.feed(&buf[..n]),
      }
    }
  }

  /// End the stream and wait briefly for the server to end its side, so
  /// the messages are processed before the connection closes.
  async fn close(&mut self) {
    if self.write("</stream:stream>").await.is_ok() {
      let deadline = Instant::now() + CLOSE_TIMEOUT;
      // An event that is ready at once completes before its timeout is
      // looked at, so a server that keeps sending would keep us here
      while Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, self.event()).await {
          Ok(Ok(Event::StreamClose)) | Ok(Err(_)) | Err(_) => break,
          Ok(Ok(_)) => {}
        }
      }
    }
    let _ = self.stream.shutdown().await;
  }
}

#[async_trait]
impl Notify for Xmpp {
  fn schemas(&self) -> &[&str] {
    &["xmpp", "xmpps"]
  }
  fn service_name(&self) -> &str {
    "XMPP"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The title goes in the message subject with `?subject=yes`; otherwise
  /// it is put in front of the body.
  fn title_maxlen(&self) -> usize {
    if self.subject { 250 } else { 0 }
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let (subject, body) = if self.subject {
      (ctx.title.clone(), ctx.body.clone())
    } else if ctx.title.is_empty() {
      (String::new(), ctx.body.clone())
    } else {
      (String::new(), format!("{}\r\n{}", ctx.title, ctx.body))
    };
    tracing::debug!(
      "XMPP init: jid={} host={} port={} mode={:?} verify_certificate={} subject={} roster={} keepalive={} targets={:?}",
      self.jid,
      if self.xmpp_host.is_empty() { &self.host } else { &self.xmpp_host },
      self.port.unwrap_or(self.mode.default_port()),
      self.mode,
      self.verify,
      self.subject,
      self.roster,
      self.keepalive,
      self.targets.iter().map(|t| &t.jid).collect::<Vec<_>>()
    );

    // A retry only sends to the targets that did not get the message yet
    let targets: Vec<Target> = self.targets.iter().filter(|t| !crate::delivery::is_delivered(&t.jid, false)).cloned().collect();
    if !self.targets.is_empty() && targets.is_empty() {
      return Ok(true);
    }

    crate::notify::throttle().await;
    let app_id = &ctx.asset.app_id;
    let attempt = async {
      if self.keepalive { self.send_keepalive(&targets, app_id, &subject, &body).await } else { self.send_once(&targets, app_id, &subject, &body).await }
    };
    let result = match tokio::time::timeout(self.timeout, attempt).await {
      Ok(result) => result,
      Err(_) => {
        if self.keepalive {
          *self.session.lock().await = None;
        }
        Err(Failure::other(format!("XMPP send timed out after {:.2}s.", self.timeout.as_secs_f64())))
      }
    };
    match result {
      Ok(()) => {
        for target in &targets {
          crate::delivery::mark_delivered(&target.jid, false);
        }
        Ok(true)
      }
      Err(Failure::Auth { channel_binding: true, reason }) => {
        if self.scramplus {
          tracing::warn!(
            "XMPP authentication failed: the server rejected SASL SCRAM-PLUS channel binding. Add ?scramplus=no to your Apprise URL to disable SCRAM-PLUS mechanism negotiation."
          );
        } else {
          tracing::warn!("XMPP authentication failed: the server rejected channel binding despite ?scramplus=no being set.");
        }
        tracing::debug!("XMPP Exception: {}", reason);
        Ok(false)
      }
      Err(Failure::Auth { reason, .. }) => {
        tracing::warn!("XMPP authentication failed for {}.", self.jid);
        tracing::debug!("XMPP Exception: {}", reason);
        Ok(false)
      }
      Err(Failure::Other(reason)) => {
        tracing::warn!("XMPP send failed.");
        tracing::debug!("XMPP Exception: {}", reason);
        Ok(false)
      }
    }
  }
}

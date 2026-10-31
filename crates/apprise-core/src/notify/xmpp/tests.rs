use super::*;
use crate::notify::registry::from_url;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

fn parse(url: &str) -> Option<Xmpp> {
  ParsedUrl::parse(url).and_then(|p| Xmpp::from_url(&p))
}

fn jids(x: &Xmpp) -> Vec<(&str, bool)> {
  x.targets.iter().map(|t| (t.jid.as_str(), t.muc)).collect()
}

#[test]
fn test_valid_urls() {
  for url in [
    "xmpps://user@example.com/joe",
    "xmpps://user:pass@example.com/joe",
    "xmpps://user:pass@example.com/joe?xmpp=xmpp.example.com",
    "xmpp://me:secret@example.com?mode=tls",
    "xmpps://me:secret@example.com?mode=none",
    "xmpp://me:secret@example.com",
    "xmpp://me:pass@example.com?to=a@example.com,b@example.net",
    "xmpp://me:pass@example.com/a?keepalive=yes",
    "xmpp://me:secret@example.com/a@example.com?subject=yes",
    "xmpp://me:secret@example.com?subject=yes&roster=yes",
    "xmpp://me:secret@example.com?verify=no",
    "xmpps://me:pass@example.com/a@example.com?scramplus=no",
    "xmpp://me%40other.org:pass@example.com/a",
    "xmpp://me:@example.com/a",
    "xmpp://me@example.com/a",
  ] {
    assert!(from_url(url).is_some(), "Should parse: {}", url);
  }
}

#[test]
fn test_invalid_urls() {
  for url in [
    "xmpp://",
    "xmpp://bad",
    "xmpps://me:secret@example.com?mode=invalid",
    "xmpp://:pass@example.com/a",
    "xmpp://me%20x:pass@example.com/a",
    "xmpp://me:pass@/a",
    "xmpp://me:pass@example.com:garbage/a",
  ] {
    assert!(from_url(url).is_none(), "Should not parse: {}", url);
  }
}

#[test]
fn test_modes_and_ports() {
  let x = parse("xmpp://me:secret@example.com").unwrap();
  assert_eq!((x.mode, x.mode.default_port()), (SecureMode::None, 5222));
  let x = parse("xmpps://me:secret@example.com").unwrap();
  assert_eq!(x.mode, SecureMode::StartTls);
  // A mode on xmpp:// turns it secure; xmpps:// never runs in plaintext
  assert_eq!(parse("xmpp://me:secret@example.com?mode=tls").unwrap().mode, SecureMode::Tls);
  assert_eq!(parse("xmpp://me:secret@example.com?mode=tls").unwrap().mode.default_port(), 5223);
  assert_eq!(parse("xmpps://me:secret@example.com?mode=none").unwrap().mode, SecureMode::StartTls);
  // Prefixes match in upstream order
  assert_eq!(parse("xmpp://me:pass@example.com/a?mode=s").unwrap().mode, SecureMode::StartTls);
  assert_eq!(parse("xmpp://me:pass@example.com:5999/a?mode=t").unwrap().mode, SecureMode::Tls);
  assert_eq!(parse("xmpp://me:pass@example.com:5999/a?mode=t").unwrap().port, Some(5999));
}

#[test]
fn test_jids_and_targets() {
  let x = parse("xmpp://me:pass@example.com/%23room@conf.example.com/bob/bob/res/x@y/z").unwrap();
  assert_eq!(x.jid, "me@example.com");
  assert_eq!(jids(&x), vec![("room@conf.example.com", true), ("bob@example.com", false), ("res@example.com", false), ("x@y", false), ("z@example.com", false)]);

  let x = parse("xmpp://me:pass@example.com?to=a@example.com,b@example.net").unwrap();
  assert_eq!(jids(&x), vec![("a@example.com", false), ("b@example.net", false)]);

  // A JID user keeps its own domain; resources survive encoding
  assert_eq!(parse("xmpp://me%40other.org:pass@example.com/a").unwrap().jid, "me@other.org");
  assert_eq!(parse("xmpp://me%2Fphone:pass@example.com/a").unwrap().jid, "me@example.com/phone");
  assert_eq!(jids(&parse("xmpp://me:pass@example.com/a%2Fres").unwrap()), vec![("a@example.com/res", false)]);
  // Spaces split targets; malformed ones are dropped
  assert_eq!(jids(&parse("xmpp://me:pass@example.com/a%20b").unwrap()), vec![("a@example.com", false), ("b@example.com", false)]);
  assert!(parse("xmpp://me:pass@example.com/@b").unwrap().targets.is_empty());
  assert!(parse("xmpp://me:pass@example.com/a@b@c").unwrap().targets.is_empty());
  assert_eq!(jids(&parse("xmpp://me:pass@example.com/%23room").unwrap()), vec![("room@example.com", true)]);

  assert_eq!(normalize_jid("user/res/extra", "h"), Some(("user@h/res".into(), false)));
  assert_eq!(normalize_jid("user/re%20s", "h"), None);
  assert_eq!(normalize_jid("", "h"), None);
}

#[test]
fn test_options() {
  let x = parse("xmpps://user:pass@example.com/joe?xmpp=xmpp.example.com").unwrap();
  assert_eq!((x.host.as_str(), x.xmpp_host.as_str(), x.domain()), ("example.com", "xmpp.example.com", "example.com"));

  let x = parse("xmpp://me:pass@example.com/a").unwrap();
  assert!(!x.roster && !x.subject && !x.keepalive && x.scramplus);
  assert_eq!(x.name, None);
  assert_eq!(x.title_maxlen(), 0);
  assert_eq!(x.timeout, Duration::from_secs(5));
  assert_eq!((x.connect_timeout(), x.roster_timeout()), (Duration::from_secs(3), Duration::from_secs(2)));

  let x = parse("xmpp://me:pass@example.com/a?name=Good_1&subject=yes&roster=yes&keepalive=yes&scramplus=no&cto=30").unwrap();
  assert!(x.roster && x.subject && x.keepalive && !x.scramplus);
  assert_eq!(x.name.as_deref(), Some("Good_1"));
  assert_eq!(x.title_maxlen(), 250);
  assert_eq!((x.timeout, x.connect_timeout()), (Duration::from_secs(30), Duration::from_secs(10)));

  // Only letters, digits and underscores make a nickname
  assert_eq!(parse("xmpp://me:pass@example.com/a?name=bad-nick").unwrap().name, None);
  // An unrecognised scramplus keeps it on; an empty one is ignored
  assert!(parse("xmpp://me:pass@example.com/a?scramplus=maybe").unwrap().scramplus);
  assert!(parse("xmpp://me:pass@example.com/a?scramplus=").unwrap().scramplus);
  assert!(!parse("xmpp://me:pass@example.com/a?verify=no").unwrap().verify);
  assert_eq!(parse("xmpp://me@example.com/a").unwrap().password, "");
}

/// Copies what the client sends so a test can check the exact bytes.
struct Tap<S> {
  inner: S,
  log: Arc<Mutex<Vec<u8>>>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Tap<S> {
  fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
    let before = buf.filled().len();
    let result = Pin::new(&mut self.inner).poll_read(cx, buf);
    if let Poll::Ready(Ok(())) = &result {
      self.log.lock().unwrap().extend_from_slice(&buf.filled()[before..]);
    }
    result
  }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tap<S> {
  fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
    Pin::new(&mut self.inner).poll_write(cx, buf)
  }
  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.inner).poll_flush(cx)
  }
  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.inner).poll_shutdown(cx)
  }
}

#[derive(Clone)]
struct ServerConfig {
  /// Require STARTTLS before authentication.
  starttls: bool,
  /// TLS from the first byte.
  direct_tls: bool,
  mechanisms: Vec<&'static str>,
  /// XEP-0440 channel binding types to advertise.
  cb_types: Option<Vec<&'static str>>,
  password: &'static str,
  /// Reject the login with this `<text>`.
  failure: Option<&'static str>,
  session_required: bool,
}

impl Default for ServerConfig {
  fn default() -> Self {
    Self {
      starttls: true,
      direct_tls: false,
      mechanisms: vec!["PLAIN", "SCRAM-SHA-1", "SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"],
      cb_types: None,
      password: "secret",
      failure: None,
      session_required: false,
    }
  }
}

#[derive(Debug, Default)]
struct Record {
  /// `to` of every stream header.
  stream_to: Vec<String>,
  /// The TLS server name the client asked for.
  sni: Option<String>,
  mechanism: String,
  /// SASL PLAIN payload, or the SCRAM gs2 flag.
  plain: Vec<u8>,
  gs2_flag: String,
  /// The SCRAM proof and channel binding checked out.
  scram_ok: bool,
  /// Stanzas after login, in order.
  stanzas: Vec<Element>,
  /// Everything the client sent after login, as text.
  raw: String,
  closed_stream: bool,
}

const FEATURES_BIND: &str = "<bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/>";

async fn header<S: XmppStream>(link: &mut Link<S>, record: &mut Record, features: &str) {
  match link.event().await.unwrap() {
    Event::StreamOpen(attrs) => record.stream_to.push(attrs.iter().find(|(k, _)| k == "to").map(|(_, v)| v.clone()).unwrap_or_default()),
    other => panic!("expected a stream header, got {:?}", other),
  }
  link
    .write(&format!(
      "<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' id='s1' from='example.com' version='1.0'><stream:features>{}</stream:features>",
      features
    ))
    .await
    .unwrap();
}

/// Server side of SASL. Returns whether the login succeeded.
async fn server_auth<S: XmppStream>(link: &mut Link<S>, cfg: &ServerConfig, record: &mut Record, exporter: Option<Vec<u8>>) -> bool {
  let auth = link.stanza().await.unwrap();
  assert!(auth.is("auth"), "{:?}", auth);
  record.mechanism = auth.attr("mechanism").unwrap().to_string();
  let payload = B64.decode(auth.text.trim()).unwrap();
  let fail = |text: &str| format!("<failure xmlns='{}'><not-authorized/><text>{}</text></failure>", NS_SASL, text);
  if let Some(text) = cfg.failure {
    link.write(&fail(text)).await.unwrap();
    return false;
  }
  if record.mechanism == "PLAIN" {
    record.plain = payload.clone();
    let ok = payload == format!("\0me\0{}", cfg.password).as_bytes();
    link.write(&if ok { format!("<success xmlns='{}'/>", NS_SASL) } else { fail("bad password") }).await.unwrap();
    return ok;
  }

  let sasl::Mechanism::Scram { hash, plus } = sasl::Mechanism::parse(&record.mechanism).unwrap() else { panic!() };
  let client_first = String::from_utf8(payload).unwrap();
  let (flag, rest) = client_first.split_once(',').unwrap();
  record.gs2_flag = flag.to_string();
  let bare = rest.split_once(',').unwrap().1.to_string();
  let gs2_header = format!("{},,", flag);
  let nonce = bare.split(',').find_map(|i| i.strip_prefix("r=")).unwrap().to_string();
  let salt = b"NaCl-salt";
  let server_first = format!("r={}server,s={},i=4096", nonce, B64.encode(salt));
  link.write(&format!("<challenge xmlns='{}'>{}</challenge>", NS_SASL, B64.encode(&server_first))).await.unwrap();

  let response = link.stanza().await.unwrap();
  let client_final = String::from_utf8(B64.decode(response.text.trim()).unwrap()).unwrap();
  let (without_proof, proof) = client_final.rsplit_once(",p=").unwrap();
  let binding = B64.decode(without_proof.strip_prefix("c=").unwrap().split(',').next().unwrap()).unwrap();
  let mut expected_binding = gs2_header.clone().into_bytes();
  if plus {
    expected_binding.extend_from_slice(&exporter.unwrap_or_default());
  }

  let auth_message = format!("{},{},{}", bare, server_first, without_proof);
  let salted = hash.hi(cfg.password.as_bytes(), salt, 4096, &std::sync::atomic::AtomicBool::new(false)).unwrap();
  let client_key = hash.hmac(&salted, b"Client Key");
  let signature = hash.hmac(&hash.hash(&client_key), auth_message.as_bytes());
  let expected_proof: Vec<u8> = client_key.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
  record.scram_ok = binding == expected_binding && B64.decode(proof).unwrap() == expected_proof;
  if !record.scram_ok {
    link.write(&fail("Invalid channel binding or proof")).await.unwrap();
    return false;
  }
  let server_signature = hash.hmac(&hash.hmac(&salted, b"Server Key"), auth_message.as_bytes());
  link.write(&format!("<success xmlns='{}'>{}</success>", NS_SASL, B64.encode(format!("v={}", B64.encode(server_signature))))).await.unwrap();
  true
}

/// Serve the bound session: bind, session, roster, MUC joins, messages.
async fn serve_session<S: XmppStream>(link: &mut Link<S>, record: &mut Record) {
  loop {
    let stanza = match link.event().await {
      Ok(Event::Stanza(s)) => s,
      Ok(Event::StreamClose) => {
        record.closed_stream = true;
        let _ = link.write("</stream:stream>").await;
        return;
      }
      _ => return,
    };
    let id = stanza.attr("id").unwrap_or("").to_string();
    if stanza.is("iq") && stanza.child("bind").is_some() {
      let reply = format!("<iq type='result' id='{}'><bind xmlns='{}'><jid>me@example.com/srv1</jid></bind></iq>", id, NS_BIND);
      link.write(&reply).await.unwrap();
    } else if stanza.is("iq") {
      link.write(&format!("<iq type='result' id='{}'/>", id)).await.unwrap();
    } else if stanza.is("presence")
      && let Some(to) = stanza.attr("to")
    {
      let room = to.split('/').next().unwrap();
      let reply = format!(
        "<presence from='{to}'><x xmlns='{NS_MUC_USER}'><item affiliation='member' role='participant'/><status code='110'/></x></presence><message from='{room}' type='groupchat'><subject>Topic</subject></message>"
      );
      link.write(&reply).await.unwrap();
    }
    record.stanzas.push(stanza);
  }
}

/// Accept the next connection that opens an XMPP stream or a TLS
/// handshake. Other local processes probe random ports, so anything else
/// is dropped.
async fn accept_client(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
  loop {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut first = [0u8; 1];
    if let Ok(Ok(1)) = tokio::time::timeout(std::time::Duration::from_secs(5), tcp.peek(&mut first)).await
      && matches!(first[0], b'<' | 0x16)
    {
      return tcp;
    }
  }
}

/// A scripted XMPP server for one connection.
async fn xmpp_server(cfg: ServerConfig) -> (u16, tokio::task::JoinHandle<Record>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  let handle = tokio::spawn(async move {
    let mut record = Record::default();
    let tcp = accept_client(&listener).await;
    let mechanisms: String = cfg.mechanisms.iter().map(|m| format!("<mechanism>{}</mechanism>", m)).collect();
    let cb = cfg
      .cb_types
      .as_ref()
      .map(|t| {
        format!(
          "<sasl-channel-binding xmlns='{}'>{}</sasl-channel-binding>",
          NS_SASL_CB,
          t.iter().map(|t| format!("<channel-binding type='{}'/>", t)).collect::<String>()
        )
      })
      .unwrap_or_default();
    let auth_features = format!("<mechanisms xmlns='{}'>{}</mechanisms>{}", NS_SASL, mechanisms, cb);

    let tls = if cfg.direct_tls {
      true
    } else if cfg.starttls {
      let mut plain = Link::new(tcp);
      header(&mut plain, &mut record, &format!("<starttls xmlns='{}'><required/></starttls>", NS_TLS)).await;
      let Ok(starttls) = plain.stanza().await else { return record };
      assert!(starttls.is("starttls"));
      plain.write(&format!("<proceed xmlns='{}'/>", NS_TLS)).await.unwrap();
      return serve_tls(plain.stream, cfg, record, &auth_features).await;
    } else {
      false
    };
    if tls {
      return serve_tls(tcp, cfg, record, &auth_features).await;
    }

    // Plaintext: the client must not authenticate
    let mut link = Link::new(tcp);
    header(&mut link, &mut record, &auth_features).await;
    if let Ok(Event::Stanza(s)) = link.event().await {
      record.mechanism = s.attr("mechanism").unwrap_or("").to_string();
    }
    record
  });
  (port, handle)
}

async fn serve_tls(tcp: TcpStream, cfg: ServerConfig, mut record: Record, auth_features: &str) -> Record {
  let Ok(tls) = crate::utils::socket::test_tls_acceptor().accept(tcp).await else { return record };
  record.sni = tls.get_ref().1.server_name().map(str::to_string);
  let exporter = tls.get_ref().1.export_keying_material([0u8; 32], b"EXPORTER-Channel-Binding", None).ok().map(|b| b.to_vec());
  let log = Arc::new(Mutex::new(Vec::new()));
  let mut link = Link::new(Tap { inner: tls, log: log.clone() });
  header(&mut link, &mut record, auth_features).await;
  if !server_auth(&mut link, &cfg, &mut record, exporter).await {
    return record;
  }
  let session = if cfg.session_required { format!("<session xmlns='{}'/>", NS_SESSION) } else { String::new() };
  header(&mut link, &mut record, &format!("{}{}", FEATURES_BIND, session)).await;
  log.lock().unwrap().clear();
  serve_session(&mut link, &mut record).await;
  record.raw = String::from_utf8_lossy(&log.lock().unwrap()).into_owned();
  record
}

fn ctx(title: &str, body: &str) -> NotifyContext {
  NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
}

fn names(record: &Record) -> Vec<String> {
  record.stanzas.iter().map(|s| s.local().to_string()).collect()
}

#[tokio::test]
async fn test_send_starttls_scram_plus() {
  let (port, server) = xmpp_server(ServerConfig::default()).await;
  // The connection goes to ?xmpp=, while the stream and TLS name the JID's
  // domain
  let svc = from_url(&format!("xmpps://me:secret@example.com:{}/a@example.com?xmpp=127.0.0.1&verify=no", port)).unwrap();
  assert!(svc.send(&ctx("Title", "Body & <more>")).await.unwrap());

  let record = server.await.unwrap();
  assert_eq!(record.stream_to, vec!["example.com"; 3]);
  assert_eq!(record.sni.as_deref(), Some("example.com"));
  assert_eq!(record.mechanism, "SCRAM-SHA-256-PLUS");
  assert_eq!(record.gs2_flag, "p=tls-exporter");
  assert!(record.scram_ok, "the server could not verify the proof and channel binding");
  assert_eq!(names(&record), vec!["iq", "presence", "message"]);

  // Bind without a resource of our own, then presence and the message
  let bind = &record.stanzas[0];
  assert_eq!(bind.attr("type"), Some("set"));
  assert_eq!(bind.child_ns("bind", NS_BIND).unwrap().children.len(), 0);
  assert!(record.raw.contains("<presence id='"), "{}", record.raw);
  let message = &record.stanzas[2];
  assert_eq!((message.attr("to"), message.attr("type")), (Some("a@example.com"), Some("chat")));
  assert_eq!(message.attr("id").unwrap().len(), 32);
  assert_eq!(message.child("body").unwrap().text, "Title\r\nBody & <more>");
  assert!(message.child("subject").is_none());
  assert!(record.raw.contains("<body>Title\r\nBody &amp; &lt;more&gt;</body></message>"), "{}", record.raw);
  assert!(record.closed_stream);
}

#[tokio::test]
async fn test_send_scramplus_disabled() {
  let (port, server) = xmpp_server(ServerConfig::default()).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no&scramplus=no", port)).unwrap();
  assert!(svc.send(&ctx("", "Body")).await.unwrap());
  let record = server.await.unwrap();
  assert_eq!(record.mechanism, "SCRAM-SHA-256");
  // The server offered channel binding and we did not use it: 'n', since
  // 'y' would make it fail the login as a downgrade
  assert_eq!(record.gs2_flag, "n");
  assert!(record.scram_ok);
}

#[tokio::test]
async fn test_send_scram_without_plus_offered() {
  // Channel binding is possible but the server offers no -PLUS: 'y'
  let cfg = ServerConfig { mechanisms: vec!["SCRAM-SHA-1", "PLAIN"], ..Default::default() };
  let (port, server) = xmpp_server(cfg).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(svc.send(&ctx("", "Body")).await.unwrap());
  let record = server.await.unwrap();
  assert_eq!((record.mechanism.as_str(), record.gs2_flag.as_str()), ("SCRAM-SHA-1", "y"));
  assert!(record.scram_ok);

  // XEP-0440 without tls-exporter rules out SCRAM-PLUS
  let cfg = ServerConfig { cb_types: Some(vec!["tls-server-end-point"]), ..Default::default() };
  let (port, server) = xmpp_server(cfg).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(svc.send(&ctx("", "Body")).await.unwrap());
  let record = server.await.unwrap();
  assert_eq!((record.mechanism.as_str(), record.gs2_flag.as_str()), ("SCRAM-SHA-256", "n"));
}

#[tokio::test]
async fn test_send_direct_tls_plain_with_options() {
  let cfg = ServerConfig { starttls: false, direct_tls: true, mechanisms: vec!["PLAIN"], session_required: true, ..Default::default() };
  let (port, server) = xmpp_server(cfg).await;
  let url = format!("xmpp://me%2Fphone:secret@127.0.0.1:{}/%23room@conference.example.com/bob?mode=tls&verify=no&subject=yes&roster=yes&name=Bot_1", port);
  let svc = from_url(&url).unwrap();
  assert_eq!(svc.title_maxlen(), 250);
  assert!(svc.send(&ctx("Title", "Body")).await.unwrap());

  let record = server.await.unwrap();
  assert_eq!(record.plain, b"\0me\0secret");
  assert_eq!(names(&record), vec!["iq", "iq", "presence", "iq", "presence", "message", "message"]);
  // Our own resource is requested
  assert_eq!(record.stanzas[0].child("bind").unwrap().child("resource").unwrap().text, "phone");
  assert!(record.stanzas[1].child_ns("session", NS_SESSION).is_some());
  assert!(record.stanzas[3].child_ns("query", NS_ROSTER).is_some());

  // Targets are sorted as upstream sorts them: '#room...' before 'bob'.
  // The room is joined before posting to it.
  let join = &record.stanzas[4];
  assert_eq!(join.attr("to"), Some("room@conference.example.com/Bot_1"));
  assert_eq!(join.child("show").unwrap().text, "chat");
  assert_eq!(join.child_ns("x", NS_MUC).unwrap().child("history").unwrap().attr("maxchars"), Some("9"));
  let groupchat = &record.stanzas[5];
  assert_eq!((groupchat.attr("to"), groupchat.attr("type")), (Some("room@conference.example.com"), Some("groupchat")));
  let chat = &record.stanzas[6];
  assert_eq!((chat.attr("to"), chat.attr("type")), (Some("bob@127.0.0.1"), Some("chat")));
  assert_eq!((chat.child("body").unwrap().text.as_str(), chat.child("subject").unwrap().text.as_str()), ("Body", "Title"));
  assert!(record.raw.contains("<body>Body</body><subject>Title</subject>"), "{}", record.raw);
}

#[tokio::test]
async fn test_send_without_targets_messages_self() {
  let (port, server) = xmpp_server(ServerConfig::default()).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}?verify=no", port)).unwrap();
  assert!(svc.send(&ctx("", "Body")).await.unwrap());
  let record = server.await.unwrap();
  assert_eq!(record.stanzas.last().unwrap().attr("to"), Some("me@127.0.0.1"));
}

#[tokio::test]
async fn test_send_keepalive_reuses_session() {
  let (port, server) = xmpp_server(ServerConfig::default()).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no&keepalive=yes", port)).unwrap();
  assert!(svc.send(&ctx("", "one")).await.unwrap());
  assert!(svc.send(&ctx("", "two")).await.unwrap());
  drop(svc);

  let record = server.await.unwrap();
  // One login, one presence, two messages on the same stream
  assert_eq!(names(&record), vec!["iq", "presence", "message", "message"]);
  assert_eq!(record.stanzas[3].child("body").unwrap().text, "two");
  assert!(!record.closed_stream);
}

#[tokio::test]
async fn test_send_authentication_failures() {
  let cfg = ServerConfig { failure: Some("Invalid channel binding"), ..Default::default() };
  let (port, server) = xmpp_server(cfg).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert!(server.await.unwrap().stanzas.is_empty());

  let (port, server) = xmpp_server(ServerConfig { mechanisms: vec!["PLAIN"], ..Default::default() }).await;
  let svc = from_url(&format!("xmpps://me:wrong@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert!(server.await.unwrap().stanzas.is_empty());

  // No mechanism we can use
  let (port, _server) = xmpp_server(ServerConfig { mechanisms: vec!["DIGEST-MD5"], ..Default::default() }).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
}

#[tokio::test]
async fn test_send_refuses_plaintext_login() {
  // xmpp:// is plaintext; credentials are never sent in the clear
  let (port, server) = xmpp_server(ServerConfig { starttls: false, ..Default::default() }).await;
  let svc = from_url(&format!("xmpp://me:secret@127.0.0.1:{}/a", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert_eq!(server.await.unwrap().mechanism, "");

  // STARTTLS mode against a server that does not offer it
  let (port, server) = xmpp_server(ServerConfig { starttls: false, ..Default::default() }).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert_eq!(server.await.unwrap().mechanism, "");
}

#[tokio::test]
async fn test_send_tls_verification() {
  // The self-signed test certificate fails verification
  let (port, server) = xmpp_server(ServerConfig::default()).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert!(server.await.unwrap().mechanism.is_empty());

  // Port 1 is privileged, so no test can be listening on it; a port
  // freed by dropping a listener may be taken by another test meanwhile.
  let port = 1;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a", port)).unwrap();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
}

/// The server picks the SCRAM iteration count. With 2^32 - 1 the key
/// derivation used to run on the async task, so the send blocked far past
/// its timeout; it must give up at the timeout and stop deriving.
#[test]
fn test_send_scram_iteration_count_bounded_by_timeout() {
  let (tx, rx) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let sent = runtime.block_on(async {
      let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
      let port = listener.local_addr().unwrap().port();
      let server = tokio::spawn(async move {
        let tcp = accept_client(&listener).await;
        let mut link = Link::new(crate::utils::socket::test_tls_acceptor().accept(tcp).await.unwrap());
        header(&mut link, &mut Record::default(), &format!("<mechanisms xmlns='{}'><mechanism>SCRAM-SHA-1</mechanism></mechanisms>", NS_SASL)).await;
        let auth = link.stanza().await.unwrap();
        let client_first = String::from_utf8(B64.decode(auth.text.trim()).unwrap()).unwrap();
        let nonce = client_first.split("r=").nth(1).unwrap().to_string();
        let server_first = format!("r={}server,s={},i={}", nonce, B64.encode("salt"), u32::MAX);
        link.write(&format!("<challenge xmlns='{}'>{}</challenge>", NS_SASL, B64.encode(server_first))).await.unwrap();
        // Hold the connection until the client gives up
        let _ = link.event().await;
      });
      let mut svc = parse(&format!("xmpps://me:secret@127.0.0.1:{}/a?mode=tls&verify=no", port)).unwrap();
      svc.timeout = Duration::from_millis(300);
      let sent = svc.send(&ctx("", "Body")).await.unwrap();
      server.abort();
      sent
    });
    tx.send(Some(sent)).unwrap();
    // Dropping a runtime waits for its blocking threads, so this returns
    // only once the derivation stopped
    drop(runtime);
    tx.send(None).unwrap();
  });
  assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Some(false)), "the send did not give up at its timeout");
  assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(None), "the key derivation kept running after the send gave up");
}

/// A server that answers the end of our stream with stanza after stanza,
/// never closing its side: close() must still give up at its deadline
/// (upstream's disconnect waits a fixed time).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_close_deadline_holds_while_stanzas_arrive() {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let server = tokio::spawn(async move {
    loop {
      let (tcp, _) = listener.accept().await.unwrap();
      let mut link = Link::new(tcp);
      // Skip stray local connections that do not speak XMPP
      if !matches!(link.event().await, Ok(Event::StreamOpen(_))) {
        continue;
      }
      let header = "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' id='s1' from='example.com' version='1.0'>";
      link.write(header).await.unwrap();
      while !matches!(link.event().await, Ok(Event::StreamClose) | Err(_)) {}
      let burst = "<presence from='flood@example.com/x'/>".repeat(200);
      while link.write(&burst).await.is_ok() {}
      return;
    }
  });
  let mut client = Link::new(TcpStream::connect(addr).await.unwrap());
  client.write("<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' to='example.com' version='1.0'>").await.unwrap();
  assert!(matches!(client.event().await, Ok(Event::StreamOpen(_))));
  let start = Instant::now();
  let closed = tokio::time::timeout(Duration::from_secs(10), client.close()).await;
  assert!(closed.is_ok() && start.elapsed() < CLOSE_TIMEOUT + Duration::from_secs(1), "took {:?}", start.elapsed());
  server.abort();
}

#[test]
fn test_static_details() {
  let details = Xmpp::static_details();
  assert_eq!(details.service_name, "XMPP");
  assert_eq!(details.protocols, vec!["xmpp", "xmpps"]);
  assert!(!details.attachment_support);
}

/// SASL payloads stay out of the log: the PLAIN one is the password in
/// base64. Upstream (slixmpp) logs the stanzas whole at DEBUG.
#[test]
fn test_loggable_hides_sasl_payloads() {
  let auth = format!("<auth xmlns='{}' mechanism='PLAIN'>AG1lAHNlY3JldA==</auth>", NS_SASL);
  assert_eq!(loggable(&auth), format!("<auth xmlns='{}' mechanism='PLAIN'>[16 bytes]</auth>", NS_SASL));
  for name in ["response", "challenge", "success"] {
    let stanza = format!("<{0} xmlns='{1}'>cj1hYmMscD14eXo=</{0}>", name, NS_SASL);
    assert_eq!(loggable(&stanza), format!("<{0} xmlns='{1}'>[16 bytes]</{0}>", name, NS_SASL));
  }
  // An empty response or success has nothing to hide
  assert_eq!(loggable(&format!("<response xmlns='{}'></response>", NS_SASL)), format!("<response xmlns='{}'>[0 bytes]</response>", NS_SASL));
  assert_eq!(loggable(&format!("<success xmlns='{}'/>", NS_SASL)), format!("<success xmlns='{}'/>", NS_SASL));
  // A prefixed or unfinished element is still cut
  assert_eq!(loggable("<sasl:auth mechanism='PLAIN'>AG1lAHNlY3JldA==</sasl:auth>"), "<sasl:auth mechanism='PLAIN'>[16 bytes]</sasl:auth>");
  assert_eq!(loggable("<auth mechanism='PLAIN'>AG1lAHNl"), "<auth mechanism='PLAIN'>[8 bytes]</auth>");
  assert_eq!(loggable("<auth mechanism='PLAIN' AG1lAHNl"), "<auth [31 bytes]");
  // Everything else is logged as it is
  for other in ["<message to='a'><body>hi</body></message>", "<authority/>", "<presence/>", "</stream:stream>", "plain text"] {
    assert_eq!(loggable(other), other);
  }
}

/// At trace level, a PLAIN login logs the auth element and its mechanism
/// but never the payload.
#[tokio::test]
async fn test_trace_log_hides_plain_payload() {
  #[derive(Clone, Default)]
  struct Buffer(Arc<Mutex<Vec<u8>>>);
  impl std::io::Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self.0.lock().unwrap().extend_from_slice(buf);
      Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  let buffer = Buffer::default();
  let writer = buffer.clone();
  let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_ansi(false).with_writer(move || writer.clone()).finish();
  let _guard = tracing::subscriber::set_default(subscriber);

  let (port, server) = xmpp_server(ServerConfig { mechanisms: vec!["PLAIN"], ..Default::default() }).await;
  let svc = from_url(&format!("xmpps://me:secret@127.0.0.1:{}/a?verify=no", port)).unwrap();
  assert!(svc.send(&ctx("", "Body")).await.unwrap());
  assert_eq!(server.await.unwrap().plain, b"\0me\0secret");

  let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
  assert!(logs.contains(&format!("XMPP SEND: <auth xmlns='{}' mechanism='PLAIN'>[16 bytes]</auth>", NS_SASL)), "{}", logs);
  assert!(!logs.contains("AG1lAHNlY3JldA=="), "{}", logs);
  assert!(!logs.contains("secret"), "{}", logs);
}

/// A server whose stream features never end: the send fails once they pass
/// the stanza limit, well before its timeout, instead of buffering all the
/// server sends. Upstream (slixmpp over expat) keeps all of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_send_fails_on_endless_stanza() {
  use super::xml::MAX_STANZA_LEN;
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  let server = tokio::spawn(async move {
    let tcp = accept_client(&listener).await;
    let mut link = Link::new(tcp);
    assert!(matches!(link.event().await, Ok(Event::StreamOpen(_))));
    link.write("<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' version='1.0'><stream:features>").await.unwrap();
    // Features that never end
    let chunk = "x".repeat(64 * 1024);
    let mut sent = 0usize;
    while link.stream.write_all(chunk.as_bytes()).await.is_ok() && sent < 64 * MAX_STANZA_LEN {
      sent += chunk.len();
    }
    sent
  });
  let mut svc = parse(&format!("xmpp://me:secret@127.0.0.1:{}/a?mode=none", port)).unwrap();
  svc.timeout = Duration::from_secs(20);
  let start = Instant::now();
  assert!(!svc.send(&ctx("", "Body")).await.unwrap());
  assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
  // The client hung up after about one stanza's worth, plus what the
  // socket buffers held
  let sent = server.await.unwrap();
  assert!(sent < 16 * MAX_STANZA_LEN, "the server sent {} bytes", sent);
}

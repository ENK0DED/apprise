//! The relay protocol, against an in-memory relay (upstream
//! `test_plugin_nostr_relay_*` with its `FakeTransport`), and over real TCP
//! and TLS to a local fake relay.

use super::*;
use crate::notify::nostr::crypto::verify_event;
use serde_json::json;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

fn sender() -> NostrKey {
  NostrKey::from_secret(&[0x11; 32]).unwrap()
}

/// An unmasked frame as a relay would send it.
pub(super) fn frame(payload: &[u8], opcode: u8, fin: bool) -> Vec<u8> {
  let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
  match payload.len() {
    n if n < 126 => out.push(n as u8),
    n if n < 65536 => {
      out.push(126);
      out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    n => {
      out.push(127);
      out.extend_from_slice(&(n as u64).to_be_bytes());
    }
  }
  out.extend_from_slice(payload);
  out
}

/// A text frame holding `value` as JSON.
pub(super) fn text(value: Value) -> Vec<u8> {
  frame(value.to_string().as_bytes(), 0x1, true)
}

/// Unmask one client frame: its opcode and payload, and how long it was.
/// Every frame a client sends must be masked.
pub(super) fn unmask(data: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
  if data.len() < 2 {
    return None;
  }
  assert!(data[1] & 0x80 != 0, "client frames must be masked");
  let short = (data[1] & 0x7F) as usize;
  let offset = match short {
    126 => 4,
    127 => 10,
    _ => 2,
  };
  if data.len() < offset + 4 {
    return None;
  }
  let length = if offset > 2 { data[2..offset].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize) } else { short };
  let mask = &data[offset..offset + 4];
  let body = data.get(offset + 4..offset + 4 + length)?;
  Some((data[0] & 0x0F, body.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect(), offset + 4 + length))
}

/// The answer to a websocket upgrade request with `key`.
pub(super) fn accept_for(request: &[u8]) -> String {
  let text = String::from_utf8_lossy(request);
  let key = text.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().trim().to_string();
  B64.encode(Sha1::digest(format!("{}{}", key, WS_ACCEPT_GUID).as_bytes()))
}

type Responder = Box<dyn FnMut(&Value) -> Vec<Vec<u8>> + Send>;

/// What the in-memory relay saw and will hand back.
#[derive(Default)]
struct FakeState {
  /// Bytes the relay will hand back
  inbox: Vec<u8>,
  /// Bytes we sent that are not yet a complete frame
  written: Vec<u8>,
  /// Every (opcode, payload) we sent, and the JSON of each text frame
  frames: Vec<(u8, Vec<u8>)>,
  messages: Vec<Value>,
  /// The websocket upgrade request
  request: Vec<u8>,
  closed: bool,
  /// Fail every write from now on
  broken: bool,
  waker: Option<Waker>,
}

/// Plays the relay side of a websocket connection.
#[derive(Clone)]
struct FakeTransport {
  state: Arc<Mutex<FakeState>>,
  responder: Arc<Mutex<Responder>>,
  /// Frames sent right after the handshake, and a replacement status
  greeting: Vec<u8>,
  status: Option<Vec<u8>>,
  /// How many bytes each read hands back
  chunk: usize,
}

impl FakeTransport {
  fn new(responder: impl FnMut(&Value) -> Vec<Vec<u8>> + Send + 'static) -> Self {
    Self { state: Arc::default(), responder: Arc::new(Mutex::new(Box::new(responder))), greeting: Vec::new(), status: None, chunk: 65536 }
  }

  fn silent() -> Self {
    Self::new(|_| Vec::new())
  }

  fn greeting(mut self, greeting: Vec<u8>) -> Self {
    self.greeting = greeting;
    self
  }

  fn status(mut self, status: &[u8]) -> Self {
    self.status = Some(status.to_vec());
    self
  }

  fn messages(&self) -> Vec<Value> {
    self.state.lock().unwrap().messages.clone()
  }

  fn kinds(&self) -> Vec<String> {
    self.messages().iter().map(|m| m[0].as_str().unwrap().to_string()).collect()
  }

  fn frames(&self) -> Vec<(u8, Vec<u8>)> {
    self.state.lock().unwrap().frames.clone()
  }

  fn request(&self) -> String {
    String::from_utf8(self.state.lock().unwrap().request.clone()).unwrap()
  }

  fn closed(&self) -> bool {
    self.state.lock().unwrap().closed
  }

  fn absorb(&self, state: &mut FakeState) {
    if state.request.is_empty() {
      // Answer the websocket upgrade
      let Some(end) = state.written.windows(4).position(|w| w == b"\r\n\r\n") else { return };
      state.request = state.written.drain(..end + 4).collect();
      let answer = match &self.status {
        Some(status) => status.clone(),
        None => format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n", accept_for(&state.request)).into_bytes(),
      };
      state.inbox.extend(answer);
      state.inbox.extend(&self.greeting);
    }
    while let Some((opcode, payload, used)) = unmask(&state.written) {
      state.written.drain(..used);
      if opcode == 0x1 {
        let message: Value = serde_json::from_slice(&payload).unwrap();
        state.messages.push(message.clone());
        for reply in (self.responder.lock().unwrap())(&message) {
          state.inbox.extend(reply);
        }
      }
      state.frames.push((opcode, payload));
    }
  }
}

impl AsyncWrite for FakeTransport {
  fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
    let mut state = self.state.lock().unwrap();
    if state.broken {
      return Poll::Ready(Err(std::io::Error::other("broken")));
    }
    state.written.extend_from_slice(data);
    self.absorb(&mut state);
    if let Some(waker) = state.waker.take() {
      waker.wake();
    }
    Poll::Ready(Ok(data.len()))
  }

  fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Poll::Ready(Ok(()))
  }

  fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    self.state.lock().unwrap().closed = true;
    Poll::Ready(Ok(()))
  }
}

impl AsyncRead for FakeTransport {
  fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<std::io::Result<()>> {
    let mut state = self.state.lock().unwrap();
    if state.inbox.is_empty() {
      // A relay that has nothing to say keeps us waiting
      state.waker = Some(cx.waker().clone());
      return Poll::Pending;
    }
    let size = self.chunk.min(buf.remaining()).min(state.inbox.len());
    buf.put_slice(&state.inbox[..size]);
    state.inbox.drain(..size);
    Poll::Ready(Ok(()))
  }
}

fn options(timeout: Duration) -> RelayOptions {
  RelayOptions { verify: true, connect_timeout: timeout, timeout, user_agent: crate::notify::APP_ID.to_string(), public_only: false }
}

/// A relay at `url` that talks to `transport`.
async fn relay(transport: &FakeTransport, url: &str, timeout: Duration) -> Result<NostrRelay, RelayError> {
  NostrRelay::handshake(url, &sender(), Box::new(transport.clone()), &RelayAddress::parse(url).unwrap(), &options(timeout)).await
}

const SECOND: Duration = Duration::from_secs(1);

/// JSON nested far deeper than a parser will follow.
fn nested() -> Vec<u8> {
  let mut out = vec![b'['; 100000];
  out.extend(vec![b']'; 100000]);
  out
}

#[test]
fn test_relay_address() {
  let parse = |u: &str| RelayAddress::parse(u).unwrap();
  let a = parse("wss://Relay.Example.com:8443/n?x=1");
  assert_eq!(
    (a.secure, a.host.as_str(), a.port, a.path.as_str(), a.host_header.as_str()),
    (true, "relay.example.com", 8443, "/n?x=1", "relay.example.com:8443")
  );
  let a = parse("ws://relay.example.com");
  assert_eq!((a.secure, a.port, a.path.as_str(), a.host_header.as_str()), (false, 80, "/", "relay.example.com"));
  let a = parse("wss://10.0.0.1");
  assert_eq!((a.port, a.host.as_str()), (443, "10.0.0.1"));
  assert!(RelayAddress::parse("wss://relay.example.com:99999").is_none());
  assert!(RelayAddress::parse("relay.example.com").is_none());
}

/// NostrRelay publishes, logging in when the relay asks for it.
#[tokio::test]
async fn test_relay_publish() {
  let authenticated = Arc::new(Mutex::new(Vec::<Value>::new()));
  let auth = authenticated.clone();
  let greeting = [
    text(json!(["AUTH", "challenge-1"])),
    frame(b"ping!", 0x9, true),
    frame(b"pong", 0xA, true),
    frame(b"not json", 0x1, true),
    text(json!({"not": "a list"})),
    text(json!([])),
    text(json!(["NOTICE", "x".repeat(70000)])),
    frame(&nested(), 0x1, true),
    frame(br#"["NOTICE","#, 0x1, false),
    frame(br#""split"]"#, 0x0, true),
  ]
  .concat();
  let mut transport = FakeTransport::new(move |m| match m[0].as_str() {
    Some("EVENT") if auth.lock().unwrap().is_empty() => {
      vec![text(json!(["OK", "other", true, ""])), text(json!(["OK", m[1]["id"], false, "auth-required: hi"]))]
    }
    Some("EVENT") => vec![text(json!(["OK", m[1]["id"], true]))],
    Some("AUTH") => {
      auth.lock().unwrap().push(m[1].clone());
      vec![text(json!(["OK", m[1]["id"], true, ""]))]
    }
    _ => Vec::new(),
  })
  .greeting(greeting);
  transport.chunk = 7;
  let event = sender().sign(1, &"y".repeat(300), Vec::new(), None);
  let mut r = relay(&transport, "wss://relay.example.com:8443/n?x=1", SECOND).await.unwrap();
  assert_eq!(r.publish(&event).await.unwrap(), (true, String::new()));
  r.close().await;

  // The handshake named the right host and path
  let request = transport.request();
  assert!(request.starts_with("GET /n?x=1 HTTP/1.1\r\n"), "{request}");
  assert!(request.contains("Host: relay.example.com:8443\r\n"));
  assert!(request.contains(&format!("User-Agent: {}\r\n", crate::notify::APP_ID)));
  assert!(request.contains("Upgrade: websocket\r\nConnection: Upgrade\r\n"));
  assert!(request.contains("Sec-WebSocket-Version: 13\r\n\r\n"));

  // We answered the ping, logged in, then resent the event
  assert!(transport.frames().contains(&(0xA, b"ping!".to_vec())));
  assert_eq!(transport.kinds(), ["EVENT", "AUTH", "EVENT"]);
  let auth = authenticated.lock().unwrap()[0].clone();
  assert_eq!(auth["kind"], 22242);
  let tags = auth["tags"].as_array().unwrap();
  assert!(tags.contains(&json!(["challenge", "challenge-1"])));
  assert!(tags.contains(&json!(["relay", "wss://relay.example.com:8443/n?x=1"])));
  assert!(verify_event(&auth).is_some());
  // The event went out exactly as signed, in upstream's field order
  assert_eq!(transport.frames()[0].1, serde_json::to_vec(&("EVENT", &event)).unwrap());

  // We said goodbye
  assert_eq!(transport.frames().last().unwrap(), &(0x8, vec![0x03, 0xe8]));
  assert!(transport.closed());

  // Large events use the longest length field
  let transport = FakeTransport::new(|m| vec![text(json!(["OK", m[1]["id"], true, ""]))]);
  let mut r = relay(&transport, "ws://relay.example.com", SECOND).await.unwrap();
  assert!(r.publish(&sender().sign(1, &"z".repeat(70000), Vec::new(), None)).await.unwrap().0);
  assert!(transport.request().contains("Host: relay.example.com\r\n"));
  assert_eq!(transport.messages()[0][1]["content"].as_str().unwrap().len(), 70000);
}

/// NostrRelay gives up when logging in does not help.
#[tokio::test]
async fn test_relay_auth_refused() {
  // The relay refuses our login, and we only ever try to log in once
  let transport = FakeTransport::new(|m| vec![text(json!(["OK", m[1]["id"], false, "auth-required: no"]))]).greeting(text(json!(["AUTH", "c"])));
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap(), (false, "auth-required: no".to_string()));
  assert!(!r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap().0);
  assert_eq!(transport.kinds(), ["EVENT", "AUTH", "EVENT"]);

  // No challenge was sent, so we can not log in at all
  let transport = FakeTransport::new(|m| vec![text(json!(["OK", m[1]["id"], false, "auth-required: no"]))]);
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  assert!(!r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap().0);
  assert_eq!(transport.kinds(), ["EVENT"]);

  // A reason that is not text is reported as Python would
  let transport = FakeTransport::new(|m| vec![text(json!(["OK", m[1]["id"], 1, null]))]);
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap(), (false, "None".to_string()));
}

/// NostrRelay collects stored events until the relay is done.
#[tokio::test]
async fn test_relay_query() {
  let stored = serde_json::to_value(NostrKey::from_secret(&[0x22; 32]).unwrap().sign(10050, "", Vec::new(), None)).unwrap();
  let seen = Arc::new(Mutex::new(0));
  let (count, event) = (seen.clone(), stored.clone());
  let transport = FakeTransport::new(move |m| match m[0].as_str() {
    Some("REQ") => {
      *count.lock().unwrap() += 1;
      if *count.lock().unwrap() == 1 {
        return vec![text(json!(["CLOSED", m[1], "auth-required: log in"]))];
      }
      vec![
        text(json!(["EVENT", "someone-else", event])),
        text(json!(["NOTICE"])),
        text(json!(["EVENT", m[1], "junk"])),
        text(json!(["EVENT", m[1], event])),
        text(json!(["EOSE", m[1]])),
      ]
    }
    Some("AUTH") => vec![text(json!(["OK", m[1]["id"], true, ""]))],
    _ => Vec::new(),
  })
  .greeting(text(json!(["AUTH", "c"])));
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  assert_eq!(r.query(&Filter { kinds: vec![10050], authors: None, limit: None }).await.unwrap(), vec![stored.clone()]);
  assert_eq!(transport.kinds(), ["REQ", "AUTH", "REQ", "CLOSE"]);
  // The subscription and its filter are written as upstream writes them
  let req = &transport.messages()[0];
  assert_eq!(req[1].as_str().unwrap().len(), 16);
  assert_eq!(req[2], json!({"kinds": [10050]}));
  assert_eq!(transport.messages()[3], json!(["CLOSE", req[1]]));

  // A subscription closed for another reason returns what it has
  let transport = FakeTransport::new(|m| if m[0] == "REQ" { vec![text(json!(["CLOSED", m[1]]))] } else { Vec::new() });
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  assert_eq!(r.query(&Filter { kinds: vec![1], authors: None, limit: None }).await.unwrap(), Vec::<Value>::new());

  // No more events are kept than were asked for
  let event = stored.clone();
  let transport =
    FakeTransport::new(move |m| if m[0] == "REQ" { vec![text(json!(["EVENT", m[1], event])), text(json!(["EVENT", m[1], event]))] } else { Vec::new() });
  let mut r = relay(&transport, "wss://relay.example.com", SECOND).await.unwrap();
  let filter = Filter { kinds: vec![10050], authors: Some(vec!["ab".repeat(32)]), limit: Some(1) };
  assert_eq!(r.query(&filter).await.unwrap(), vec![stored]);
  assert_eq!(transport.kinds(), ["REQ", "CLOSE"]);
  let sent = String::from_utf8(transport.frames()[0].1.clone()).unwrap();
  assert!(sent.ends_with(&format!(r#",{{"kinds":[10050],"authors":["{}"],"limit":1}}]"#, "ab".repeat(32))), "{sent}");
}

/// NostrRelay raises socket errors for broken connections.
#[tokio::test]
async fn test_relay_errors() {
  let url = "wss://relay.example.com";
  // Not upgraded to a websocket
  assert!(relay(&FakeTransport::silent().status(b"HTTP/1.1 403 Forbidden\r\n\r\n"), url, SECOND).await.is_err());

  // Upgraded, but without proof the server understood us
  assert!(relay(&FakeTransport::silent().status(b"HTTP/1.1 101 OK\r\nSec-WebSocket-Accept: nope\r\n\r\n"), url, SECOND).await.is_err());

  // The relay hangs up on us
  let transport = FakeTransport::new(|_| vec![frame(b"", 0x8, true)]);
  let mut r = relay(&transport, url, SECOND).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap_err().0, "The relay closed the connection.");

  // The relay never answers
  let mut r = relay(&FakeTransport::silent(), url, Duration::from_millis(50)).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap_err().0, "Timed out waiting for the relay.");

  // A handshake reply that never ends, or arrives too large all at once
  let mut endless = b"HTTP/1.1 101 OK\r\n".to_vec();
  endless.extend(vec![b'X'; 20000]);
  let mut large = endless.clone();
  large.extend(b"\r\n\r\n");
  for status in [endless, large] {
    let err = relay(&FakeTransport::silent().status(&status), url, SECOND).await.err().unwrap();
    assert_eq!(err.0, "Relay sent an oversized websocket handshake.");
  }

  // A frame that claims to be enormous is refused before it arrives
  let transport = FakeTransport::new(|_| vec![[vec![0x81, 0x7f], (1u64 << 40).to_be_bytes().to_vec()].concat()]);
  let mut r = relay(&transport, url, SECOND).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap_err().0, "Relay sent an oversized frame.");

  // So is a message split into many frames that adds up to too much
  let transport = FakeTransport::new(|_| {
    let mut frames = vec![frame(&vec![b'x'; 60000], 0x1, false)];
    frames.extend(std::iter::repeat_n(frame(&vec![b'x'; 60000], 0x0, false), 20));
    frames
  });
  let mut r = relay(&transport, url, SECOND).await.unwrap();
  assert_eq!(r.publish(&sender().sign(1, "x", Vec::new(), None)).await.unwrap_err().0, "Relay sent an oversized message.");

  // A failing goodbye still closes the socket
  let transport = FakeTransport::silent();
  let mut r = relay(&transport, url, SECOND).await.unwrap();
  transport.state.lock().unwrap().broken = true;
  r.close().await;
  assert!(transport.closed());
}

/// A relay from someone's list that only leads to private (or IPv6)
/// addresses is never connected to, while each public address is tried in
/// turn until one works.
#[tokio::test]
async fn test_relay_public_addresses() {
  let ips = |list: &[&str]| list.iter().map(|s| s.parse::<IpAddr>().unwrap()).collect::<Vec<_>>();
  let url = "wss://relay.example.com";
  let err = public_addresses(url, &ips(&["10.0.0.1", "127.0.0.1"])).unwrap_err();
  assert!(err.0.contains("no usable public address"), "{err}");
  let err = public_addresses(url, &ips(&["2606:4700::1"])).unwrap_err();
  assert!(err.0.contains("no public IPv4 address"), "{err}");

  // Duplicates are tried once, and only the first 4 are tried at all
  let many: Vec<String> = (1..=50).map(|n| format!("93.184.216.{n}")).collect();
  let mut listed = vec![many[0].as_str(), many[0].as_str(), "10.0.0.1", "2606:4700::1"];
  listed.extend(many.iter().map(String::as_str));
  assert_eq!(public_addresses(url, &ips(&listed)).unwrap(), ips(&many[..4].iter().map(String::as_str).collect::<Vec<_>>()));

  // A name that only leads to private addresses (looked up for real)
  let address = RelayAddress::parse("wss://localhost:1").unwrap();
  let options = RelayOptions { public_only: true, ..options(SECOND) };
  let err = connect_public(url, &address, &options).await.err().unwrap();
  assert!(err.0.contains("no usable public address"), "{err}");

  // Each address is tried in turn until one works
  let tried = Arc::new(Mutex::new(Vec::<(IpAddr, Duration)>::new()));
  let attempt = |refuse: Vec<IpAddr>, delay: Duration| {
    let tried = tried.clone();
    move |ip: IpAddr, remaining: Duration| {
      tried.lock().unwrap().push((ip, remaining));
      let refused = refuse.contains(&ip);
      async move {
        tokio::time::sleep(delay).await;
        if refused { Err(error("refused")) } else { Ok(ip) }
      }
    }
  };
  let candidates = ips(&["93.184.216.34", "93.184.216.35"]);
  let got = connect_in_turn(url, &candidates, SECOND, attempt(ips(&["93.184.216.34"]), Duration::ZERO)).await.unwrap();
  assert_eq!(got, candidates[1]);
  assert_eq!(tried.lock().unwrap().len(), 2);

  // Every attempt shares one connection timeout; when none of them work,
  // the last error is reported
  tried.lock().unwrap().clear();
  let four = ips(&many[..4].iter().map(String::as_str).collect::<Vec<_>>());
  let err = connect_in_turn(url, &four, SECOND, attempt(four.clone(), Duration::from_millis(5))).await.unwrap_err();
  assert_eq!(err.0, "refused");
  let timeouts: Vec<Duration> = tried.lock().unwrap().iter().map(|(_, t)| *t).collect();
  assert_eq!(timeouts.len(), 4);
  assert!(timeouts.iter().all(|t| !t.is_zero() && *t <= SECOND));
  assert!(timeouts.windows(2).all(|w| w[0] > w[1]), "{timeouts:?}");

  // Slow refusals use up the shared timeout, so later addresses are never
  // tried
  tried.lock().unwrap().clear();
  let err = connect_in_turn(url, &four, Duration::from_millis(50), attempt(four.clone(), Duration::from_millis(60))).await.unwrap_err();
  assert_eq!(err.0, format!("Timed out connecting to relay {url}."));
  assert_eq!(tried.lock().unwrap().len(), 1);
}

/// A fake relay on 127.0.0.1 that accepts every event, over plain TCP or
/// TLS; it hands back the frames it received from each connection.
pub(in crate::notify::nostr) async fn tcp_relay(tls: bool) -> (u16, tokio::sync::mpsc::UnboundedReceiver<Vec<Value>>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
  tokio::spawn(async move {
    while let Ok((tcp, _)) = listener.accept().await {
      let tx = tx.clone();
      tokio::spawn(async move {
        let stream: Box<dyn RelayStream> = if tls {
          match crate::utils::socket::test_tls_acceptor().accept(tcp).await {
            Ok(s) => Box::new(s),
            Err(_) => return,
          }
        } else {
          Box::new(tcp)
        };
        if let Some(messages) = serve(stream).await {
          let _ = tx.send(messages);
        }
      });
    }
  });
  (port, rx)
}

/// Speak the relay side of one websocket connection: accept every event
/// and answer every query with nothing. Connections that do not open with
/// a websocket upgrade (stray local traffic) are dropped.
async fn serve(mut stream: Box<dyn RelayStream>) -> Option<Vec<Value>> {
  let mut buf = Vec::new();
  let end = loop {
    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
      break end;
    }
    if stream.read_buf(&mut buf).await.ok()? == 0 {
      return None;
    }
  };
  let request: Vec<u8> = buf.drain(..end + 4).collect();
  if !request.starts_with(b"GET ") || !String::from_utf8_lossy(&request).contains("Sec-WebSocket-Key") {
    return None;
  }
  let answer =
    format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n", accept_for(&request));
  stream.write_all(answer.as_bytes()).await.ok()?;
  let mut messages = Vec::new();
  loop {
    while let Some((opcode, payload, used)) = unmask(&buf) {
      buf.drain(..used);
      match opcode {
        0x8 => return Some(messages),
        0x1 => {
          let message: Value = serde_json::from_slice(&payload).ok()?;
          let reply = match message[0].as_str() {
            Some("EVENT") => json!(["OK", message[1]["id"], true, ""]),
            Some("REQ") => json!(["EOSE", message[1]]),
            _ => Value::Null,
          };
          if !reply.is_null() {
            stream.write_all(&text(reply)).await.ok()?;
          }
          messages.push(message);
        }
        _ => {}
      }
    }
    if stream.read_buf(&mut buf).await.ok()? == 0 {
      return Some(messages);
    }
  }
}

/// A real connection, over TCP and over TLS (with ?verify=no for the test
/// certificate), publishes and queries.
#[tokio::test]
async fn test_relay_over_tcp_and_tls() {
  for tls in [false, true] {
    let (port, mut received) = tcp_relay(tls).await;
    let url = format!("{}://127.0.0.1:{}", if tls { "wss" } else { "ws" }, port);
    let opts = RelayOptions { verify: false, ..options(Duration::from_secs(5)) };
    let mut r = NostrRelay::connect(&url, &sender(), &opts).await.unwrap();
    let event = sender().sign(1, "over the wire", Vec::new(), None);
    assert_eq!(r.publish(&event).await.unwrap(), (true, String::new()));
    assert_eq!(r.query(&Filter { kinds: vec![1], authors: None, limit: Some(1) }).await.unwrap(), Vec::<Value>::new());
    r.close().await;
    let messages = received.recv().await.unwrap();
    assert_eq!(messages[0], json!(["EVENT", serde_json::to_value(&event).unwrap()]));
    assert_eq!(messages.iter().map(|m| m[0].as_str().unwrap()).collect::<Vec<_>>(), ["EVENT", "REQ", "CLOSE"]);
  }

  // With verification on, the test certificate is refused
  let (port, _received) = tcp_relay(true).await;
  let err = NostrRelay::connect(&format!("wss://localhost:{port}"), &sender(), &options(Duration::from_secs(5))).await.err().unwrap();
  assert!(err.0.contains("TLS negotiation failed"), "{err}");

  // Nothing listening
  let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
  assert!(NostrRelay::connect(&format!("ws://127.0.0.1:{closed}"), &sender(), &options(SECOND)).await.is_err());
}

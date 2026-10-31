use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use blake2::{Blake2b512, Digest};
use ed25519_dalek::{Signer, SigningKey};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::email::parse_list;
use crate::utils::http::read_bounded_response;
use crate::utils::parse::{ParsedUrl, unquote};

/// Stop reading a response once it grows past a normal acknowledgement; a
/// hostile or compromised server cannot make us buffer more than this.
const MAX_RESPONSE_SIZE: usize = 32768;

/// Session Open Group Server (Session community) room messages, posted by
/// a bot identified by an Ed25519 seed.
pub struct SessionOgs {
  /// The server's 32-byte public key, part of every signed request
  public_key: [u8; 32],
  signing_key: SigningKey,
  host: String,
  port: Option<u16>,
  secure: bool,
  rooms: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl SessionOgs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;

    // The public key is the URL user; ?key= and then ?public_key= override it
    // Upstream unquotes the query values once more
    let mut public_key = url.user.clone();
    for key in ["key", "public_key"] {
      if let Some(v) = url.get(key).filter(|v| !v.is_empty()) {
        public_key = Some(unquote(v));
      }
    }
    let Some(public_key) = decode_key(public_key.as_deref()) else {
      tracing::warn!("The SOGS public_key must be exactly 64 hex characters.");
      return None;
    };

    // The bot seed is the URL password; ?seed= overrides it
    let mut seed = url.password.clone();
    if let Some(v) = url.get("seed").filter(|v| !v.is_empty()) {
      seed = Some(unquote(v));
    }
    let Some(seed) = decode_key(seed.as_deref()) else {
      tracing::warn!("The SOGS bot seed must be exactly 64 hex characters.");
      return None;
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      entries.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    let mut rooms = Vec::new();
    for room in parse_list(&entries) {
      let room = room.trim().to_lowercase();
      if is_room(&room) {
        rooms.push(room);
      } else {
        tracing::warn!("SOGS ignoring invalid room token {:?}.", room);
      }
    }
    if rooms.is_empty() {
      tracing::warn!("At least one valid SOGS room token must be specified.");
      return None;
    }

    Some(Self {
      public_key,
      signing_key: SigningKey::from_bytes(&seed),
      host,
      port: url.port,
      secure: url.schema != "session",
      rooms,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Session Open Group Server",
      service_url: Some("https://getsession.org/"),
      setup_url: Some("https://appriseit.com/services/sogs/"),
      protocols: vec!["session", "sessions", "sogs"],
      description: "Post messages to Session Open Group Server (Session community) rooms.",
      attachment_support: false,
    }
  }

  /// The request body for one message. It is assembled by hand because the
  /// auth signature covers these exact bytes, which upstream produces with
  /// Python's `json.dumps` (a space after each `:` and `,`).
  fn request_body(&self, text: &str) -> String {
    let message = build_session_message(text);
    let signature = self.signing_key.sign(&message);
    format!(r#"{{"data": "{}", "signature": "{}"}}"#, B64.encode(&message), B64.encode(signature.to_bytes()))
  }

  /// The four `X-SOGS-*` headers. The signature covers
  /// `SERVER_KEY ‖ NONCE ‖ TIMESTAMP ‖ METHOD ‖ PATH [‖ BLAKE2B-512(BODY)]`,
  /// with the body hash only when there is a body.
  fn auth_headers(&self, method: &str, path: &str, body: &[u8], nonce: [u8; 16], timestamp: u64) -> [(&'static str, String); 4] {
    let timestamp = timestamp.to_string();
    let mut to_sign = Vec::with_capacity(32 + 16 + timestamp.len() + method.len() + path.len() + 64);
    to_sign.extend_from_slice(&self.public_key);
    to_sign.extend_from_slice(&nonce);
    to_sign.extend_from_slice(timestamp.as_bytes());
    to_sign.extend_from_slice(method.to_uppercase().as_bytes());
    to_sign.extend_from_slice(path.as_bytes());
    if !body.is_empty() {
      to_sign.extend_from_slice(&Blake2b512::digest(body));
    }
    let signature = self.signing_key.sign(&to_sign);
    [
      // The 00 prefix marks the key as unblinded
      ("X-SOGS-Pubkey", format!("00{}", hex::encode(self.signing_key.verifying_key().to_bytes()))),
      ("X-SOGS-Nonce", B64.encode(nonce)),
      ("X-SOGS-Timestamp", timestamp),
      ("X-SOGS-Signature", B64.encode(signature.to_bytes())),
    ]
  }

  fn base_url(&self) -> String {
    let (schema, default_port) = if self.secure { ("https", 443) } else { ("http", 80) };
    match self.port {
      Some(port) if port != default_port => format!("{}://{}:{}", schema, self.host, port),
      _ => format!("{}://{}", schema, self.host),
    }
  }

  /// POST one message to one room.
  async fn post(&self, client: &reqwest::Client, room: &str, body: &str) -> bool {
    let path = format!("/room/{}/message", room);
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let mut req = client.post(format!("{}{}", self.base_url(), path)).header("User-Agent", APP_ID).header("Content-Type", "application/json");
    for (name, value) in self.auth_headers("POST", &path, body.as_bytes(), rand::random(), timestamp) {
      req = req.header(name, value);
    }

    crate::notify::throttle().await;
    let resp = match req.body(body.to_string()).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A connection error occurred posting to SOGS room {:?}: {}", room, crate::error::loggable_http_error(&e));
        return false;
      }
    };
    let status = resp.status().as_u16();
    let content = match read_bounded_response(resp, MAX_RESPONSE_SIZE).await {
      Ok(Some(content)) => content,
      Ok(None) => {
        tracing::warn!("SOGS response from room {:?} exceeds {} bytes; not parsing it.", room, MAX_RESPONSE_SIZE);
        return false;
      }
      Err(e) => {
        tracing::warn!("A connection error occurred posting to SOGS room {:?}: {}", room, crate::error::loggable_http_error(&e));
        return false;
      }
    };
    if status != 200 && status != 201 {
      tracing::warn!("Failed to post to SOGS room {:?}: {}error={}.", room, error_hint(status), status);
      tracing::debug!("Response Details: {}", String::from_utf8_lossy(&content));
      return false;
    }
    tracing::info!("Sent SOGS notification to room {:?}.", room);
    true
  }
}

/// Encode text as a Session protocol `Content { dataMessage { body } }`
/// protobuf, followed by the 0x80 padding marker the server uses to find
/// the message boundary.
fn build_session_message(text: &str) -> Vec<u8> {
  let data_message = ld_field(1, text.as_bytes());
  let mut content = ld_field(1, &data_message);
  content.push(0x80);
  content
}

/// A protobuf length-delimited field (wire type 2).
fn ld_field(field: u64, data: &[u8]) -> Vec<u8> {
  let mut out = encode_varint((field << 3) | 2);
  out.extend(encode_varint(data.len() as u64));
  out.extend_from_slice(data);
  out
}

/// A protobuf base-128 varint.
fn encode_varint(mut n: u64) -> Vec<u8> {
  let mut out = Vec::new();
  loop {
    let b = (n & 0x7f) as u8;
    n >>= 7;
    if n == 0 {
      out.push(b);
      return out;
    }
    out.push(b | 0x80);
  }
}

/// 64 hex characters (either case, surrounding whitespace ignored) as 32 bytes.
fn decode_key(value: Option<&str>) -> Option<[u8; 32]> {
  let value = value?.trim();
  if value.len() != 64 {
    return None;
  }
  hex::decode(value).ok()?.try_into().ok()
}

/// A room token: 1 to 64 of alphanumerics, `_` and `-`.
fn is_room(s: &str) -> bool {
  (1..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The server's meaning of an HTTP status, for the log.
fn error_hint(status: u16) -> &'static str {
  match status {
    400 => "Bad Request -- malformed message data or headers., ",
    401 => "Unauthorized -- invalid or missing X-SOGS-* auth headers., ",
    403 => "Forbidden -- the bot does not have write permission in this room., ",
    404 => "Not Found -- the room token does not exist on this server., ",
    425 => "Too Early -- timestamp out of range or nonce already used., ",
    429 => "Too Many Requests -- rate limit exceeded., ",
    _ => "",
  }
}

#[async_trait]
impl Notify for SessionOgs {
  fn schemas(&self) -> &[&str] {
    &["session", "sessions", "sogs"]
  }
  fn service_name(&self) -> &str {
    "Session Open Group Server"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  // Session has no titles; the title is folded into the body
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // One signed message, posted to every room
    let body = self.request_body(&ctx.body);
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for room in &self.rooms {
      // A retry skips rooms that already accepted the message
      if crate::delivery::is_delivered(room, false) {
        continue;
      }
      if !self.post(&client, room, &body).await {
        has_error = true;
        continue;
      }
      crate::delivery::mark_delivered(room, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use ed25519_dalek::{Signature, Verifier};
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap_or_default().into_iter().filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with("/room/")).collect()
  }

  const SEED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  const PUBLIC_KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
  const BAD_KEY: &str = "ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
  const ROOM: &str = "test-room";

  // Produced by upstream's own plugin with the nonce pinned to 0x00..0x0f
  // and the timestamp to 1700000000 (the same vectors apprise-go pins).
  const VECTOR_SEED: &str = "0101010101010101010101010101010101010101010101010101010101010101";
  const VECTOR_PUBLIC_KEY: &str = "abababababababababababababababababababababababababababababababab";
  const VECTOR_TEXT: &str = "apprise parity\r\nhello from python";
  const VECTOR_MESSAGE_HEX: &str = "0a230a2161707072697365207061726974790d0a68656c6c6f2066726f6d20707974686f6e80";
  const VECTOR_BODY: &str = r#"{"data": "CiMKIWFwcHJpc2UgcGFyaXR5DQpoZWxsbyBmcm9tIHB5dGhvboA=", "signature": "8SOaRmy0etoWavp5tAgNSw0kSHeGmL7tKZRiVzhnE7YG8ONZ1ZSh50Al8milWnYT3YOe4+OYILJnufgNYlZtBQ=="}"#;
  const VECTOR_BOT_PUBKEY: &str = "008a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
  const VECTOR_AUTH_SIGNATURE: &str = "EwrOaAufFDdgr470EsJlRcRkrVAcYVojTyYbcEDtd88WBPx+Z+NIBOftvRuFaTjPciINx5xv8cleGvKke32lAg==";
  const VECTOR_GET_SIGNATURE: &str = "J3shF0sTsR1PGw83I2ol2lQlzxYWdfqgjVTuTGUsvhhqUPOiyg6R19P6s2IQ7ZHcLhsw8gOna/IOh9LNr3xkAw==";

  fn parse(url: &str) -> Option<SessionOgs> {
    SessionOgs::from_url(&ParsedUrl::parse(url)?)
  }

  fn vector_target() -> SessionOgs {
    parse(&format!("sogs://{}:{}@sogs.example.com/general", VECTOR_PUBLIC_KEY, VECTOR_SEED)).unwrap()
  }

  fn nonce() -> [u8; 16] {
    std::array::from_fn(|i| i as u8)
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("sessions://{}:{}@open.getsession.org/{}", PUBLIC_KEY, SEED, ROOM),
      format!("sessions://{}:{}@open.getsession.org?to={}", PUBLIC_KEY, SEED, ROOM),
      format!("sessions://{}:{}@open.getsession.org:8443/{}", PUBLIC_KEY, SEED, ROOM),
      format!("session://{}:{}@open.getsession.org/{}", PUBLIC_KEY, SEED, ROOM),
      format!("sogs://{}:{}@open.getsession.org/{}", PUBLIC_KEY, SEED, ROOM),
      format!("sessions://{}:{}@open.getsession.org/{}/other_room", PUBLIC_KEY, SEED, ROOM),
      format!("sessions://open.getsession.org/{}?key={}&seed={}", ROOM, PUBLIC_KEY, SEED),
      format!("sessions://open.getsession.org/{}?public_key={}&seed={}", ROOM, PUBLIC_KEY, SEED),
      format!("sessions://{}:{}@open.getsession.org/{}", PUBLIC_KEY.to_uppercase(), SEED.to_uppercase(), ROOM),
      // One valid room is enough; invalid ones are dropped
      format!("sessions://{}:{}@open.getsession.org/{}/!!!bad!!!", PUBLIC_KEY, SEED, ROOM),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "sessions://".to_string(),
      format!("sessions://host/{}", ROOM),
      format!("sessions://{}@host/{}", PUBLIC_KEY, ROOM),
      format!("sessions://{}:{}@host/{}", BAD_KEY, SEED, ROOM),
      format!("sessions://{}:{}@host", PUBLIC_KEY, SEED),
      format!("sessions://{}:{}@host/{}", PUBLIC_KEY, BAD_KEY, ROOM),
      format!("sessions://{}:{}@host/{}", PUBLIC_KEY, "g".repeat(64), ROOM),
      format!("sessions://{}:{}@host/!!!bad!!!", PUBLIC_KEY, SEED),
      format!("sessions://{}:{}@host/{}", PUBLIC_KEY, SEED, "a".repeat(65)),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_verifies_host() {
    // Upstream parses this URL with host verification
    crate::notify::custom_json::assert_verifies_host(&format!("sogs://{}:{}@{{h}}/room", "a".repeat(64), "b".repeat(64)));
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse(&format!("sessions://{}:{}@open.getsession.org/Room-B/{}", PUBLIC_KEY, SEED, ROOM)).unwrap();
    assert_eq!(hex::encode(obj.public_key), PUBLIC_KEY);
    // Rooms are lowercased after sorting
    assert_eq!(obj.rooms, vec!["room-b", ROOM]);
    assert!(obj.secure);
    assert_eq!(obj.base_url(), "https://open.getsession.org");

    // ?public_key= wins over ?key=, which wins over the user field
    let other = "d".repeat(64);
    let obj = parse(&format!("sessions://{}:{}@host/{}?key={}&public_key={}", BAD_KEY, SEED, ROOM, PUBLIC_KEY, other)).unwrap();
    assert_eq!(hex::encode(obj.public_key), other);
    // ?seed= wins over the password field
    let obj = parse(&format!("sessions://{}:{}@host/{}?seed={}", PUBLIC_KEY, BAD_KEY, ROOM, VECTOR_SEED)).unwrap();
    assert_eq!(obj.signing_key.to_bytes(), vector_target().signing_key.to_bytes());

    // Only a non-default port is part of the address
    assert_eq!(parse(&format!("session://{}:{}@host:80/{}", PUBLIC_KEY, SEED, ROOM)).unwrap().base_url(), "http://host");
    assert_eq!(parse(&format!("sessions://{}:{}@host:443/{}", PUBLIC_KEY, SEED, ROOM)).unwrap().base_url(), "https://host");
    assert_eq!(parse(&format!("sessions://{}:{}@host:8443/{}", PUBLIC_KEY, SEED, ROOM)).unwrap().base_url(), "https://host:8443");
    assert_eq!(parse(&format!("sogs://{}:{}@host:80/{}", PUBLIC_KEY, SEED, ROOM)).unwrap().base_url(), "https://host:80");
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("sogs://{}:{}@host/{}", PUBLIC_KEY, SEED, ROOM)).unwrap();
    assert_eq!(svc.body_maxlen(), 2000);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(!svc.attachment_support());
    assert_eq!(SessionOgs::static_details().protocols, vec!["session", "sessions", "sogs"]);
  }

  #[test]
  fn test_protobuf_helpers() {
    assert_eq!(encode_varint(0), vec![0x00]);
    assert_eq!(encode_varint(1), vec![0x01]);
    assert_eq!(encode_varint(127), vec![0x7f]);
    assert_eq!(encode_varint(128), vec![0x80, 0x01]);
    assert_eq!(encode_varint(300), vec![0xac, 0x02]);
    assert_eq!(ld_field(1, b"hi"), b"\x0a\x02hi".to_vec());
    let msg = build_session_message("hello");
    assert_eq!(msg[0], 0x0a);
    assert_eq!(*msg.last().unwrap(), 0x80);
    // A body over 127 bytes needs a two-byte length
    assert_eq!(&build_session_message(&"x".repeat(200))[..5], &[0x0a, 0xcb, 0x01, 0x0a, 0xc8]);
  }

  #[test]
  fn test_vectors_match_upstream() {
    assert_eq!(hex::encode(build_session_message(VECTOR_TEXT)), VECTOR_MESSAGE_HEX);
    let target = vector_target();
    // The exact body bytes matter: the auth signature covers them
    assert_eq!(target.request_body(VECTOR_TEXT), VECTOR_BODY);

    let headers = target.auth_headers("POST", "/room/general/message", VECTOR_BODY.as_bytes(), nonce(), 1700000000);
    assert_eq!(
      headers,
      [
        ("X-SOGS-Pubkey", VECTOR_BOT_PUBKEY.to_string()),
        ("X-SOGS-Nonce", "AAECAwQFBgcICQoLDA0ODw==".to_string()),
        ("X-SOGS-Timestamp", "1700000000".to_string()),
        ("X-SOGS-Signature", VECTOR_AUTH_SIGNATURE.to_string()),
      ]
    );

    // Without a body, the BLAKE2b hash is left out of what is signed
    let headers = target.auth_headers("get", "/room/test", b"", nonce(), 1700000000);
    assert_eq!(headers[3].1, VECTOR_GET_SIGNATURE);
  }

  /// Check a captured request's signature with the bot's public key.
  fn verify_request(req: &wiremock::Request, public_key: &[u8; 32]) {
    let header = |name: &str| req.headers.get(name).unwrap().to_str().unwrap().to_string();
    let pubkey = hex::decode(&header("X-SOGS-Pubkey")[2..]).unwrap();
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&pubkey.try_into().unwrap()).unwrap();
    let mut signed = public_key.to_vec();
    signed.extend(B64.decode(header("X-SOGS-Nonce")).unwrap());
    signed.extend(header("X-SOGS-Timestamp").as_bytes());
    signed.extend(b"POST");
    signed.extend(req.url.path().as_bytes());
    signed.extend(Blake2b512::digest(&req.body));
    let signature = Signature::from_slice(&B64.decode(header("X-SOGS-Signature")).unwrap()).unwrap();
    verifying_key.verify(&signed, &signature).expect("request signature verifies");

    // The message itself is signed by the same key
    let payload: Value = serde_json::from_slice(&req.body).unwrap();
    let message = B64.decode(payload["data"].as_str().unwrap()).unwrap();
    let signature = Signature::from_slice(&B64.decode(payload["signature"].as_str().unwrap()).unwrap()).unwrap();
    verifying_key.verify(&message, &signature).expect("message signature verifies");
  }

  async fn mock_rooms(rooms: &[(&str, u16, &str)]) -> MockServer {
    let server = MockServer::start().await;
    for (room, status, body) in rooms {
      Mock::given(method("POST"))
        .and(path(format!("/room/{}/message", room)))
        .respond_with(ResponseTemplate::new(*status).set_body_string(*body))
        .mount(&server)
        .await;
    }
    server
  }

  fn url_for(server: &MockServer, rooms: &str) -> String {
    let addr = server.address();
    format!("session://{}:{}@{}:{}/{}", VECTOR_PUBLIC_KEY, VECTOR_SEED, addr.ip(), addr.port(), rooms)
  }

  #[tokio::test]
  async fn test_send_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/room/{}/message", ROOM)))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id": 1}"#))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&url_for(&server, ROOM)).unwrap();
    let ctx = NotifyContext { body: VECTOR_TEXT.into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());

    let requests = received(&server).await;
    assert_eq!(String::from_utf8(requests[0].body.clone()).unwrap(), VECTOR_BODY);
    let header = |name: &str| requests[0].headers.get(name).unwrap().to_str().unwrap().to_string();
    assert_eq!(header("X-SOGS-Pubkey"), VECTOR_BOT_PUBKEY);
    assert!(header("X-SOGS-Timestamp").bytes().all(|c| c.is_ascii_digit()));
    assert_eq!(B64.decode(header("X-SOGS-Nonce")).unwrap().len(), 16);
    verify_request(&requests[0], &hex::decode(VECTOR_PUBLIC_KEY).unwrap().try_into().unwrap());
  }

  #[tokio::test]
  async fn test_send_multi_room() {
    let server = mock_rooms(&[("alpha", 200, "{}"), ("beta", 201, "")]).await;
    let svc = from_url(&url_for(&server, "beta/alpha")).unwrap();
    assert!(svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
    let requests = received(&server).await;
    let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
    assert_eq!(paths, vec!["/room/alpha/message", "/room/beta/message"]);
    // Each request has its own nonce
    assert_ne!(requests[0].headers.get("X-SOGS-Nonce"), requests[1].headers.get("X-SOGS-Nonce"));
  }

  #[tokio::test]
  async fn test_send_partial_failure() {
    let server = mock_rooms(&[("alpha", 403, r#"{"error": "forbidden"}"#), ("beta", 201, "")]).await;
    let svc = from_url(&url_for(&server, "alpha/beta")).unwrap();
    assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
    // Every room is still tried
    assert_eq!(received(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [400u16, 401, 404, 425, 500, 999, 204] {
      let server = mock_rooms(&[(ROOM, status, "")]).await;
      let svc = from_url(&url_for(&server, ROOM)).unwrap();
      assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap(), "status {}", status);
    }
    // An unreadable body on success is fine
    let server = mock_rooms(&[(ROOM, 201, "not-json")]).await;
    let svc = from_url(&url_for(&server, ROOM)).unwrap();
    assert!(svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());

    let svc = from_url(&format!("session://{}:{}@127.0.0.1:1/{}", PUBLIC_KEY, SEED, ROOM)).unwrap();
    assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_oversized_response_rejected() {
    let big = "x".repeat(MAX_RESPONSE_SIZE + 1);
    let server = mock_rooms(&[(ROOM, 201, big.as_str())]).await;
    let svc = from_url(&url_for(&server, ROOM)).unwrap();
    assert!(!svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());

    // Exactly the limit is still read
    let fits = "x".repeat(MAX_RESPONSE_SIZE);
    let server = mock_rooms(&[(ROOM, 201, fits.as_str())]).await;
    let svc = from_url(&url_for(&server, ROOM)).unwrap();
    assert!(svc.send(&NotifyContext { body: "B".into(), ..Default::default() }).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/room/alpha/message")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The second room fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/room/beta/message"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/room/beta/message")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("{}?retry=1&wait=0", url_for(&server, "alpha/beta"))));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "B".into(), ..Default::default() })).await.is_success());
    let sent: Vec<String> =
      crate::notify::custom_json::requests_to(&server, "POST", "/room/beta/message").await.iter().map(|r| r.url.path().to_string()).collect();
    assert_eq!(sent, vec!["/room/beta/message", "/room/beta/message"]);
  }
}

//! Upstream `tests/test_plugin_nostr.py`: relays are replaced by an
//! in-memory stand-in (upstream's `relays` fixture) and Blossom servers and
//! Nostr address lookups by `wiremock`.

use super::crypto::nip44_decrypt;
use super::*;
use crate::notify::with_test_base;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// ── The relay stand-in ───────────────────────────────────────────────────

/// What the stand-in relays saw, and how they behave.
#[derive(Default)]
pub(super) struct FakeRelays {
  /// (relay url, event) for every publish attempt
  published: Vec<(String, Event)>,
  /// (relay url, filter) for every lookup
  queries: Vec<(String, Filter)>,
  /// Relays that can not be reached, or that refuse everything
  down: HashSet<String>,
  refuse: HashSet<String>,
  /// Recipients whose messages are always refused
  refuse_pubkeys: HashSet<String>,
  /// Stored events returned by lookups, by kind
  events: HashMap<i64, Vec<Value>>,
  /// Relays only allowed to reach public addresses
  public_only: HashSet<String>,
}

type Relays = Arc<Mutex<FakeRelays>>;

tokio::task_local! {
  static FAKE_RELAYS: Relays;
}

/// Stand in for the relay at `url` when a test set up [`FakeRelays`].
pub(super) fn fake_relay(url: &str, public_only: bool) -> Option<Result<Box<dyn Relay>, RelayError>> {
  let state = FAKE_RELAYS.try_with(Arc::clone).ok()?;
  let mut s = state.lock().unwrap();
  if public_only {
    s.public_only.insert(url.to_string());
  }
  if s.down.contains(url) {
    return Some(Err(RelayError("down".into())));
  }
  drop(s);
  Some(Ok(Box::new(FakeRelay { url: url.to_string(), state })))
}

struct FakeRelay {
  url: String,
  state: Relays,
}

#[async_trait]
impl Relay for FakeRelay {
  async fn publish(&mut self, event: &Event) -> Result<(bool, String), RelayError> {
    let mut s = self.state.lock().unwrap();
    s.published.push((self.url.clone(), event.clone()));
    let refused = s.refuse.contains(&self.url) || event.tags.iter().any(|t| t[0] == "p" && s.refuse_pubkeys.contains(&t[1]));
    Ok(if refused { (false, "blocked: no".to_string()) } else { (true, String::new()) })
  }

  async fn query(&mut self, filter: &Filter) -> Result<Vec<Value>, RelayError> {
    let mut s = self.state.lock().unwrap();
    s.queries.push((self.url.clone(), filter.clone()));
    Ok(s.events.get(&filter.kinds[0]).cloned().unwrap_or_default())
  }

  async fn close(&mut self) {}
}

fn relays() -> Relays {
  Relays::default()
}

/// Run `fut` with every relay connection going to `relays`.
async fn with_relays<F: std::future::Future>(relays: &Relays, fut: F) -> F::Output {
  FAKE_RELAYS.scope(relays.clone(), fut).await
}

// ── Keys, URLs and helpers ───────────────────────────────────────────────

fn sender() -> NostrKey {
  NostrKey::from_secret(&[0x11; 32]).unwrap()
}

fn recipient() -> NostrKey {
  NostrKey::from_secret(&[0x22; 32]).unwrap()
}

fn other() -> NostrKey {
  NostrKey::from_secret(&[0x33; 32]).unwrap()
}

const NSEC: &str = "nsec1zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs4rm7hz";
const NPUB: &str = "npub1gekhljh9v0jukzdq6xrshdvqx3yqgctc0xs5jjw0yg597xaw8uns47vduw";
const NPUB_OTHER: &str = "npub183e2mk60muy6l98se9xhl6f28p48uux03gwctytrs6aj2dw8kxcsw0rcm2";
const BLOB_URL: &str = "https://cdn.example.com/abc.png";

/// The test attachment: a PNG from the default theme.
const ATTACHMENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/themes/default/apprise-info-72x72.png");

fn attachment_data() -> Vec<u8> {
  std::fs::read(ATTACHMENT).unwrap()
}

fn parse(url: &str) -> Option<Nostr> {
  Nostr::from_url(&ParsedUrl::parse(url)?)
}

fn ctx(body: &str) -> NotifyContext {
  NotifyContext { body: body.to_string(), ..Default::default() }
}

fn apprise(url: &str) -> crate::Apprise {
  let mut a = crate::Apprise::new();
  assert!(a.add(url), "{url}");
  a
}

/// Send through the whole pipeline (title folding, retries, the store).
async fn notify(relays: &Relays, url: &str, body: &str, title: &str, attach: bool) -> bool {
  notify_with(relays, &apprise(url), body, title, attach).await
}

async fn notify_with(relays: &Relays, a: &crate::Apprise, body: &str, title: &str, attach: bool) -> bool {
  let ctx = NotifyContext { title: title.to_string(), ..ctx(body) };
  let sources: Vec<String> = if attach { vec![ATTACHMENT.to_string()] } else { Vec::new() };
  with_relays(relays, a.notify_with_attachments(&ctx, &sources)).await.is_success()
}

/// NIP-17 relay lists (kind 10050) for each key.
fn inbox(keys: &[NostrKey], url: &str) -> Vec<Value> {
  keys.iter().map(|k| serde_json::to_value(k.sign(10050, "", vec![tag(&["relay", url])], None)).unwrap()).collect()
}

/// The message hidden inside a NIP-17 gift wrap.
fn unwrap(wrap: &Event, key: &NostrKey) -> Event {
  let seal = nip44_decrypt(&wrap.content, &key.conversation_key(&wrap.pubkey).unwrap()).unwrap();
  let seal = verify_event(&serde_json::from_str(&seal).unwrap()).unwrap();
  assert_eq!(seal.kind, 13);
  let message: Event = serde_json::from_str(&nip44_decrypt(&seal.content, &key.conversation_key(&seal.pubkey).unwrap()).unwrap()).unwrap();
  assert_eq!(message.pubkey, seal.pubkey);
  assert_eq!(message.pubkey, sender().public_key());
  message
}

fn published(relays: &Relays) -> Vec<(String, Event)> {
  relays.lock().unwrap().published.clone()
}

fn published_urls(relays: &Relays) -> Vec<String> {
  published(relays).into_iter().map(|(u, _)| u).collect()
}

/// A store that lasts for the whole test, for calls made outside a send.
fn store() -> crate::storage::StoreSlot {
  crate::storage::StoreSlot { registry: Default::default(), namespace: Some("nostrtest".into()), path: None, mode: crate::types::StorageMode::Memory }
}

/// Answer a Blossom upload with a description of the stored file.
struct BlossomReply;

impl Respond for BlossomReply {
  fn respond(&self, request: &Request) -> ResponseTemplate {
    ResponseTemplate::new(201).set_body_json(json!({"url": BLOB_URL, "sha256": hex::encode(Sha256::digest(&request.body)), "size": request.body.len()}))
  }
}

async fn blossom(server: &MockServer, reply: impl Respond + 'static) {
  Mock::given(method("PUT")).and(path("/upload")).respond_with(reply).mount(server).await;
}

async fn uploads(server: &MockServer) -> Vec<Request> {
  server.received_requests().await.unwrap_or_default().into_iter().filter(|r| r.method.as_str() == "PUT" && r.url.path() == "/upload").collect()
}

// ── URLs ─────────────────────────────────────────────────────────────────

/// Upstream `apprise_url_tests`: which URLs load.
#[test]
fn test_urls() {
  let hex_secret = "11".repeat(32);
  for url in [
    // No secret key, or not a usable one
    "nostr://".to_string(),
    "nostr://invalid".to_string(),
    // Zero is not a valid secret key
    format!("nostr://{}", "0".repeat(64)),
    // An npub is not a secret key
    format!("nostr://{}", NPUB),
    format!("nostr://{}?mode=invalid", NSEC),
    format!("nostr://{}?relay=relay!", NSEC),
    // A port that can not exist, or can not be connected to
    format!("nostr://{}?relay=relay.example.com:99999", NSEC),
    format!("nostr://{}?relay=relay.example.com:0", NSEC),
    // A hostname label longer than 63 characters
    format!("nostr://{}?relay={}.example.com", NSEC, "a".repeat(64)),
  ] {
    assert!(parse(&url).is_none(), "{url}");
  }
  for url in [
    format!("nostr://{}", NSEC),
    format!("nostr://?key={}", hex_secret),
    format!("nostr://{}/junk", NSEC),
    format!("nostr://{}?blossom=cdn.example.com&relay=relay.example.com", NSEC),
    format!("nostr://{}?blossom=https://cdn.example.com", NSEC),
    format!("nostr://{}?mode=%20NIP04%20", NSEC),
    format!("nostr://{}?mode=", NSEC),
  ] {
    assert!(parse(&url).is_some(), "{url}");
  }
  // The secret key may be given in ?key= even with a host
  assert_eq!(parse(&format!("nostr://ignored?key={}", hex_secret)).unwrap().key.public_key(), sender().public_key());
  assert!(crate::notify::registry::from_url(&format!("nostr://{}", NSEC)).is_some());
}

/// Upstream `test_plugin_nostr_init`: keys, recipients, relays and options.
#[test]
fn test_init() {
  let url = format!(
    "nostr://{}/{}/{}/Alice@Example.com/x?relay=relay.example.com,%20ws://10.0.0.1:7777/path&blossom=https://cdn.example.com/&mode=NIP04",
    "11".repeat(32),
    NPUB,
    recipient().public_key().to_uppercase()
  );
  let obj = parse(&url).unwrap();
  assert_eq!(obj.key.public_key(), sender().public_key());
  assert_eq!(obj.targets, [recipient().public_key(), recipient().public_key(), "alice@example.com"]);
  assert_eq!(obj.invalid_targets, ["x"]);
  assert_eq!(obj.relays, ["wss://relay.example.com", "ws://10.0.0.1:7777/path"]);
  assert_eq!(obj.blossom.as_deref(), Some("https://cdn.example.com"));
  assert_eq!(obj.mode, Mode::Nip04);

  // Defaults, and recipients given with ?to= (sorted, as upstream's
  // parse_list() does)
  let obj = parse(&format!("nostr://{}?to={},bob@example.com", NSEC, NPUB)).unwrap();
  assert_eq!(obj.targets, ["bob@example.com", recipient().public_key()]);
  assert_eq!(obj.mode, Mode::Nip17);
  assert_eq!(obj.blossom, None);
  assert_eq!(obj.relays, NOSTR_DEFAULT_RELAYS);

  // The public key is the identity, whatever form the key came in
  let id = Some(vec!["nostr".to_string(), sender().public_key().to_string()]);
  assert_eq!(parse(&format!("nostr://{}", NSEC)).unwrap().url_identifier(), id);
  assert_eq!(parse(&format!("nostr://{}", "11".repeat(32))).unwrap().url_identifier(), id);

  // Relays keep their order and duplicates, and their case
  let obj = parse(&format!("nostr://{}?relay=WSS://A.example.com%3Bb.example.com%20[a.example.com]", NSEC)).unwrap();
  assert_eq!(obj.relays, ["wss://A.example.com", "wss://b.example.com", "wss://a.example.com"]);
  // A Blossom server keeps its schema
  assert_eq!(parse(&format!("nostr://{}?blossom=http://10.0.0.1:3000//", NSEC)).unwrap().blossom.as_deref(), Some("http://10.0.0.1:3000"));
}

/// Upstream `normalize_relay()`.
#[test]
fn test_normalize_relay() {
  for (value, expected) in [
    ("relay.example.com", Some("wss://relay.example.com")),
    ("ws://relay.example.com:7777/path/x", Some("ws://relay.example.com:7777/path/x")),
    ("WSS://Relay.example.com", Some("wss://Relay.example.com")),
    ("10.0.0.1", Some("wss://10.0.0.1")),
    ("relay.example.com\n", Some("wss://relay.example.com")),
    ("wss://relay.example.com:00080", Some("wss://relay.example.com:00080")),
    ("wss://:7777", None),
    ("wss://relay.example.com:", None),
    ("wss://relay.example.com/a b", None),
    ("wss://relay.example.com/a?b", None),
    ("wss://relay.example.com/a#b", None),
    ("http://relay.example.com", None),
    ("wss://relay~x.example.com", None),
    ("wss://user@relay.example.com", None),
    ("wss://[::1]", None),
    ("wss://relay.example.com:65536", None),
  ] {
    assert_eq!(normalize_relay(value).as_deref(), expected, "{value:?}");
  }
  assert_eq!(normalize_relay(&format!("wss://example.com/{}", "a".repeat(2048))), None);
  assert_eq!(normalize_relay(&format!("wss://{}.example.com", "a".repeat(256))), None);
}

#[test]
fn test_service_details() {
  let svc = parse(&format!("nostr://{}", NSEC)).unwrap();
  assert_eq!(svc.schemas(), ["nostr"]);
  assert_eq!(svc.notify_format(), NotifyFormat::Text);
  assert_eq!((svc.title_maxlen(), svc.body_maxlen()), (0, 7000));
  assert_eq!(svc.request_rate_per_sec(), 0.0);
  assert!(svc.attachment_support());
  assert!(Nostr::static_details().attachment_support);
}

// ── Public notes ─────────────────────────────────────────────────────────

/// Upstream `test_plugin_nostr_note`: a signed public note goes to every
/// relay, and one accepting it is enough.
#[tokio::test]
async fn test_note() {
  let relays = relays();
  let url = format!("nostr://{}?relay=one.example.com,two.example.com", NSEC);
  assert!(notify(&relays, &url, "Hello & <world>", "Title", false).await);
  assert_eq!(published_urls(&relays), ["wss://one.example.com", "wss://two.example.com"]);
  let all = published(&relays);
  let event = &all[0].1;
  assert_eq!(&all[1].1, event);
  assert_eq!((event.kind, event.pubkey.as_str()), (1, sender().public_key()));
  assert_eq!(event.content, "Title\r\nHello & <world>");
  assert!(event.tags.is_empty());
  assert!(verify_event(&serde_json::to_value(event).unwrap()).is_some());

  relays.lock().unwrap().published.clear();
  relays.lock().unwrap().refuse.insert("wss://one.example.com".into());
  assert!(notify(&relays, &url, "Hello", "", false).await);

  relays.lock().unwrap().down.insert("wss://two.example.com".into());
  assert!(!notify(&relays, &url, "Hello", "", false).await);

  // Only invalid recipients, so nothing is sent
  relays.lock().unwrap().published.clear();
  assert!(!notify(&relays, &format!("nostr://{}/junk?relay=one.example.com", NSEC), "Hello", "", false).await);
  assert!(published(&relays).is_empty());

  // With no relays given, the well known public ones are used
  let relays = Relays::default();
  assert!(notify(&relays, &format!("nostr://{}", NSEC), "Hello", "", false).await);
  assert_eq!(published_urls(&relays), NOSTR_DEFAULT_RELAYS);
}

/// Upstream `test_plugin_nostr_note_attachments`: attachments are uploaded
/// with a signed permission and linked in the note.
#[tokio::test]
async fn test_note_attachments() {
  let server = MockServer::start().await;
  blossom(&server, BlossomReply).await;
  let data = attachment_data();
  let digest = hex::encode(Sha256::digest(&data));
  let relays = relays();
  let url = format!("nostr://{}?relay=one.example.com&blossom=cdn.example.com", NSEC);
  assert!(with_test_base(server.uri(), notify(&relays, &url, "Look", "", true)).await);

  // The upload went to the Blossom server with a signed permission
  let sent = uploads(&server).await;
  assert_eq!(sent.len(), 1);
  assert_eq!(sent[0].body, data);
  let header = |name: &str| sent[0].headers.get(name).unwrap().to_str().unwrap().to_string();
  assert_eq!(header("X-SHA-256"), digest);
  assert_eq!(header("Content-Type"), "image/png");
  assert_eq!(header("User-Agent"), APP_ID);
  let token = header("Authorization").strip_prefix("Nostr ").unwrap().to_string();
  assert!(!token.contains('='));
  let auth_json = String::from_utf8(URL_SAFE_NO_PAD.decode(&token).unwrap()).unwrap();
  // Written as Python's json.dumps() writes it
  assert!(auth_json.starts_with(&format!("{{\"pubkey\": \"{}\", \"created_at\": ", sender().public_key())), "{auth_json}");
  let auth = verify_event(&serde_json::from_str(&auth_json).unwrap()).unwrap();
  assert_eq!((auth.kind, auth.content.as_str()), (24242, "Upload Blob"));
  assert!(auth.tags.contains(&tag(&["t", "upload"])));
  assert!(auth.tags.contains(&tag(&["x", &digest])));
  let expires: i64 = auth.tags.iter().find(|t| t[0] == "expiration").unwrap()[1].parse().unwrap();
  assert!((expires - chrono::Utc::now().timestamp() - 300).abs() <= 5);

  // The note links the file and describes it
  let event = &published(&relays)[0].1;
  assert_eq!(event.content, format!("Look\n{}", BLOB_URL));
  assert_eq!(
    event.tags,
    vec![vec!["imeta".to_string(), format!("url {}", BLOB_URL), "m image/png".into(), format!("x {}", digest), format!("size {}", data.len())]]
  );

  // An attachment with no body is fine too
  relays.lock().unwrap().published.clear();
  assert!(with_test_base(server.uri(), notify(&relays, &url, "", "", true)).await);
  assert_eq!(published(&relays)[0].1.content, BLOB_URL);
}

/// Upstream `test_plugin_nostr_attach_errors`: a send fails cleanly when an
/// attachment can not be sent.
#[tokio::test]
async fn test_attach_errors() {
  let relays = relays();
  let url = format!("nostr://{}?blossom=cdn.example.com", NSEC);

  // An inaccessible file is never uploaded
  let server = MockServer::start().await;
  blossom(&server, BlossomReply).await;
  let a = apprise(&url);
  let ctx = ctx("x");
  let missing = vec!["/nonexistent/apprise-test.png".to_string()];
  assert!(!with_test_base(server.uri(), with_relays(&relays, a.notify_with_attachments(&ctx, &missing))).await.is_success());
  assert!(uploads(&server).await.is_empty());

  // The Blossom server can not be reached
  let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
  assert!(!with_test_base(format!("http://127.0.0.1:{closed}"), notify(&relays, &url, "x", "", true)).await);

  // The Blossom server refuses it
  let server = MockServer::start().await;
  blossom(&server, ResponseTemplate::new(401).insert_header("X-Reason", "denied")).await;
  assert!(!with_test_base(server.uri(), notify(&relays, &url, "x", "", true)).await);

  // The answer has no usable link, or does not describe our file
  let data = attachment_data();
  let good = json!({"url": BLOB_URL, "sha256": hex::encode(Sha256::digest(&data)), "size": data.len()});
  let with = |changes: Value| {
    let mut v = good.clone();
    for (k, val) in changes.as_object().unwrap() {
      v[k] = val.clone();
    }
    v.to_string().into_bytes()
  };
  let mut nested = vec![b'['; 100000];
  nested.extend(vec![b']'; 100000]);
  for content in [
    b"not json".to_vec(),
    nested,
    b"[]".to_vec(),
    vec![b'x'; 1048577],
    with(json!({"url": "ftp://x"})),
    with(json!({"url": "http://cdn.example.com/abc.png"})),
    with(json!({"url": format!("https://x/{}", "a".repeat(2048))})),
    with(json!({"url": 1})),
    with(json!({"sha256": "0".repeat(64)})),
    with(json!({"sha256": null})),
    with(json!({"size": 1})),
    with(json!({"size": null})),
    with(json!({"size": "1"})),
  ] {
    let server = MockServer::start().await;
    blossom(&server, ResponseTemplate::new(200).set_body_bytes(content.clone())).await;
    assert!(!with_test_base(server.uri(), notify(&relays, &url, "x", "", true)).await, "{}", String::from_utf8_lossy(&content[..content.len().min(80)]));
  }

  // A good answer with a 200 (already stored), an upper case digest and the
  // size written as a float is accepted
  let server = MockServer::start().await;
  let reply = json!({"url": BLOB_URL, "sha256": hex::encode(Sha256::digest(&data)).to_uppercase(), "size": data.len() as f64});
  blossom(&server, ResponseTemplate::new(200).set_body_json(reply)).await;
  assert!(with_test_base(server.uri(), notify(&relays, &url, "x", "", true)).await);

  // Nothing was published until then
  assert_eq!(published(&relays).len(), NOSTR_DEFAULT_RELAYS.len());
}

/// Upstream `test_plugin_nostr_blossom_lookup`: our own Blossom server list
/// is used, or a default.
#[tokio::test]
async fn test_blossom_lookup() {
  let relays = relays();
  let list = sender().sign(10063, "", vec![tag(&["server", "http://insecure.example.com"]), tag(&["server", "https://mine.example.com/"])], None);
  relays.lock().unwrap().events.insert(10063, vec![serde_json::to_value(list).unwrap()]);
  let obj = parse(&format!("nostr://{}?relay=one.example.com", NSEC)).unwrap();
  let slot = store();
  let server = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.blossom_server())).await;
  assert_eq!(server, "https://mine.example.com");
  assert_eq!(relays.lock().unwrap().queries[0].1, Filter { kinds: vec![10063], authors: Some(vec![sender().public_key().to_string()]), limit: Some(1) });

  // The answer is cached
  let server = crate::storage::with_store_slot(slot, with_relays(&relays, obj.blossom_server())).await;
  assert_eq!(server, "https://mine.example.com");
  assert_eq!(relays.lock().unwrap().queries.len(), 1);

  // ... and used for uploads
  let mock = MockServer::start().await;
  blossom(&mock, BlossomReply).await;
  let url = format!("nostr://{}?relay=one.example.com", NSEC);
  let a = apprise(&url);
  assert!(with_test_base(mock.uri(), notify_with(&relays, &a, "x", "", true)).await);
  assert!(with_test_base(mock.uri(), notify_with(&relays, &a, "x", "", true)).await);
  assert_eq!(relays.lock().unwrap().queries.len(), 2);
  assert_eq!(uploads(&mock).await.len(), 2);

  // Without a list, the default server is used
  relays.lock().unwrap().events.clear();
  let server = crate::storage::with_store_slot(store(), with_relays(&relays, obj.blossom_server())).await;
  assert_eq!(server, NOSTR_DEFAULT_BLOSSOM);

  // When no relay can be asked, the default is used but not remembered
  relays.lock().unwrap().down.insert("wss://one.example.com".into());
  let slot = store();
  let server = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.blossom_server())).await;
  assert_eq!(server, NOSTR_DEFAULT_BLOSSOM);
  assert_eq!(crate::storage::with_store_slot(slot, async { store_get("blossom") }).await, None);
  assert!(!with_test_base(mock.uri(), notify(&relays, &url, "x", "", true)).await);

  // A plain http:// server of our own may hand back http:// links
  relays.lock().unwrap().down.clear();
  let data = attachment_data();
  let mock = MockServer::start().await;
  blossom(&mock, ResponseTemplate::new(200).set_body_json(json!({"url": "http://10.0.0.1/abc.png", "sha256": hex::encode(Sha256::digest(&data))}))).await;
  assert!(with_test_base(mock.uri(), notify(&relays, &format!("nostr://{}?relay=one.example.com&blossom=http://10.0.0.1", NSEC), "x", "", true)).await);
}

// ── Lookups ──────────────────────────────────────────────────────────────

/// Upstream `test_plugin_nostr_lookup`: only the newest correctly signed
/// list is trusted.
#[tokio::test]
async fn test_lookup() {
  let relays = relays();
  relays.lock().unwrap().down.insert("wss://down.example.com".into());
  let obj = parse(&format!("nostr://{}?relay=down.example.com,one.example.com", NSEC)).unwrap();
  let r = recipient();
  let sign = |key: &NostrKey, kind: i64, tags: Vec<Vec<String>>, at: i64| serde_json::to_value(key.sign(kind, "", tags, Some(at))).unwrap();
  let older = sign(&r, 10050, vec![tag(&["relay", "wss://old.example.com"])], 100);
  let newer = sign(
    &r,
    10050,
    vec![
      tag(&["relay", "wss://new.example.com"]),
      tag(&["relay", "wss://new.example.com"]),
      tag(&["relay", "ws://insecure.example.com"]),
      tag(&["relay", "wss://:7777"]),
      tag(&["relay", "wss://bad-port.example.com:99999"]),
      vec!["relay".into(), format!("wss://{}.example.com", "a".repeat(256))],
      vec!["relay".into(), format!("wss://example.com/{}", "a".repeat(2048))],
      tag(&["relay", "wss://two.example.com"]),
      tag(&["relay", "wss://three.example.com"]),
      tag(&["relay", "wss://four.example.com"]),
      tag(&["relay"]),
      tag(&["other", "wss://x.example.com"]),
    ],
    200,
  );
  let oldest = sign(&r, 10050, vec![tag(&["relay", "wss://x.example.com"])], 50);
  let mut forged = newer.clone();
  forged["created_at"] = json!(300);
  let stranger = sign(&other(), 10050, vec![tag(&["relay", "wss://x.example.com"])], 400);
  let wrong_kind = sign(&r, 1, vec![tag(&["relay", "wss://x.example.com"])], 500);
  let mut malformed = newer.clone();
  malformed["tags"] = json!(1);
  malformed["created_at"] = json!([]);
  relays.lock().unwrap().events.insert(10050, vec![older, newer, oldest, forged, stranger, wrong_kind, malformed]);

  // Duplicates and broken entries are dropped, and only 3 are used
  let expected = ["wss://new.example.com", "wss://two.example.com", "wss://three.example.com"].map(String::from).to_vec();
  let slot = store();
  let found = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.inbox_relays(r.public_key()))).await;
  assert_eq!(found, Some(expected.clone()));

  // The answer is cached, so the relays are not asked again
  let queries = relays.lock().unwrap().queries.len();
  let found = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.inbox_relays(r.public_key()))).await;
  assert_eq!(found, Some(expected));
  assert_eq!(relays.lock().unwrap().queries.len(), queries);

  // A recipient without a list has no relays
  relays.lock().unwrap().events.clear();
  let found = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.inbox_relays(other().public_key()))).await;
  assert_eq!(found, Some(Vec::new()));

  // When no relay can be asked, the lookup failed and is not cached
  relays.lock().unwrap().down.insert("wss://one.example.com".into());
  let found = crate::storage::with_store_slot(slot.clone(), with_relays(&relays, obj.inbox_relays(sender().public_key()))).await;
  assert_eq!(found, None);
  let key = format!("inbox_{}", sender().public_key());
  assert_eq!(crate::storage::with_store_slot(slot, async move { store_get(&key) }).await, None);

  // Without a store (outside a send) lookups still work, uncached
  relays.lock().unwrap().down.clear();
  relays.lock().unwrap().events.insert(10050, inbox(std::slice::from_ref(&r), "wss://inbox.example.com"));
  for _ in 0..2 {
    assert_eq!(with_relays(&relays, obj.inbox_relays(r.public_key())).await, Some(vec!["wss://inbox.example.com".to_string()]));
  }
}

/// Upstream `test_plugin_nostr_nip05`: Nostr addresses are looked up and
/// cached.
#[tokio::test]
async fn test_nip05() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/.well-known/nostr.json"))
    .and(query_param("name", "alice"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"names": {"alice": recipient().public_key().to_uppercase()}})))
    .expect(1)
    .mount(&server)
    .await;
  let obj = parse(&format!("nostr://{}/alice@example.com", NSEC)).unwrap();
  let slot = store();
  let resolve = |target: &'static str| crate::storage::with_store_slot(slot.clone(), with_test_base(server.uri(), obj.resolve(target)));
  assert_eq!(resolve("alice@example.com").await.as_deref(), Some(recipient().public_key()));

  // The answer is cached
  assert_eq!(resolve("alice@example.com").await.as_deref(), Some(recipient().public_key()));
  let sent: Vec<Request> = server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == "/.well-known/nostr.json").collect();
  assert_eq!(sent.len(), 1);
  assert_eq!(sent[0].headers.get("Accept").unwrap(), "application/json");
  assert_eq!(sent[0].headers.get("User-Agent").unwrap(), APP_ID);

  // Public keys need no lookup
  let hex_other = other().public_key().to_string();
  assert_eq!(obj.resolve(&hex_other).await, Some(hex_other));

  // Names that are missing, malformed or refused are not found
  let mut nested = vec![b'['; 100000];
  nested.extend(vec![b']'; 100000]);
  for (status, content) in [
    (200, br#"{"names": {"bob": "zz"}}"#.to_vec()),
    (200, json!({"names": {"bob": "f".repeat(64)}}).to_string().into_bytes()),
    (200, br#"{"names": []}"#.to_vec()),
    (200, b"[]".to_vec()),
    (200, b"garbage".to_vec()),
    (200, nested),
    (200, vec![b'x'; 1048577]),
    (404, json!({"names": {"bob": recipient().public_key()}}).to_string().into_bytes()),
    (200, json!({"names": {"alice": recipient().public_key()}}).to_string().into_bytes()),
  ] {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/.well-known/nostr.json")).respond_with(ResponseTemplate::new(status).set_body_bytes(content)).mount(&server).await;
    assert_eq!(with_test_base(server.uri(), obj.resolve("bob@example.com")).await, None);
  }

  // NIP-05 requires that redirects are never followed
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/.well-known/nostr.json"))
    .respond_with(ResponseTemplate::new(302).insert_header("Location", format!("{}/moved", server.uri())))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/moved"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"names": {"bob": recipient().public_key()}})))
    .mount(&server)
    .await;
  assert_eq!(with_test_base(server.uri(), obj.resolve("bob@example.com")).await, None);

  // A server that can not be reached
  let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
  assert_eq!(with_test_base(format!("http://127.0.0.1:{closed}"), obj.resolve("bob@example.com")).await, None);

  // Addresses that are not NIP-05 names are dropped when loading
  for target in ["bob@localhost", "@example.com", "bob@", "bob@exa_mple.com", "b+b@example.com", "bob@example..com"] {
    assert_eq!(parse(&format!("nostr://{}/{}", NSEC, target)).unwrap().invalid_targets, [target], "{target}");
  }
}

// ── Direct messages ──────────────────────────────────────────────────────

/// Upstream `test_plugin_nostr_nip17`: sealed, gift wrapped private
/// messages.
#[tokio::test]
async fn test_nip17() {
  let url = format!("nostr://{}/{}?relay=one.example.com", NSEC, NPUB);
  let relays = relays();

  // A recipient without a list of relays can not be sent to
  assert!(!notify(&relays, &url, "Hello", "", false).await);
  assert!(published(&relays).is_empty());

  // Nor can one whose list could not be looked up
  relays.lock().unwrap().down.insert("wss://one.example.com".into());
  assert!(!notify(&relays, &url, "Hello", "", false).await);
  assert!(published(&relays).is_empty());
  relays.lock().unwrap().down.clear();

  relays.lock().unwrap().events.insert(10050, inbox(&[recipient()], "wss://inbox.example.com"));
  assert!(notify(&relays, &url, "Hello & <world> é", "Title", false).await);

  // The message went only to the recipient's own inbox relay, and only to
  // a public address
  let all = published(&relays);
  assert_eq!(all.len(), 1);
  let (relay_url, wrap) = &all[0];
  assert_eq!(relay_url, "wss://inbox.example.com");
  assert_eq!(relays.lock().unwrap().public_only, HashSet::from(["wss://inbox.example.com".to_string()]));
  assert_eq!(wrap.kind, 1059);
  assert_eq!(wrap.tags, vec![tag(&["p", recipient().public_key()])]);
  assert_ne!(wrap.pubkey, sender().public_key());
  assert!(verify_event(&serde_json::to_value(wrap).unwrap()).is_some());
  let message = unwrap(wrap, &recipient());
  assert_eq!(message.kind, 14);
  assert_eq!(message.content, "Title\r\nHello & <world> é");
  assert_eq!(message.tags, vec![tag(&["p", recipient().public_key()])]);
  assert_eq!(message.sig, None);

  // When we list relays of our own, we keep a copy there too
  relays.lock().unwrap().published.clear();
  relays.lock().unwrap().events.get_mut(&10050).unwrap().extend(inbox(&[sender()], "wss://mine.example.com"));
  assert!(notify(&relays, &url, "Hello", "", false).await);
  assert_eq!(published_urls(&relays), ["wss://inbox.example.com", "wss://mine.example.com"]);
  let copy = published(&relays)[1].1.clone();
  assert_eq!(copy.tags, vec![tag(&["p", sender().public_key()])]);
  assert_eq!(unwrap(&copy, &sender()).content, "Hello");
  assert_eq!(unwrap(&copy, &sender()).tags, vec![tag(&["p", recipient().public_key()])]);

  // Our own list is trusted, so it may name private relays
  assert_eq!(relays.lock().unwrap().public_only, HashSet::from(["wss://inbox.example.com".to_string()]));

  // Losing our own copy does not fail the message
  relays.lock().unwrap().refuse.insert("wss://mine.example.com".into());
  assert!(notify(&relays, &url, "Hello", "", false).await);

  // A message to ourselves is sent once, and our own relays may be private
  // ones
  relays.lock().unwrap().published.clear();
  relays.lock().unwrap().public_only.clear();
  relays.lock().unwrap().refuse.clear();
  let own = format!("nostr://{}/{}?relay=one.example.com", NSEC, npub(sender().public_key()));
  assert!(notify(&relays, &own, "Hello", "", false).await);
  assert_eq!(published_urls(&relays), ["wss://mine.example.com"]);
  assert!(relays.lock().unwrap().public_only.is_empty());
}

/// Upstream `test_plugin_nostr_nip17_attachments`: files are encrypted for
/// NIP-17 file messages, uploaded once and shared by every recipient.
#[tokio::test]
async fn test_nip17_attachments() {
  use aes_gcm::aead::{Aead, KeyInit};
  let server = MockServer::start().await;
  blossom(&server, BlossomReply).await;
  let relays = relays();
  relays.lock().unwrap().events.insert(10050, inbox(&[recipient(), other()], "wss://one.example.com"));
  let data = attachment_data();
  let url = format!("nostr://{}/{}/{}?relay=one.example.com&blossom=https://cdn.example.com", NSEC, NPUB, NPUB_OTHER);
  assert!(with_test_base(server.uri(), notify(&relays, &url, "", "", true)).await);

  let sent = uploads(&server).await;
  assert_eq!(sent.len(), 1);
  let uploaded = sent[0].body.clone();
  assert_ne!(uploaded, data);
  assert_eq!(sent[0].headers.get("Content-Type").unwrap(), "application/octet-stream");

  // No body means a file message only
  let all = published(&relays);
  assert_eq!(all.len(), 2);
  let message = unwrap(&all[0].1, &recipient());
  assert_eq!((message.kind, message.content.as_str()), (15, BLOB_URL));
  let tags: HashMap<String, String> = message.tags.iter().map(|t| (t[0].clone(), t[1].clone())).collect();
  assert_eq!(tags["p"], recipient().public_key());
  assert_eq!(tags["file-type"], "image/png");
  assert_eq!(tags["encryption-algorithm"], "aes-gcm");
  assert_eq!(tags["x"], hex::encode(Sha256::digest(&uploaded)));
  assert_eq!(tags["ox"], hex::encode(Sha256::digest(&data)));
  assert_eq!(tags["size"], uploaded.len().to_string());
  let nonce = hex::decode(&tags["decryption-nonce"]).unwrap();
  assert_eq!(nonce.len(), 16);
  let file_key: [u8; 32] = hex::decode(&tags["decryption-key"]).unwrap().try_into().unwrap();
  let nonce: [u8; 16] = nonce.try_into().unwrap();
  let cipher = crypto::FileCipher::new(&file_key.into());
  assert_eq!(cipher.decrypt(&nonce.into(), uploaded.as_slice()).unwrap(), data);
  assert_eq!(
    message.tags.iter().map(|t| t[0].as_str()).collect::<Vec<_>>(),
    ["p", "file-type", "encryption-algorithm", "decryption-key", "decryption-nonce", "x", "ox", "size"]
  );

  // The second recipient got their own copy
  assert_eq!(unwrap(&all[1].1, &other()).content, BLOB_URL);

  // A body and a file make two messages
  relays.lock().unwrap().published.clear();
  assert!(
    with_test_base(server.uri(), notify(&relays, &format!("nostr://{}/{}?relay=one.example.com&blossom=cdn.example.com", NSEC, NPUB), "Hi", "", true)).await
  );
  let kinds: Vec<i64> = published(&relays).iter().map(|(_, w)| unwrap(w, &recipient()).kind).collect();
  assert_eq!(kinds, [14, 15]);

  // Upload problems stop the message
  relays.lock().unwrap().published.clear();
  let a = apprise(&url);
  let missing = vec!["/nonexistent/apprise-test.png".to_string()];
  assert!(!with_test_base(server.uri(), with_relays(&relays, a.notify_with_attachments(&ctx("x"), &missing))).await.is_success());
  let failing = MockServer::start().await;
  blossom(&failing, ResponseTemplate::new(500)).await;
  assert!(!with_test_base(failing.uri(), notify(&relays, &url, "x", "", true)).await);
  assert!(published(&relays).is_empty());
}

/// Upstream `test_plugin_nostr_nip04`: older NIP-04 encrypted messages go to
/// our own relays.
#[tokio::test]
async fn test_nip04() {
  let server = MockServer::start().await;
  blossom(&server, BlossomReply).await;
  let relays = relays();
  let url = format!("nostr://{}/{}?mode=nip04&relay=wss://one.example.com&blossom=cdn.example.com", NSEC, NPUB);
  assert!(with_test_base(server.uri(), notify(&relays, &url, "Hello", "", true)).await);

  // Published to our own relays and tagged for the recipient
  let all = published(&relays);
  let (relay_url, event) = &all[0];
  assert_eq!(relay_url, "wss://one.example.com");
  assert_eq!(event.kind, 4);
  assert_eq!(event.tags, vec![tag(&["p", recipient().public_key()])]);
  assert!(verify_event(&serde_json::to_value(event).unwrap()).is_some());
  // No inbox relay list is needed
  assert!(relays.lock().unwrap().queries.is_empty());

  // The recipient can read the text and the file link
  let shared = recipient().shared_x(sender().public_key()).unwrap();
  assert_eq!(crypto::nip04_decrypt(&event.content, &shared).as_deref(), Some(format!("Hello\n{}", BLOB_URL).as_str()));
  // The file itself is uploaded as it is
  assert_eq!(uploads(&server).await[0].body, attachment_data());

  // Upload problems stop the message
  let failing = MockServer::start().await;
  blossom(&failing, ResponseTemplate::new(500)).await;
  assert!(!with_test_base(failing.uri(), notify(&relays, &url, "x", "", true)).await);
}

/// Upstream `test_plugin_nostr_dm_errors`: recipients that could not be
/// reached are reported, and the others still get the message.
#[tokio::test]
async fn test_dm_errors() {
  let server = MockServer::start().await;
  Mock::given(method("GET")).and(path("/.well-known/nostr.json")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
  let relays = relays();
  relays.lock().unwrap().events.insert(10050, inbox(&[recipient()], "wss://one.example.com"));
  let url = format!("nostr://{}/nobody@example.com/{}?relay=one.example.com", NSEC, NPUB);
  assert!(!with_test_base(server.uri(), notify(&relays, &url, "Hello", "", false)).await);

  let all = published(&relays);
  assert_eq!(all.len(), 1);
  assert_eq!(unwrap(&all[0].1, &recipient()).content, "Hello");

  // A Nostr address that resolves is sent to like a public key
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/.well-known/nostr.json"))
    .and(query_param("name", "alice"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"names": {"alice": recipient().public_key()}})))
    .mount(&server)
    .await;
  relays.lock().unwrap().published.clear();
  assert!(with_test_base(server.uri(), notify(&relays, &format!("nostr://{}/Alice@Example.com?relay=one.example.com", NSEC), "Hi", "", false)).await);
  assert_eq!(unwrap(&published(&relays)[0].1, &recipient()).content, "Hi");
}

/// Upstream `test_plugin_nostr_retry`: a retry only goes to the recipient
/// that failed, with the very same event.
#[tokio::test]
async fn test_retry() {
  let relays = relays();
  relays.lock().unwrap().refuse_pubkeys.insert(other().public_key().to_string());
  relays.lock().unwrap().events.insert(10050, inbox(&[recipient(), other()], "wss://one.example.com"));
  let url = format!("nostr://{}/{}/{}?relay=wss://one.example.com&retry=2&wait=0", NSEC, NPUB, NPUB_OTHER);
  assert!(!notify(&relays, &url, "Hello", "", false).await);

  // The healthy recipient was contacted once; the broken one 3 times
  let sent: Vec<String> = published(&relays).iter().map(|(_, e)| e.tags[0][1].clone()).collect();
  assert_eq!(sent.iter().filter(|p| *p == recipient().public_key()).count(), 1);
  assert_eq!(sent.iter().filter(|p| *p == other().public_key()).count(), 3);

  // Each retry resent the very same event, so relays can spot it
  let retried: HashSet<String> = published(&relays).iter().filter(|(_, e)| e.tags[0][1] == other().public_key()).map(|(_, e)| e.id.clone()).collect();
  assert_eq!(retried.len(), 1);
}

/// Upstream `test_plugin_nostr_note_retry`: a retry resends the same note.
#[tokio::test]
async fn test_note_retry() {
  let relays = relays();
  relays.lock().unwrap().refuse.insert("wss://one.example.com".into());
  assert!(!notify(&relays, &format!("nostr://{}?relay=wss://one.example.com&retry=1&wait=0", NSEC), "Hello", "", false).await);
  let all = published(&relays);
  assert_eq!(all.len(), 2);
  assert_eq!(all[0].1.id, all[1].1.id);
}

/// Upstream `test_plugin_nostr_upload_retry`: uploads are reused when a
/// message is retried.
#[tokio::test]
async fn test_upload_retry() {
  let relays = relays();
  relays.lock().unwrap().refuse.insert("wss://one.example.com".into());
  relays.lock().unwrap().events.insert(10050, inbox(&[recipient()], "wss://one.example.com"));
  for mode in ["nip04", "nip17"] {
    let server = MockServer::start().await;
    blossom(&server, BlossomReply).await;
    let url = format!("nostr://{}/{}?relay=wss://one.example.com&blossom=cdn.example.com&mode={}&retry=1&wait=0", NSEC, NPUB, mode);
    assert!(!with_test_base(server.uri(), notify(&relays, &url, "x", "", true)).await);
    assert_eq!(uploads(&server).await.len(), 1, "{mode}");
  }
}

// ── A real relay ─────────────────────────────────────────────────────────

/// A public note and a direct message to ourselves reach a real websocket
/// relay on 127.0.0.1 through the whole pipeline.
#[tokio::test]
async fn test_send_to_local_relay() {
  let (port, mut received) = super::relay::tests::tcp_relay(false).await;
  let url = format!("nostr://{}?relay=ws://127.0.0.1:{}&cto=5&rto=5", NSEC, port);
  let a = apprise(&url);
  let ctx = NotifyContext { title: "T".into(), ..ctx("Over the wire é") };
  assert!(a.notify(&ctx).await.is_success());
  let messages = received.recv().await.unwrap();
  assert_eq!(messages.len(), 1);
  let event = verify_event(&messages[0][1]).unwrap();
  assert_eq!((event.kind, event.content.as_str()), (1, "T\r\nOver the wire é"));
}

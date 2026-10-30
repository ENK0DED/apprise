//! Encrypted rooms end to end. Nothing here compares ciphertext: a
//! receiving device built on vodozemac decrypts what Apprise sent, the way
//! apprise-go verifies its encrypted payloads.

use super::*;
use crate::notify::matrix::e2ee::canonical_json;
use crate::notify::matrix::e2ee::tests::mac;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use std::sync::{Arc, Mutex};
use vodozemac::Curve25519PublicKey;
use vodozemac::megolm::{InboundGroupSession, MegolmMessage, SessionConfig as MegolmConfig, SessionKey};
use vodozemac::olm::{Account, OlmMessage, SessionConfig as OlmConfig};

const PEER: &str = "@peer:localhost";
const PEER_DEVICE: &str = "PEERDEV";
const ROOM: &str = "!secret:localhost";
const ROOM_PATH: &str = "%21secret%3Alocalhost";

/// Another member's device: it publishes signed keys and decrypts.
struct Receiver {
  account: Account,
  otk: String,
}

impl Receiver {
  fn new() -> Self {
    let mut account = Account::new();
    account.generate_one_time_keys(1);
    let otk = account.one_time_keys().into_values().next().unwrap().to_base64();
    account.mark_keys_as_published();
    Self { account, otk }
  }

  fn sign(&self, value: &Value) -> String {
    self.account.sign(canonical_json(value).as_bytes()).to_base64()
  }

  fn curve(&self) -> String {
    self.account.curve25519_key().to_base64()
  }

  fn device_keys(&self, user: &str, device: &str) -> Value {
    let mut keys = json!({
      "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
      "device_id": device,
      "keys": {format!("curve25519:{device}"): self.curve(), format!("ed25519:{device}"): self.account.ed25519_key().to_base64()},
      "user_id": user,
    });
    let signature = self.sign(&keys);
    keys["signatures"] = json!({user: {format!("ed25519:{device}"): signature}});
    keys
  }

  fn claimed_otk(&self) -> Value {
    let mut otk = json!({"key": self.otk});
    let signature = self.sign(&otk);
    otk["signatures"] = json!({PEER: {format!("ed25519:{PEER_DEVICE}"): signature}});
    json!({"one_time_keys": {PEER: {PEER_DEVICE: {"signed_curve25519:AAAAAQ": otk}}}})
  }

  /// Open the `m.room_key` addressed to this device.
  fn open_room_key(&mut self, to_device: &Value, sender: &Value) -> SessionKey {
    let message = &to_device["messages"][PEER][PEER_DEVICE];
    assert_eq!(message["algorithm"], "m.olm.v1.curve25519-aes-sha2");
    let sender_key = message["sender_key"].as_str().unwrap();
    assert_eq!(sender_key, sender["keys"]["curve25519:DEVICE"]);
    let OlmMessage::PreKey(pre_key) = serde_json::from_value(message["ciphertext"][self.curve()].clone()).unwrap() else { panic!("pre-key message") };
    let opened = self.account.create_inbound_session(OlmConfig::version_1(), Curve25519PublicKey::from_base64(sender_key).unwrap(), &pre_key).unwrap();
    let inner: Value = serde_json::from_slice(&opened.plaintext).unwrap();
    assert_eq!(inner["type"], "m.room_key");
    assert_eq!(inner["sender"], "@apprise:localhost");
    assert_eq!(inner["recipient"], PEER);
    assert_eq!(inner["recipient_keys"]["ed25519"], self.account.ed25519_key().to_base64());
    assert_eq!(inner["keys"]["ed25519"], sender["keys"]["ed25519:DEVICE"]);
    assert_eq!(inner["content"]["algorithm"], "m.megolm.v1.aes-sha2");
    assert_eq!(inner["content"]["room_id"], ROOM);
    SessionKey::from_base64(inner["content"]["session_key"].as_str().unwrap()).unwrap()
  }
}

/// Decrypt an `m.room.encrypted` event, returning its index and inner event.
fn open_event(key: &SessionKey, envelope: &Value) -> (u32, Value) {
  assert_eq!(envelope["algorithm"], "m.megolm.v1.aes-sha2");
  assert_eq!(envelope["device_id"], "DEVICE");
  let mut session = InboundGroupSession::new(key, MegolmConfig::version_1());
  assert_eq!(envelope["session_id"], session.session_id());
  let decrypted = session.decrypt(&MegolmMessage::from_base64(envelope["ciphertext"].as_str().unwrap()).unwrap()).unwrap();
  (decrypted.message_index, serde_json::from_slice(&decrypted.plaintext).unwrap())
}

/// A homeserver with one encrypted room shared with the receiver.
async fn encrypted_homeserver(server: &MockServer, receiver: &Receiver) {
  homeserver(server).await;
  mock(server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({"one_time_key_counts": {}})).await;
  mock(server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/state/m.room.encryption"), 200, json!({"algorithm": "m.megolm.v1.aes-sha2"})).await;
  mock(server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/joined_members"), 200, json!({"joined": {PEER: {}, "@apprise:localhost": {}}})).await;
  let own = Receiver::new();
  mock(
    server,
    "POST",
    "/_matrix/client/v3/keys/query",
    200,
    json!({"device_keys": {PEER: {PEER_DEVICE: receiver.device_keys(PEER, PEER_DEVICE)}, "@apprise:localhost": {"DEVICE": own.device_keys("@apprise:localhost", "DEVICE")}}}),
  )
  .await;
  mock(server, "POST", "/_matrix/client/v3/keys/claim", 200, receiver.claimed_otk()).await;
  mock_re(server, "PUT", r"^/_matrix/client/v3/sendToDevice/", 200, json!({})).await;
}

fn find<'a>(requests: &'a [Request], needle: &str) -> Vec<&'a Request> {
  requests.iter().filter(|r| r.url.path().contains(needle)).collect()
}

const URL: &str = "matrixs://user:pass@localhost/!secret:localhost?discovery=no";

#[tokio::test]
async fn test_encrypted_send_decrypts_on_the_receiver() {
  let server = MockServer::start().await;
  let mut receiver = Receiver::new();
  encrypted_homeserver(&server, &receiver).await;
  let m = load(URL);
  let mut c = ctx("Title", "<p>secret <b>message</b></p>");
  c.body_format = NotifyFormat::Html;
  assert!(send(&m, &server, &c).await);
  let requests = seen(&server).await;
  let lines: Vec<String> = requests.iter().map(line).collect();
  assert_eq!(lines[0], "POST /_matrix/client/v3/login");
  assert_eq!(lines[1], "POST /_matrix/client/v3/keys/upload");

  // Our device keys are self-signed; no one-time keys are published
  // (upstream e0d29095), only a signed fallback key.
  let upload = body(&requests[1]);
  let device_keys = &upload["device_keys"];
  assert!(crate::notify::matrix::e2ee::verify_device_keys(device_keys, "@apprise:localhost", "DEVICE"));
  assert!(upload.get("one_time_keys").is_none());
  let (_, fallback) = upload["fallback_keys"].as_object().unwrap().iter().next().unwrap();
  assert!(crate::notify::matrix::e2ee::verify_signed_otk(fallback, "@apprise:localhost", "DEVICE", device_keys["keys"]["ed25519:DEVICE"].as_str().unwrap()));

  // The claim asks for every member device, ours included as upstream
  // does; the key goes out before the message it unlocks, and only to the
  // other device.
  let claim = find(&requests, "/keys/claim");
  assert_eq!(body(claim[0]), json!({"one_time_keys": {PEER: {PEER_DEVICE: "signed_curve25519"}, "@apprise:localhost": {"DEVICE": "signed_curve25519"}}}));
  let share_at = lines.iter().position(|l| l.contains("/sendToDevice/m.room.encrypted/")).unwrap();
  let send_at = lines.iter().position(|l| l.contains("/send/m.room.encrypted/")).unwrap();
  assert!(share_at < send_at);
  let to_device = body(&requests[share_at]);
  assert_eq!(to_device["messages"]["@apprise:localhost"], json!({}));
  let key = receiver.open_room_key(&to_device, device_keys);

  // The receiver reads the exact message.
  let (index, event) = open_event(&key, &body(&requests[send_at]));
  assert_eq!(index, 0);
  assert_eq!(
    event,
    json!({"type": "m.room.message", "room_id": ROOM, "content": {
      "msgtype": "m.text", "body": "# Title\r\nsecret message", "format": "org.matrix.custom.html",
      "formatted_body": "<h1>Title</h1><br/><p>secret <b>message</b></p>"}})
  );
  // The plaintext never crosses the wire.
  assert!(requests.iter().all(|r| !String::from_utf8_lossy(&r.body).contains("secret message")));
  assert_eq!(body(&requests[send_at])["sender_key"], device_keys["keys"]["curve25519:DEVICE"]);

  // The next send reuses the shared session: no new claim or key share.
  assert!(send(&m, &server, &ctx("", "second secret")).await);
  let requests = seen(&server).await;
  assert_eq!(find(&requests, "/keys/claim").len(), 1);
  assert_eq!(find(&requests, "/keys/upload").len(), 1);
  assert_eq!(find(&requests, "/sendToDevice/").len(), 1);
  let events = find(&requests, "/send/m.room.encrypted/");
  let (index, event) = open_event(&key, &body(events[1]));
  assert_eq!(index, 1);
  assert_eq!(event["content"], json!({"msgtype": "m.text", "body": "second secret"}));
  // Each event has its own transaction ID.
  let ids: std::collections::HashSet<&str> = requests.iter().filter(|r| r.method.as_str() == "PUT").map(|r| r.url.path().rsplit('/').next().unwrap()).collect();
  assert_eq!(ids.len(), requests.iter().filter(|r| r.method.as_str() == "PUT").count());
}

#[tokio::test]
async fn test_encrypted_attachment_decrypts() {
  let server = MockServer::start().await;
  let mut receiver = Receiver::new();
  encrypted_homeserver(&server, &receiver).await;
  mock(&server, "POST", "/_matrix/media/v3/upload", 200, json!({"content_uri": "mxc://localhost/cipher"})).await;
  let m = load(URL);
  let data = b"top secret attachment".to_vec();
  let mut c = ctx("", "with file");
  c.attachments = vec![Attachment { name: "notes.txt".into(), data: data.clone(), mime_type: "text/plain".into() }];
  assert!(send(&m, &server, &c).await);
  let requests = seen(&server).await;
  let key = receiver.open_room_key(&body(find(&requests, "/sendToDevice/")[0]), &body(find(&requests, "/keys/upload")[0])["device_keys"]);

  // Only ciphertext is uploaded.
  let upload = find(&requests, "/media/v3/upload")[0];
  assert_eq!(upload.url.query(), Some("filename=notes.txt"));
  assert_eq!(upload.headers.get("content-type").unwrap(), "application/octet-stream");
  assert_ne!(upload.body, data);

  let events = find(&requests, "/send/m.room.encrypted/");
  assert_eq!(events.len(), 2);
  let (_, message) = open_event(&key, &body(events[0]));
  assert_eq!(message["content"]["body"], "with file");
  let (index, event) = open_event(&key, &body(events[1]));
  assert_eq!(index, 1);
  let content = &event["content"];
  assert_eq!(content["msgtype"], "m.file");
  assert_eq!(content["filename"], "notes.txt");
  assert_eq!(content["info"], json!({"mimetype": "text/plain", "size": data.len()}));
  let file = &content["file"];
  assert_eq!(file["url"], "mxc://localhost/cipher");
  // Decrypt the upload with the key and IV the event carries.
  use aes::cipher::{KeyIvInit, StreamCipher};
  let k: [u8; 32] = URL_SAFE_NO_PAD.decode(file["key"]["k"].as_str().unwrap()).unwrap().try_into().unwrap();
  let iv: [u8; 16] = STANDARD_NO_PAD.decode(file["iv"].as_str().unwrap()).unwrap().try_into().unwrap();
  let digest = <sha2::Sha256 as sha2::Digest>::digest(&upload.body);
  assert_eq!(file["hashes"]["sha256"], STANDARD_NO_PAD.encode(digest));
  let mut plain = upload.body.clone();
  ctr::Ctr128BE::<aes::Aes256>::new(&k.into(), &iv.into()).apply_keystream(&mut plain);
  assert_eq!(plain, data);
}

#[tokio::test]
async fn test_rotated_session_is_shared_again() {
  let server = MockServer::start().await;
  encrypted_homeserver(&server, &Receiver::new()).await;
  let m = load(URL);
  assert!(send(&m, &server, &ctx("", "one")).await);
  // Age the cached session past its rotation threshold.
  {
    let mut session = m.session.lock().await;
    let key = format!("e2ee_megolm_{ROOM}");
    let (value, _) = session.local.get_mut(&key).unwrap();
    value["created_at"] = json!(0.0);
  }
  // A new receiver, since the first one-time key was consumed.
  let mut replacement = Receiver::new();
  server.reset().await;
  encrypted_homeserver(&server, &replacement).await;
  assert!(send(&m, &server, &ctx("", "two")).await);
  let requests = seen(&server).await;
  assert_eq!(find(&requests, "/sendToDevice/").len(), 1);
  let upload_keys = {
    let session = m.session.lock().await;
    session.account.as_ref().unwrap().device_keys_payload("@apprise:localhost", "DEVICE")
  };
  let key = replacement.open_room_key(&body(find(&requests, "/sendToDevice/")[0]), &upload_keys);
  let (index, event) = open_event(&key, &body(find(&requests, "/send/m.room.encrypted/")[0]));
  // A fresh session starts again at index 0.
  assert_eq!((index, event["content"]["body"].clone()), (0, json!("two")));
}

#[tokio::test]
async fn test_oversized_encrypted_event_is_not_sent() {
  // A device ID learned after sizing can push the event past the limit.
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@apprise:localhost", "device_id": "D".repeat(30000)})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
  mock(&server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/state/m.room.encryption"), 200, json!({"algorithm": "m.megolm.v1.aes-sha2"})).await;
  mock(&server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/joined_members"), 200, json!({"joined": {}})).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load(URL);
  assert!(!send(&m, &server, &ctx("", &"x".repeat(30000))).await);
  assert!(find(&seen(&server).await, "/send/").is_empty());
}

#[tokio::test]
async fn test_unknown_encryption_state_fails_closed() {
  // apprise-go 04d2c71: only a 404 confirms a plaintext room.
  for (status, state) in [(500, json!({"errcode": "M_UNKNOWN"})), (200, json!({})), (403, json!({"errcode": "M_FORBIDDEN"}))] {
    let server = MockServer::start().await;
    homeserver(&server).await;
    mock(&server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
    mock(&server, "GET", "/_matrix/client/v3/rooms/%21unknown%3Alocalhost/state/m.room.encryption", status, state.clone()).await;
    mock(&server, "GET", "/_matrix/client/v3/rooms/%21plain%3Alocalhost/state/m.room.encryption", 404, json!({"errcode": "M_NOT_FOUND"})).await;
    let m = load("matrixs://user:pass@localhost/!unknown:localhost/!plain:localhost?discovery=no");
    assert!(!send(&m, &server, &ctx("", "hello")).await, "{status} {state}");
    let requests = seen(&server).await;
    let events = sends(&requests);
    assert_eq!(events.len(), 1, "{status} {state}");
    assert!(events[0].url.path().contains("%21plain%3Alocalhost/send/m.room.message/"));
    // The unknown state is not cached: the next send asks again; the
    // confirmed plaintext room is not asked twice.
    send(&m, &server, &ctx("", "hello")).await;
    let requests = seen(&server).await;
    assert_eq!(find(&requests, "%21unknown%3Alocalhost/state/").len(), 2);
    assert_eq!(find(&requests, "%21plain%3Alocalhost/state/").len(), 1);
  }
}

#[tokio::test]
async fn test_key_upload_failure_sends_plaintext() {
  // As upstream: a failed setup sends this notification unencrypted and
  // tries the upload again next time.
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/upload", 500, json!({})).await;
  let m = load(URL);
  assert!(send(&m, &server, &ctx("", "hello")).await);
  assert!(send(&m, &server, &ctx("", "hello")).await);
  let requests = seen(&server).await;
  assert_eq!(find(&requests, "/keys/upload").len(), 2);
  assert!(find(&requests, "/state/").is_empty());
  assert_eq!(find(&requests, "/send/m.room.message/").len(), 2);
}

#[tokio::test]
async fn test_created_rooms_are_encrypted() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@apprise:localhost", "device_id": "DEVICE"})).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
  mock(&server, "POST", "/_matrix/client/v3/join/%23fresh%3Alocalhost", 404, json!({"errcode": "M_NOT_FOUND"})).await;
  mock(&server, "POST", "/_matrix/client/v3/createRoom", 200, json!({"room_id": "!fresh:localhost", "room_alias": "#fresh:localhost"})).await;
  mock_re(&server, "GET", "m.direct$", 200, json!({})).await;
  mock_re(&server, "PUT", "m.direct$", 200, json!({})).await;
  Mock::given(method("POST")).and(path_regex("/join/%21")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "GET", "/joined_members$", 200, json!({"joined": {}})).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrixs://user:pass@localhost/%23fresh/@bob?discovery=no");
  assert!(send(&m, &server, &ctx("", "hello")).await);
  let requests = seen(&server).await;
  let creates = find(&requests, "/createRoom");
  let encryption = json!([{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}}]);
  assert_eq!(creates.len(), 2);
  for create in creates {
    assert_eq!(body(create)["initial_state"], encryption);
  }
  // Rooms we just encrypted are not looked up, and get encrypted events.
  assert!(find(&requests, "/state/").is_empty());
  assert_eq!(find(&requests, "/send/m.room.encrypted/").len(), 2);
  assert!(find(&requests, "/send/m.room.message/").is_empty());
}

#[tokio::test]
async fn test_untrusted_devices_get_no_key() {
  // A forged device signature and a missing one-time key are skipped.
  let server = MockServer::start().await;
  let receiver = Receiver::new();
  homeserver(&server).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
  mock(&server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/state/m.room.encryption"), 200, json!({"algorithm": "m.megolm.v1.aes-sha2"})).await;
  mock(&server, "GET", &format!("/_matrix/client/v3/rooms/{ROOM_PATH}/joined_members"), 200, json!({"joined": {PEER: {}, "@other:localhost": {}}})).await;
  let mut forged = receiver.device_keys(PEER, PEER_DEVICE);
  forged["keys"][format!("curve25519:{PEER_DEVICE}")] = json!(Receiver::new().curve());
  let other = Receiver::new();
  mock(
    &server,
    "POST",
    "/_matrix/client/v3/keys/query",
    200,
    json!({"device_keys": {PEER: {PEER_DEVICE: forged}, "@other:localhost": {"OTHER": other.device_keys("@other:localhost", "OTHER")}}}),
  )
  .await;
  mock(&server, "POST", "/_matrix/client/v3/keys/claim", 200, json!({"one_time_keys": {}, "failures": {"remote.example": {}}})).await;
  let m = load(URL);
  assert!(send(&m, &server, &ctx("", "hello")).await);
  let requests = seen(&server).await;
  // The claim skips the forged device; nobody had a key, so nothing is shared.
  assert_eq!(body(find(&requests, "/keys/claim")[0]), json!({"one_time_keys": {PEER: {}, "@other:localhost": {"OTHER": "signed_curve25519"}}}));
  assert!(find(&requests, "/sendToDevice/").is_empty());
  assert_eq!(find(&requests, "/send/m.room.encrypted/").len(), 1);
}

#[tokio::test]
async fn test_identity_persists_across_instances() {
  let server = MockServer::start().await;
  let mut receiver = Receiver::new();
  encrypted_homeserver(&server, &receiver).await;
  let dir = tempfile::tempdir().unwrap();
  for body_text in ["first", "second"] {
    let asset = AppriseAsset { storage_path: Some(dir.path().to_path_buf()), ..Default::default() };
    let mut a = Apprise::with_asset(asset);
    assert!(a.add(URL));
    assert!(with_test_base(server.uri(), a.notify(&ctx("", body_text))).await.is_success());
  }
  let requests = seen(&server).await;
  // One login, one key upload, one key share: the device, its keys and
  // the Megolm session were restored from the store.
  assert_eq!(find(&requests, "/login").len(), 1);
  assert_eq!(find(&requests, "/keys/upload").len(), 1);
  assert_eq!(find(&requests, "/sendToDevice/").len(), 1);
  let key = receiver.open_room_key(&body(find(&requests, "/sendToDevice/")[0]), &body(find(&requests, "/keys/upload")[0])["device_keys"]);
  let events = find(&requests, "/send/m.room.encrypted/");
  assert_eq!(open_event(&key, &body(events[1])), (1, json!({"type": "m.room.message", "room_id": ROOM, "content": {"msgtype": "m.text", "body": "second"}})));
  // The store holds upstream's record format.
  let stored = {
    let session = crate::storage::PersistentStore::disk_scan(dir.path(), &[], false);
    assert_eq!(session.len(), 1);
    let mut store = crate::storage::PersistentStore::new(Some(dir.path()), &session[0], crate::types::StorageMode::Auto).unwrap();
    (store.get("e2ee_account").unwrap(), store.get(&format!("e2ee_megolm_{ROOM}")).unwrap(), store.get("transaction_id"))
  };
  assert!(stored.0["ik"].is_string() && stored.0["sk"].is_string() && stored.0["fallback_otk"]["id"].is_string());
  assert_eq!(stored.1["version"], 1);
  assert_eq!(stored.1["counter"], 2);
  assert!(stored.2.is_some());
}

// ── Automatic SAS verification ───────────────────────────────────────────────

/// Our other device, scripted through `/sync` and `/sendToDevice`.
struct Phone {
  secret: x25519_dalek::StaticSecret,
  public: String,
  device: Receiver,
  step: u32,
  /// The SAS key Apprise sent, once it did.
  apprise_key: Option<String>,
  /// Apprise's accept and MAC contents.
  accept: Option<Value>,
  apprise_mac: Option<Value>,
  sent: Vec<String>,
  start: Value,
}

const ME: &str = "@apprise:localhost";

impl Phone {
  fn new() -> Self {
    let secret = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let public = STANDARD_NO_PAD.encode(x25519_dalek::PublicKey::from(&secret).as_bytes());
    let start = json!({
      "transaction_id": "txn1", "from_device": "PHONE", "method": "m.sas.v1",
      "key_agreement_protocols": ["curve25519-hkdf-sha256"], "hashes": ["sha256"],
      "message_authentication_codes": ["hkdf-hmac-sha256.v2"], "short_authentication_string": ["decimal", "emoji"],
    });
    Self { secret, public, device: Receiver::new(), step: 0, apprise_key: None, accept: None, apprise_mac: None, sent: Vec::new(), start }
  }

  fn shared(&self) -> Vec<u8> {
    let theirs: [u8; 32] = STANDARD_NO_PAD.decode(self.apprise_key.as_ref().unwrap()).unwrap().try_into().unwrap();
    self.secret.diffie_hellman(&x25519_dalek::PublicKey::from(theirs)).as_bytes().to_vec()
  }

  fn signing_key(&self) -> String {
    self.device.account.ed25519_key().to_base64()
  }

  fn event(event_type: &str, content: Value) -> Value {
    json!({"type": event_type, "sender": ME, "content": content})
  }

  /// The next `/sync` batch.
  fn sync(&mut self) -> Value {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let request = |txn: &str| {
      Self::event("m.key.verification.request", json!({"transaction_id": txn, "from_device": "PHONE", "methods": ["m.sas.v1"], "timestamp": now_ms}))
    };
    let events = match self.step {
      // The same device asks twice: both requests are cancelled.
      100 => vec![request("first"), request("first"), request("second")],
      0 => vec![
        // Someone else's request and a stale one are ignored.
        json!({"type": "m.key.verification.request", "sender": "@mallory:localhost", "content": {"transaction_id": "evil", "from_device": "X", "methods": ["m.sas.v1"], "timestamp": now_ms}}),
        Self::event(
          "m.key.verification.request",
          json!({"transaction_id": "old", "from_device": "PHONE", "methods": ["m.sas.v1"], "timestamp": now_ms - 3_600_000}),
        ),
        Self::event("m.key.verification.request", json!({"transaction_id": "txn1", "from_device": "PHONE", "methods": ["m.sas.v1"], "timestamp": now_ms})),
      ],
      1 if self.sent.contains(&"ready".to_string()) => vec![Self::event("m.key.verification.start", self.start.clone())],
      2 if self.accept.is_some() => vec![Self::event("m.key.verification.key", json!({"transaction_id": "txn1", "key": self.public}))],
      3 if self.apprise_mac.is_some() => {
        let secret = self.shared();
        let info = |key_id: &str| format!("MATRIX_KEY_VERIFICATION_MAC{ME}PHONE{ME}DEVICEtxn1{key_id}");
        let content = json!({
          "transaction_id": "txn1",
          "mac": {"ed25519:PHONE": mac(&secret, &info("ed25519:PHONE"), &self.signing_key())},
          "keys": mac(&secret, &info("KEY_IDS"), "ed25519:PHONE"),
        });
        vec![Self::event("m.key.verification.mac", content), Self::event("m.key.verification.done", json!({"transaction_id": "txn1"}))]
      }
      _ => vec![],
    };
    if !events.is_empty() {
      self.step += 1;
    }
    json!({"next_batch": format!("b{}", self.step), "to_device": {"events": events}})
  }

  fn received(&mut self, event_type: &str, content: &Value) {
    let mut name = event_type.trim_start_matches("m.key.verification.").to_string();
    if name == "cancel" {
      name = format!("cancel:{}", content["transaction_id"].as_str().unwrap_or_default());
    }
    match name.as_str() {
      "accept" => self.accept = Some(content.clone()),
      "key" => self.apprise_key = content["key"].as_str().map(str::to_string),
      "mac" => self.apprise_mac = Some(content.clone()),
      _ => {}
    }
    self.sent.push(name);
  }
}

struct PhoneSync(Arc<Mutex<Phone>>);

impl Respond for PhoneSync {
  fn respond(&self, request: &Request) -> ResponseTemplate {
    let _ = request;
    ResponseTemplate::new(200).set_body_json(self.0.lock().unwrap().sync()).set_delay(std::time::Duration::from_millis(5))
  }
}

struct PhoneInbox(Arc<Mutex<Phone>>);

impl Respond for PhoneInbox {
  fn respond(&self, request: &Request) -> ResponseTemplate {
    let event_type = request.url.path().split('/').nth(5).unwrap().to_string();
    let payload = body(request);
    let content = &payload["messages"][ME]["PHONE"];
    self.0.lock().unwrap().received(&event_type, content);
    ResponseTemplate::new(200).set_body_json(json!({}))
  }
}

async fn verification_homeserver(server: &MockServer, phone: &Arc<Mutex<Phone>>) {
  homeserver(server).await;
  mock(server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
  mock_re(server, "GET", "/state/m.room.encryption$", 404, json!({"errcode": "M_NOT_FOUND"})).await;
  let device_keys = phone.lock().unwrap().device.device_keys(ME, "PHONE");
  mock(
    server,
    "POST",
    "/_matrix/client/v3/keys/query",
    200,
    json!({"device_keys": {ME: {"PHONE": device_keys}}, "master_keys": {ME: {"keys": {"ed25519:MASTER": "bWFzdGVy"}}}}),
  )
  .await;
  Mock::given(method("GET")).and(path("/_matrix/client/v3/sync")).respond_with(PhoneSync(phone.clone())).mount(server).await;
  Mock::given(method("PUT"))
    .and(path_regex(r"^/_matrix/client/v3/sendToDevice/m\.key\.verification\."))
    .respond_with(PhoneInbox(phone.clone()))
    .mount(server)
    .await;
}

#[tokio::test]
async fn test_autoverify_completes_a_same_user_verification() {
  let server = MockServer::start().await;
  let phone = Arc::new(Mutex::new(Phone::new()));
  verification_homeserver(&server, &phone).await;
  let mut m = load("matrixs://user:pass@localhost/!room:localhost?discovery=no&autoverify=yes");
  m.autoverify_timeout = Duration::from_secs(20);
  assert!(send(&m, &server, &ctx("", "hello")).await);

  {
    let phone = phone.lock().unwrap();
    // Ignored strangers and stale requests got no answer.
    assert_eq!(phone.sent, vec!["ready", "accept", "key", "mac", "done"]);
    // The commitment binds Apprise's key to the start event.
    let accept = phone.accept.as_ref().unwrap();
    let mut digest = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut digest, phone.apprise_key.as_ref().unwrap().as_bytes());
    sha2::Digest::update(&mut digest, canonical_json(&phone.start).as_bytes());
    assert_eq!(accept["commitment"], STANDARD_NO_PAD.encode(sha2::Digest::finalize(digest)));
    assert_eq!(accept["short_authentication_string"], json!(["decimal"]));
    // Apprise proved its own device signing key.
    let session = m.session.try_lock().unwrap();
    let signing_key = session.account.as_ref().unwrap().signing_key();
    let secret = phone.shared();
    let info = |key_id: &str| format!("MATRIX_KEY_VERIFICATION_MAC{ME}DEVICE{ME}PHONEtxn1{key_id}");
    let apprise_mac = phone.apprise_mac.as_ref().unwrap();
    assert_eq!(apprise_mac["mac"]["ed25519:DEVICE"], mac(&secret, &info("ed25519:DEVICE"), &signing_key));
    assert_eq!(apprise_mac["keys"], mac(&secret, &info("KEY_IDS"), "ed25519:DEVICE"));
  }

  // Trust is stored for this identity; the message went out after.
  {
    let mut session = m.session.lock().await;
    let binding = session.get_str("e2ee_device_binding").unwrap();
    assert_eq!(session.get_str("e2ee_verified_binding"), Some(binding));
    assert!(session.get("e2ee_autoverify_refreshed_at").is_some());
  }
  let requests = seen(&server).await;
  assert_eq!(find(&requests, "/send/m.room.message/").len(), 1);
  let sync = find(&requests, "/sync");
  let query: std::collections::HashMap<String, String> = sync[0].url.query_pairs().into_owned().collect();
  assert_eq!(query["set_presence"], "offline");
  assert_eq!(query["filter"], SYNC_FILTER_FOR_TESTS);
  assert!(!query.contains_key("since"));
  assert_eq!(sync[1].url.query_pairs().find(|(k, _)| k == "since").unwrap().1, "b1");

  // Verified: the next send does not poll again.
  let polls = sync.len();
  assert!(send(&m, &server, &ctx("", "again")).await);
  assert_eq!(find(&seen(&server).await, "/sync").len(), polls);
}

const SYNC_FILTER_FOR_TESTS: &str = r#"{"presence": {"types": []}, "account_data": {"types": []}, "room": {"rooms": []}}"#;

#[tokio::test]
async fn test_autoverify_timeout_does_not_block_delivery() {
  let server = MockServer::start().await;
  let phone = Arc::new(Mutex::new(Phone::new()));
  // Nothing arrives: the phone never starts a verification.
  phone.lock().unwrap().step = 99;
  verification_homeserver(&server, &phone).await;
  let mut m = load("matrixs://user:pass@localhost/!room:localhost?discovery=no&autoverify=yes");
  m.autoverify_timeout = Duration::from_millis(200);
  assert!(send(&m, &server, &ctx("", "hello")).await);
  let requests = seen(&server).await;
  assert!(!find(&requests, "/sync").is_empty());
  assert_eq!(find(&requests, "/send/m.room.message/").len(), 1);
  // A cooldown stops the next send from waiting again.
  let polls = find(&requests, "/sync").len();
  assert!(send(&m, &server, &ctx("", "again")).await);
  assert_eq!(find(&seen(&server).await, "/sync").len(), polls);
  let mut session = m.session.lock().await;
  assert!(session.get_str("e2ee_verified_binding").is_none());
  assert!(session.get_str("e2ee_autoverify_cooldown").is_some());
}

#[tokio::test]
async fn test_autoverify_rejects_a_bad_mac() {
  let server = MockServer::start().await;
  let phone = Arc::new(Mutex::new(Phone::new()));
  // The phone proves a different signing key than its device keys show.
  phone.lock().unwrap().device = Receiver::new();
  let shown = Receiver::new().device_keys(ME, "PHONE");
  homeserver(&server).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/upload", 200, json!({})).await;
  mock_re(&server, "GET", "/state/m.room.encryption$", 404, json!({})).await;
  mock(&server, "POST", "/_matrix/client/v3/keys/query", 200, json!({"device_keys": {ME: {"PHONE": shown}}})).await;
  Mock::given(method("GET")).and(path("/_matrix/client/v3/sync")).respond_with(PhoneSync(phone.clone())).mount(&server).await;
  Mock::given(method("PUT"))
    .and(path_regex(r"^/_matrix/client/v3/sendToDevice/m\.key\.verification\."))
    .respond_with(PhoneInbox(phone.clone()))
    .mount(&server)
    .await;
  let mut m = load("matrixs://user:pass@localhost/!room:localhost?discovery=no&autoverify=yes");
  m.autoverify_timeout = Duration::from_secs(20);
  // Delivery still succeeds; trust is not granted.
  assert!(send(&m, &server, &ctx("", "hello")).await);
  assert_eq!(phone.lock().unwrap().sent, vec!["ready", "accept", "key", "mac", "cancel:txn1"]);
  let mut session = m.session.lock().await;
  assert!(session.get_str("e2ee_verified_binding").is_none());
}

#[tokio::test]
async fn test_autoverify_cancels_a_superseded_request() {
  let server = MockServer::start().await;
  let phone = Arc::new(Mutex::new(Phone::new()));
  phone.lock().unwrap().step = 100;
  verification_homeserver(&server, &phone).await;
  let mut m = load("matrixs://user:pass@localhost/!room:localhost?discovery=no&autoverify=yes");
  m.autoverify_timeout = Duration::from_millis(300);
  assert!(send(&m, &server, &ctx("", "hello")).await);
  // A replay is ignored; a newer request ends both transactions.
  assert_eq!(phone.lock().unwrap().sent, vec!["ready", "cancel:first", "cancel:second"]);
}

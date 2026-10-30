use super::*;
use hmac::{KeyInit, Mac as _};
use vodozemac::megolm::{InboundGroupSession, MegolmMessage, SessionKey};
use vodozemac::olm::{OlmMessage, PreKeyMessage};

fn vectors() -> Value {
  serde_json::from_str(include_str!("../testdata/upstream_e2ee.json")).unwrap()
}

// ── Upstream store format and wire compatibility ──────────────────────────────

#[test]
fn test_account_reads_upstream_record() {
  // Keys and signatures computed by upstream MatrixOlmAccount.
  let v = vectors();
  let mut account = OlmAccount::from_value(&v["account"]).expect("upstream account loads");
  assert_eq!(account.identity_key(), v["identity_key"]);
  assert_eq!(account.signing_key(), v["signing_key"]);
  // Ed25519 signatures are deterministic, so the payloads match exactly.
  assert_eq!(account.device_keys_payload("@bot:example.com", "DEVICE"), v["device_keys"]);
  assert_eq!(account.fallback_keys_payload("@bot:example.com", "DEVICE"), v["fallback_keys"]);
  assert_eq!(account.to_value(), v["account"]);
}

#[test]
fn test_account_round_trips() {
  let mut account = OlmAccount::new();
  assert_eq!(account.to_value()["fallback_otk"], Value::Null);
  let fallback = account.fallback_keys_payload("@u:hs", "D");
  let restored = OlmAccount::from_value(&account.to_value()).unwrap();
  assert_eq!(restored.identity_key(), account.identity_key());
  assert_eq!(restored.signing_key(), account.signing_key());
  // The fallback key is kept, so a re-upload publishes the same one.
  let mut restored = restored;
  assert_eq!(restored.fallback_keys_payload("@u:hs", "D"), fallback);
  let (key_id, object) = fallback.as_object().unwrap().iter().next().unwrap();
  assert!(key_id.starts_with("signed_curve25519:") && key_id.len() == "signed_curve25519:".len() + 10);
  assert!(verify_signed_otk(object, "@u:hs", "D", &account.signing_key()));
}

#[test]
fn test_account_rejects_damaged_records() {
  for record in [json!({}), json!({"ik": "abc", "sk": "def"}), json!({"ik": b64enc(&[1; 31]), "sk": b64enc(&[2; 32])}), json!("x")] {
    assert!(OlmAccount::from_value(&record).is_none(), "{record}");
  }
  // A missing fallback key is fine; one is made on the next upload.
  let account = json!({"ik": b64enc(&[1; 32]), "sk": b64enc(&[2; 32])});
  assert!(OlmAccount::from_value(&account).is_some());
}

#[test]
fn test_megolm_matches_upstream() {
  let v = vectors();
  let mut session = Megolm::from_value(&v["megolm_before"]).expect("upstream session loads");
  assert_eq!(session.session_id(), v["megolm_before"]["session_id"]);
  assert_eq!(session.counter(), 5);
  assert_eq!(session.to_value(), v["megolm_before"]);
  assert_eq!(session.session_key(), v["megolm_session_key"]);
  // Same ratchet, plaintext and signing key: the same ciphertext.
  let ciphertext = session.session.encrypt(v["megolm_plaintext"].as_str().unwrap()).to_base64();
  assert_eq!(ciphertext, v["megolm_ciphertext"]);
  assert_eq!(session.to_value(), v["megolm_after"]);
}

#[test]
fn test_megolm_ratchet_boundary_matches_upstream() {
  let v = vectors();
  let mut before = v["megolm_before"].clone();
  before["counter"] = json!(255);
  let mut session = Megolm::from_value(&before).unwrap();
  session.encrypt(&json!({"x": 1}));
  assert_eq!(session.to_value(), v["megolm_boundary_after"]);
}

#[test]
fn test_megolm_rejects_other_versions_and_rotates() {
  let v = vectors();
  for version in [json!(0), json!(2), Value::Null] {
    let mut record = v["megolm_before"].clone();
    record["version"] = version;
    assert!(Megolm::from_value(&record).is_none());
  }
  // created_at is old, so the upstream session is due for rotation.
  assert!(Megolm::from_value(&v["megolm_before"]).unwrap().should_rotate());
  let mut fresh = Megolm::new();
  assert!(!fresh.should_rotate());
  for _ in 0..MEGOLM_ROTATION_MSGS {
    fresh.encrypt(&json!({}));
  }
  assert!(fresh.should_rotate());
}

#[test]
fn test_megolm_decrypts_with_an_independent_session() {
  let mut session = Megolm::new();
  let key = SessionKey::from_base64(&session.session_key()).unwrap();
  let mut inbound = InboundGroupSession::new(&key, MegolmConfig::version_1());
  let event = json!({"type": "m.room.message", "content": {"body": "héllo"}, "room_id": "!r:hs"});
  let ciphertext = session.encrypt(&event);
  let decrypted = inbound.decrypt(&MegolmMessage::from_base64(&ciphertext).unwrap()).unwrap();
  assert_eq!(serde_json::from_slice::<Value>(&decrypted.plaintext).unwrap(), event);
  assert_eq!(decrypted.message_index, 0);
}

#[test]
fn test_olm_room_key_decrypts_on_the_receiving_device() {
  let sender = OlmAccount::new();
  let mut receiver = Account::new();
  receiver.generate_one_time_keys(1);
  let otk = receiver.one_time_keys().into_values().next().unwrap();
  let ciphertext = sender.encrypt_for_device(&receiver.curve25519_key().to_base64(), &otk.to_base64(), r#"{"type":"m.room_key"}"#).unwrap();
  assert_eq!(ciphertext["type"], 0);
  let message: OlmMessage = serde_json::from_value(ciphertext).unwrap();
  let OlmMessage::PreKey(pre_key) = message else { panic!("pre-key message expected") };
  let pre_key: PreKeyMessage = pre_key;
  let sender_key = Curve25519PublicKey::from_base64(&sender.identity_key()).unwrap();
  let result = receiver.create_inbound_session(OlmConfig::version_1(), sender_key, &pre_key).unwrap();
  assert_eq!(result.plaintext, br#"{"type":"m.room_key"}"#);
  // Bad keys are reported, not panicked on.
  assert!(sender.encrypt_for_device("nope", &otk.to_base64(), "x").is_err());
}

// ── Key verification ─────────────────────────────────────────────────────────

fn signed_device(account: &OlmAccount, user: &str, device: &str) -> Value {
  account.device_keys_payload(user, device)
}

#[test]
fn test_verify_device_keys() {
  let account = OlmAccount::new();
  let device = signed_device(&account, "@u:hs", "DEV");
  assert!(verify_device_keys(&device, "@u:hs", "DEV"));
  // unsigned is not covered by the signature.
  let mut with_unsigned = device.clone();
  with_unsigned["unsigned"] = json!({"device_display_name": "x"});
  assert!(verify_device_keys(&with_unsigned, "@u:hs", "DEV"));
  // The object must name the user and device it is checked for.
  assert!(!verify_device_keys(&device, "@other:hs", "DEV"));
  assert!(!verify_device_keys(&device, "@u:hs", "OTHER"));
  // A changed key or forged signature fails.
  let mut tampered = device.clone();
  tampered["keys"]["curve25519:DEV"] = json!(OlmAccount::new().identity_key());
  assert!(!verify_device_keys(&tampered, "@u:hs", "DEV"));
  let mut forged = device.clone();
  forged["signatures"]["@u:hs"]["ed25519:DEV"] = json!(b64enc(&[0; 64]));
  assert!(!verify_device_keys(&forged, "@u:hs", "DEV"));
  for broken in [json!([]), json!({"user_id": "@u:hs", "device_id": "DEV"}), json!({"user_id": "@u:hs", "device_id": "DEV", "keys": "x"})] {
    assert!(!verify_device_keys(&broken, "@u:hs", "DEV"));
  }
}

#[test]
fn test_verify_signed_otk() {
  let mut account = OlmAccount::new();
  let keys = account.fallback_keys_payload("@u:hs", "DEV");
  let otk = keys.as_object().unwrap().values().next().unwrap().clone();
  assert!(verify_signed_otk(&otk, "@u:hs", "DEV", &account.signing_key()));
  assert!(!verify_signed_otk(&otk, "@u:hs", "DEV", &OlmAccount::new().signing_key()));
  assert!(!verify_signed_otk(&otk, "@u:hs", "OTHER", &account.signing_key()));
  let mut tampered = otk.clone();
  tampered["key"] = json!(OlmAccount::new().identity_key());
  assert!(!verify_signed_otk(&tampered, "@u:hs", "DEV", &account.signing_key()));
}

#[test]
fn test_canonical_json() {
  let value = json!({"b": [1, {"z": true, "a": null}], "a": "é\n\"", "c": {}});
  assert_eq!(canonical_json(&value), r#"{"a":"é\n\"","b":[1,{"a":null,"z":true}],"c":{}}"#);
}

#[test]
fn test_attachment_encryption() {
  use aes::cipher::{KeyIvInit, StreamCipher};
  let data = b"attachment payload".repeat(10);
  let (ciphertext, info) = encrypt_attachment(&data);
  assert_ne!(ciphertext, data);
  assert_eq!(info["v"], "v2");
  assert_eq!(info["key"]["alg"], "A256CTR");
  assert_eq!(info["key"]["key_ops"], json!(["encrypt", "decrypt"]));
  // The IV is standard base64 (upstream 8227752a) with a zero counter.
  let iv_text = info["iv"].as_str().unwrap();
  assert!(!iv_text.contains(['-', '_', '=']));
  let iv: [u8; 16] = STANDARD_NO_PAD.decode(iv_text).unwrap().try_into().unwrap();
  assert_eq!(&iv[8..], &[0; 8]);
  let key: [u8; 32] = URL_SAFE_NO_PAD.decode(info["key"]["k"].as_str().unwrap()).unwrap().try_into().unwrap();
  assert_eq!(STANDARD_NO_PAD.decode(info["hashes"]["sha256"].as_str().unwrap()).unwrap(), Sha256::digest(&ciphertext).to_vec());
  let mut plain = ciphertext.clone();
  ctr::Ctr128BE::<aes::Aes256>::new(&key.into(), &iv.into()).apply_keystream(&mut plain);
  assert_eq!(plain, data);
  // Every attachment gets its own key and nonce.
  let (_, other) = encrypt_attachment(&data);
  assert_ne!(other["key"]["k"], info["key"]["k"]);
  assert_ne!(other["iv"], info["iv"]);
}

// ── SAS, against an initiator built from independent primitives ──────────────

/// HKDF-SHA-256 with an all-zero salt (RFC 5869).
pub(in crate::notify::matrix) fn hkdf(ikm: &[u8], info: &str, len: usize) -> Vec<u8> {
  type H = hmac::Hmac<Sha256>;
  let mut extract = <H as KeyInit>::new_from_slice(&[0u8; 32]).unwrap();
  extract.update(ikm);
  let prk = extract.finalize().into_bytes();
  let (mut out, mut block) = (Vec::new(), Vec::new());
  let mut counter = 1u8;
  while out.len() < len {
    let mut expand = <H as KeyInit>::new_from_slice(&prk).unwrap();
    expand.update(&block);
    expand.update(info.as_bytes());
    expand.update(&[counter]);
    block = expand.finalize().into_bytes().to_vec();
    out.extend_from_slice(&block);
    counter += 1;
  }
  out.truncate(len);
  out
}

/// The `hkdf-hmac-sha256.v2` SAS MAC.
pub(in crate::notify::matrix) fn mac(secret: &[u8], info: &str, value: &str) -> String {
  let key = hkdf(secret, info, 32);
  let mut h = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&key).unwrap();
  h.update(value.as_bytes());
  b64enc(&h.finalize().into_bytes())
}

/// The other device: an X25519 key and its own SAS arithmetic.
struct Initiator {
  secret: x25519_dalek::StaticSecret,
  public: String,
  user: &'static str,
  device: &'static str,
  signing_key: String,
}

impl Initiator {
  fn new() -> Self {
    let secret = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let public = b64enc(x25519_dalek::PublicKey::from(&secret).as_bytes());
    Self { secret, public, user: "@me:hs", device: "PHONE", signing_key: OlmAccount::new().signing_key() }
  }

  fn start(&self) -> Value {
    json!({
      "transaction_id": "txn1",
      "from_device": self.device,
      "method": "m.sas.v1",
      "key_agreement_protocols": ["curve25519-hkdf-sha256"],
      "hashes": ["sha256"],
      "message_authentication_codes": ["hkdf-hmac-sha256.v2", "hkdf-hmac-sha256"],
      "short_authentication_string": ["decimal", "emoji"],
    })
  }

  fn shared(&self, their_key: &str) -> Vec<u8> {
    let theirs: [u8; 32] = STANDARD_NO_PAD.decode(their_key).unwrap().try_into().unwrap();
    self.secret.diffie_hellman(&x25519_dalek::PublicKey::from(theirs)).as_bytes().to_vec()
  }

  fn mac_info(&self, sender: (&str, &str), receiver: (&str, &str), key_id: &str) -> String {
    format!("MATRIX_KEY_VERIFICATION_MAC{}{}{}{}txn1{key_id}", sender.0, sender.1, receiver.0, receiver.1)
  }
}

fn responder(peer: &Initiator) -> SasResponder {
  SasResponder::new("@me:hs", "APPRISE", peer.user, peer.device, &peer.start()).unwrap()
}

#[test]
fn test_sas_round_trip() {
  let peer = Initiator::new();
  let mut sas = responder(&peer);
  let accept = sas.accept_content().unwrap();
  assert_eq!(accept["short_authentication_string"], json!(["decimal"]));
  assert_eq!(accept["message_authentication_code"], "hkdf-hmac-sha256.v2");
  sas.receive_key(&json!(peer.public)).unwrap();
  let key = sas.key_content().unwrap();
  let our_key = key["key"].as_str().unwrap();

  // The commitment binds our key to the canonical start event.
  let mut digest = Sha256::new();
  digest.update(our_key.as_bytes());
  digest.update(canonical_json(&peer.start()).as_bytes());
  assert_eq!(accept["commitment"], b64enc(&digest.finalize()));

  // Both sides show the same decimals.
  let secret = peer.shared(our_key);
  let info = format!("MATRIX_KEY_VERIFICATION_SAS|@me:hs|PHONE|{}|@me:hs|APPRISE|{our_key}|txn1", peer.public);
  let bytes = hkdf(&secret, &info, 5);
  let value = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]) >> 1;
  let expected = (((value >> 26) & 0x1fff) as u16 + 1000, ((value >> 13) & 0x1fff) as u16 + 1000, (value & 0x1fff) as u16 + 1000);
  assert_eq!(sas.decimal_sas().unwrap(), expected);

  // Our MAC proves our signing key to the peer.
  let account = OlmAccount::new();
  let content = sas.mac_content(&account.signing_key()).unwrap();
  let key_id = "ed25519:APPRISE";
  assert_eq!(content["mac"][key_id], mac(&secret, &peer.mac_info(("@me:hs", "APPRISE"), ("@me:hs", "PHONE"), key_id), &account.signing_key()));
  assert_eq!(content["keys"], mac(&secret, &peer.mac_info(("@me:hs", "APPRISE"), ("@me:hs", "PHONE"), "KEY_IDS"), key_id));

  // And the peer's MAC over its device and master keys checks out.
  let peer_keys: Map<String, Value> = json!({"ed25519:PHONE": peer.signing_key, "ed25519:MASTER": "bWFzdGVy"}).as_object().unwrap().clone();
  let from_peer = |key_id: &str, value: &str| mac(&secret, &peer.mac_info(("@me:hs", "PHONE"), ("@me:hs", "APPRISE"), key_id), value);
  let peer_mac = json!({
    "transaction_id": "txn1",
    "mac": {"ed25519:PHONE": from_peer("ed25519:PHONE", &peer.signing_key), "ed25519:MASTER": from_peer("ed25519:MASTER", "bWFzdGVy")},
    "keys": from_peer("KEY_IDS", "ed25519:MASTER,ed25519:PHONE"),
  });
  sas.verify_peer_mac(&peer_mac, &peer_keys).unwrap();
  assert_eq!(sas.state, SasState::Verified);
}

#[test]
fn test_sas_rejects_bad_peer_macs() {
  let peer = Initiator::new();
  let keys: Map<String, Value> = json!({"ed25519:PHONE": peer.signing_key}).as_object().unwrap().clone();
  let prepared = || {
    let mut sas = responder(&peer);
    sas.accept_content().unwrap();
    sas.receive_key(&json!(peer.public)).unwrap();
    let our_key = sas.key_content().unwrap()["key"].as_str().unwrap().to_string();
    sas.mac_content("signing").unwrap();
    let secret = peer.shared(&our_key);
    (sas, secret)
  };
  let good = |secret: &[u8]| {
    let m = |key_id: &str, value: &str| mac(secret, &peer.mac_info(("@me:hs", "PHONE"), ("@me:hs", "APPRISE"), key_id), value);
    json!({"transaction_id": "txn1", "mac": {"ed25519:PHONE": m("ed25519:PHONE", &peer.signing_key)}, "keys": m("KEY_IDS", "ed25519:PHONE")})
  };

  type Mutation = Box<dyn Fn(&mut Value)>;
  let cases: Vec<(&str, Mutation)> = vec![
    ("wrong transaction", Box::new(|c| c["transaction_id"] = json!("other"))),
    ("no keys", Box::new(|c| c["mac"] = json!({}))),
    ("tampered key MAC", Box::new(|c| c["mac"]["ed25519:PHONE"] = json!(b64enc(&[1; 32])))),
    ("tampered key list", Box::new(|c| c["keys"] = json!(b64enc(&[1; 32])))),
    ("missing key list", Box::new(|c| c["keys"] = Value::Null)),
    ("omits device key", Box::new(|c| c["mac"] = json!({"ed25519:OTHER": "x"}))),
    ("unknown key", Box::new(|c| c["mac"]["ed25519:UNKNOWN"] = json!("eA"))),
    ("oversized entry", Box::new(|c| c["mac"]["ed25519:PHONE"] = json!("x".repeat(513)))),
    ("too many keys", Box::new(|c| (0..11).for_each(|i| c["mac"][format!("ed25519:K{i}")] = json!("x")))),
  ];
  for (name, mutate) in cases {
    let (mut sas, secret) = prepared();
    let mut content = good(&secret);
    mutate(&mut content);
    assert!(sas.verify_peer_mac(&content, &keys).is_err(), "{name}");
    assert_eq!(sas.state, SasState::MacSent, "{name}");
  }
  let (mut sas, secret) = prepared();
  sas.verify_peer_mac(&good(&secret), &keys).unwrap();
}

#[test]
fn test_sas_start_validation() {
  let peer = Initiator::new();
  let mutations: Vec<(&str, Value)> = vec![
    ("transaction_id", json!("")),
    ("from_device", json!("OTHER")),
    ("method", json!("m.reciprocate.v1")),
    ("key_agreement_protocols", json!(["curve25519"])),
    ("hashes", json!(["sha512"])),
    ("message_authentication_codes", json!(["hkdf-hmac-sha256"])),
    ("short_authentication_string", json!(["emoji"])),
    ("key_agreement_protocols", json!("curve25519-hkdf-sha256")),
  ];
  for (field, value) in mutations {
    let mut start = peer.start();
    start[field] = value;
    assert!(SasResponder::new("@me:hs", "APPRISE", peer.user, peer.device, &start).is_err(), "{field}");
  }
  assert!(SasResponder::new("@me:hs", "APPRISE", peer.user, "", &peer.start()).is_err());
}

#[test]
fn test_sas_state_guards() {
  let peer = Initiator::new();
  let mut sas = responder(&peer);
  // Nothing before acceptance.
  assert!(sas.receive_key(&json!(peer.public)).is_err());
  assert!(sas.key_content().is_err());
  assert!(sas.decimal_sas().is_err());
  assert!(sas.mac_content("k").is_err());
  sas.accept_content().unwrap();
  assert!(sas.accept_content().is_err());
  for bad in [json!(""), json!(5), json!("x".repeat(513)), json!("!!!!"), json!(b64enc(&[0; 32]))] {
    let mut fresh = responder(&peer);
    fresh.accept_content().unwrap();
    assert!(fresh.receive_key(&bad).is_err(), "{bad}");
  }
  sas.receive_key(&json!(peer.public)).unwrap();
  assert!(sas.receive_key(&json!(peer.public)).is_err());
  assert!(sas.verify_peer_mac(&json!({}), &Map::new()).is_err());
}

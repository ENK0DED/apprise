//! Matrix end-to-end encryption (upstream `apprise/plugins/matrix/e2ee.py`).
//!
//! Upstream implements the outbound halves of Olm and Megolm itself; this
//! port uses [`vodozemac`], the Matrix team's implementation, for Olm,
//! Megolm and SAS. Only the sending side is needed: Apprise shares Megolm
//! room keys over Olm, encrypts room events with Megolm and answers SAS
//! verification requests.
//!
//! Account and session state is stored in upstream's format (raw base64
//! private keys and ratchet parts), so a store directory shared with
//! upstream Apprise keeps one device identity.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use vodozemac::megolm::{GroupSession, GroupSessionPickle, SessionConfig as MegolmConfig};
use vodozemac::olm::{Account, AccountPickle, SessionConfig as OlmConfig};
use vodozemac::sas::{EstablishedSas, Mac, Sas};
use vodozemac::{Curve25519PublicKey, Curve25519SecretKey, Ed25519PublicKey, Ed25519Signature};

/// Rotate the Megolm session after this many messages.
pub(super) const MEGOLM_ROTATION_MSGS: u32 = 100;

/// Rotate the Megolm session after this many seconds (7 days).
pub(super) const MEGOLM_ROTATION_AGE: f64 = 60.0 * 60.0 * 24.0 * 7.0;

/// Upstream `MATRIX_MEGOLM_STORE_VERSION`: a cached session of another
/// version is replaced by a fresh one.
const MATRIX_MEGOLM_STORE_VERSION: i64 = 1;

pub(super) const OLM_ALGORITHM: &str = "m.olm.v1.curve25519-aes-sha2";
pub(super) const MEGOLM_ALGORITHM: &str = "m.megolm.v1.aes-sha2";

/// Matrix-style unpadded base64.
pub(super) fn b64enc(data: &[u8]) -> String {
  STANDARD_NO_PAD.encode(data)
}

/// Decode base64, tolerating missing padding and URL-safe characters.
fn b64dec(s: &str) -> Option<Vec<u8>> {
  let s: String = s
    .chars()
    .map(|c| match c {
      '-' => '+',
      '_' => '/',
      c => c,
    })
    .collect();
  STANDARD_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

fn key32(value: Option<&Value>) -> Option<[u8; 32]> {
  b64dec(value?.as_str()?)?.try_into().ok()
}

fn now_secs() -> f64 {
  std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Matrix canonical JSON (sorted keys, no spaces, UTF-8 kept as is), the
/// form every signature covers.
pub(super) fn canonical_json(value: &Value) -> String {
  fn write(value: &Value, out: &mut String) {
    match value {
      Value::Object(map) => {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        out.push('{');
        for (i, key) in keys.into_iter().enumerate() {
          if i > 0 {
            out.push(',');
          }
          out.push_str(&serde_json::to_string(key).unwrap_or_default());
          out.push(':');
          write(&map[key], out);
        }
        out.push('}');
      }
      Value::Array(items) => {
        out.push('[');
        for (i, item) in items.iter().enumerate() {
          if i > 0 {
            out.push(',');
          }
          write(item, out);
        }
        out.push(']');
      }
      other => out.push_str(&serde_json::to_string(other).unwrap_or_default()),
    }
  }
  let mut out = String::new();
  write(value, &mut out);
  out
}

/// Whether `signature_b64` is a valid Ed25519 signature of `message` by
/// `public_key_b64`.
fn verify_ed25519(public_key_b64: &str, message: &[u8], signature_b64: &str) -> bool {
  let Ok(key) = Ed25519PublicKey::from_base64(public_key_b64) else { return false };
  let Ok(signature) = Ed25519Signature::from_base64(signature_b64) else { return false };
  key.verify(message, &signature).is_ok()
}

/// Verify the self-signature on a `/keys/query` device object, which must
/// also name the user and device it is claimed for (so a homeserver cannot
/// move keys between devices).
pub(super) fn verify_device_keys(dev_info: &Value, user_id: &str, device_id: &str) -> bool {
  let Some(obj) = dev_info.as_object() else { return false };
  if obj.get("user_id").and_then(Value::as_str) != Some(user_id) || obj.get("device_id").and_then(Value::as_str) != Some(device_id) {
    return false;
  }
  let sig_key = format!("ed25519:{device_id}");
  let Some(public_key) = obj.get("keys").and_then(|k| k.get(&sig_key)).and_then(Value::as_str).filter(|k| !k.is_empty()) else { return false };
  let Some(signature) = obj.get("signatures").and_then(|s| s.get(user_id)).and_then(|s| s.get(&sig_key)).and_then(Value::as_str).filter(|s| !s.is_empty())
  else {
    return false;
  };
  let signed: Map<String, Value> =
    obj.iter().filter(|(k, _)| k.as_str() != "signatures" && k.as_str() != "unsigned").map(|(k, v)| (k.clone(), v.clone())).collect();
  verify_ed25519(public_key, canonical_json(&Value::Object(signed)).as_bytes(), signature)
}

/// Verify the signature on a `signed_curve25519` one-time key.
pub(super) fn verify_signed_otk(otk: &Value, user_id: &str, device_id: &str, ed25519_pub: &str) -> bool {
  let Some(obj) = otk.as_object() else { return false };
  let Some(signature) =
    obj.get("signatures").and_then(|s| s.get(user_id)).and_then(|s| s.get(format!("ed25519:{device_id}"))).and_then(Value::as_str).filter(|s| !s.is_empty())
  else {
    return false;
  };
  let signed: Map<String, Value> = obj.iter().filter(|(k, _)| k.as_str() != "signatures").map(|(k, v)| (k.clone(), v.clone())).collect();
  verify_ed25519(ed25519_pub, canonical_json(&Value::Object(signed)).as_bytes(), signature)
}

/// Encrypt an attachment for an encrypted room (AES-256-CTR, Matrix
/// encrypted attachments v2). Returns the ciphertext and the
/// `EncryptedFile` object without its `url`.
pub(super) fn encrypt_attachment(data: &[u8]) -> (Vec<u8>, Value) {
  use aes::cipher::{KeyIvInit, StreamCipher};
  let key: [u8; 32] = rand::random();
  // Eight random bytes, then an eight-byte zero counter, so the counter
  // never wraps into the nonce.
  let mut iv: [u8; 16] = rand::random();
  iv[8..].fill(0);
  let mut ciphertext = data.to_vec();
  let mut cipher = ctr::Ctr128BE::<aes::Aes256>::new(&key.into(), &iv.into());
  cipher.apply_keystream(&mut ciphertext);
  let digest = Sha256::digest(&ciphertext);
  let info = json!({
    "v": "v2",
    "key": {
      "kty": "oct",
      "key_ops": ["encrypt", "decrypt"],
      "alg": "A256CTR",
      "k": URL_SAFE_NO_PAD.encode(key),
      "ext": true,
    },
    // Standard base64 (upstream 8227752a), unpadded as Matrix requires.
    "iv": b64enc(&iv),
    "hashes": {"sha256": b64enc(&digest)},
  });
  (ciphertext, info)
}

// ── Device account ────────────────────────────────────────────────────────────

/// This device's Curve25519 identity and Ed25519 signing keys (upstream
/// `MatrixOlmAccount`), plus the fallback key published for it.
pub(super) struct OlmAccount {
  account: Account,
  /// Upstream's fallback key record: its key ID and Curve25519 private key.
  fallback: Option<(String, [u8; 32])>,
}

impl OlmAccount {
  pub(super) fn new() -> Self {
    Self { account: Account::new(), fallback: None }
  }

  /// Restore from upstream's `{"ik", "sk", "fallback_otk"}` record.
  pub(super) fn from_value(value: &Value) -> Option<Self> {
    let ik = key32(value.get("ik"))?;
    let sk = key32(value.get("sk"))?;
    let pickle = json!({
      "signing_key": {"Normal": sk.to_vec()},
      "diffie_hellman_key": ik.to_vec(),
      "one_time_keys": {"next_key_id": 0, "public_keys": {}, "private_keys": {}},
      "fallback_keys": {"key_id": 0, "fallback_key": null, "previous_fallback_key": null},
    });
    let account = Account::from_pickle(serde_json::from_value::<AccountPickle>(pickle).ok()?);
    let fallback = value.get("fallback_otk").and_then(|f| Some((f.get("id")?.as_str()?.to_string(), key32(f.get("sk"))?)));
    Some(Self { account, fallback })
  }

  /// The record [`OlmAccount::from_value`] reads.
  pub(super) fn to_value(&self) -> Value {
    let pickle = serde_json::to_value(self.account.pickle()).unwrap_or_default();
    let bytes = |v: Option<&Value>| -> Vec<u8> {
      v.and_then(Value::as_array).map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default()
    };
    let ik = bytes(pickle.get("diffie_hellman_key"));
    let sk = bytes(pickle.get("signing_key").and_then(|k| k.get("Normal")));
    json!({
      "ik": b64enc(&ik),
      "sk": b64enc(&sk),
      "fallback_otk": self.fallback.as_ref().map(|(id, key)| json!({"id": id, "sk": b64enc(key)})),
    })
  }

  /// Base64 Curve25519 identity key.
  pub(super) fn identity_key(&self) -> String {
    self.account.curve25519_key().to_base64()
  }

  /// Base64 Ed25519 signing key.
  pub(super) fn signing_key(&self) -> String {
    self.account.ed25519_key().to_base64()
  }

  fn sign(&self, message: &str) -> String {
    self.account.sign(message.as_bytes()).to_base64()
  }

  /// The signed `device_keys` object for `/keys/upload`.
  pub(super) fn device_keys_payload(&self, user_id: &str, device_id: &str) -> Value {
    let mut keys = json!({
      "algorithms": [OLM_ALGORITHM, MEGOLM_ALGORITHM],
      "device_id": device_id,
      "keys": {
        format!("curve25519:{device_id}"): self.identity_key(),
        format!("ed25519:{device_id}"): self.signing_key(),
      },
      "user_id": user_id,
    });
    let signature = self.sign(&canonical_json(&keys));
    keys["signatures"] = json!({user_id: {format!("ed25519:{device_id}"): signature}});
    keys
  }

  /// The signed `fallback_keys` for `/keys/upload`, creating the fallback
  /// key on first use.
  pub(super) fn fallback_keys_payload(&mut self, user_id: &str, device_id: &str) -> Value {
    let (key_id, secret) = self
      .fallback
      .get_or_insert_with(|| {
        let id: [u8; 5] = rand::random();
        (hex::encode(id), *Curve25519SecretKey::new().to_bytes())
      })
      .clone();
    let public = Curve25519PublicKey::from(&Curve25519SecretKey::from_slice(&secret)).to_base64();
    let mut object = json!({"key": public});
    let signature = self.sign(&canonical_json(&object));
    object["signatures"] = json!({user_id: {format!("ed25519:{device_id}"): signature}});
    json!({format!("signed_curve25519:{key_id}"): object})
  }

  /// Olm-encrypt `plaintext` for one device, starting a new outbound
  /// session with its identity key and a claimed one-time key. Returns the
  /// `{"type": 0, "body": ...}` ciphertext object.
  pub(super) fn encrypt_for_device(&self, identity_key: &str, one_time_key: &str, plaintext: &str) -> Result<Value, String> {
    let identity_key = Curve25519PublicKey::from_base64(identity_key).map_err(|e| e.to_string())?;
    let one_time_key = Curve25519PublicKey::from_base64(one_time_key).map_err(|e| e.to_string())?;
    let mut session = self.account.create_outbound_session(OlmConfig::version_1(), identity_key, one_time_key).map_err(|e| e.to_string())?;
    let message = session.encrypt(plaintext).map_err(|e| e.to_string())?;
    serde_json::to_value(&message).map_err(|e| e.to_string())
  }
}

// ── Megolm outbound session ────────────────────────────────────────────────────

/// An outbound Megolm session for one room (upstream `MatrixMegOlmSession`).
pub(super) struct Megolm {
  session: GroupSession,
  created_at: f64,
}

impl Megolm {
  pub(super) fn new() -> Self {
    Self { session: GroupSession::new(MegolmConfig::version_1()), created_at: now_secs() }
  }

  /// Restore upstream's `{"version", "ratchet", "counter", "sk",
  /// "session_id", "created_at"}` record; `None` for another version or a
  /// damaged record.
  pub(super) fn from_value(value: &Value) -> Option<Self> {
    if value.get("version").and_then(Value::as_i64) != Some(MATRIX_MEGOLM_STORE_VERSION) {
      return None;
    }
    let parts = value.get("ratchet")?.as_array()?;
    if parts.len() != 4 {
      return None;
    }
    let mut inner = Vec::with_capacity(128);
    for part in parts {
      inner.extend_from_slice(&key32(Some(part))?);
    }
    let counter = u32::try_from(value.get("counter")?.as_u64()?).ok()?;
    let sk = key32(value.get("sk"))?;
    let pickle = json!({
      "ratchet": {"inner": inner, "counter": counter},
      "signing_key": {"Normal": sk.to_vec()},
      "config": {"version": "V1"},
    });
    let session = GroupSession::from_pickle(serde_json::from_value::<GroupSessionPickle>(pickle).ok()?);
    let created_at = value.get("created_at").and_then(Value::as_f64).unwrap_or_else(now_secs);
    Some(Self { session, created_at })
  }

  /// The record [`Megolm::from_value`] reads.
  pub(super) fn to_value(&self) -> Value {
    let pickle = serde_json::to_value(self.session.pickle()).unwrap_or_default();
    let bytes = |v: Option<&Value>| -> Vec<u8> {
      v.and_then(Value::as_array).map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect()).unwrap_or_default()
    };
    let inner = bytes(pickle.get("ratchet").and_then(|r| r.get("inner")));
    let sk = bytes(pickle.get("signing_key").and_then(|k| k.get("Normal")));
    json!({
      "version": MATRIX_MEGOLM_STORE_VERSION,
      "ratchet": inner.chunks(32).map(b64enc).collect::<Vec<_>>(),
      "counter": self.session.message_index(),
      "sk": b64enc(&sk),
      "session_id": self.session_id(),
      "created_at": self.created_at,
    })
  }

  pub(super) fn session_id(&self) -> String {
    self.session.session_id()
  }

  /// The index of the next message (upstream `_counter`).
  pub(super) fn counter(&self) -> u32 {
    self.session.message_index()
  }

  /// Whether the session reached a rotation threshold.
  pub(super) fn should_rotate(&self) -> bool {
    self.counter() >= MEGOLM_ROTATION_MSGS || now_secs() - self.created_at >= MEGOLM_ROTATION_AGE
  }

  /// Megolm-encrypt an event, returning the base64 ciphertext.
  pub(super) fn encrypt(&mut self, event: &Value) -> String {
    self.session.encrypt(event.to_string()).to_base64()
  }

  /// The signed session key shared in `m.room_key` events.
  pub(super) fn session_key(&self) -> String {
    self.session.session_key().to_base64()
  }
}

// ── SAS verification ──────────────────────────────────────────────────────────

/// Where a [`SasResponder`] is in the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SasState {
  Started,
  Accepted,
  KeyReceived,
  MacSent,
  Verified,
}

/// Answers a SAS verification started by another device of the same user
/// (upstream `MatrixSASVerification`), reusing this device's identity.
pub(super) struct SasResponder {
  own_user_id: String,
  own_device_id: String,
  peer_user_id: String,
  peer_device_id: String,
  start_content: Value,
  pub(super) transaction_id: String,
  sas_methods: Vec<&'static str>,
  sas: Option<Sas>,
  established: Option<EstablishedSas>,
  public_key: String,
  peer_public_key: Option<String>,
  pub(super) state: SasState,
}

pub(super) const SAS_METHOD: &str = "m.sas.v1";
const SAS_KEY_AGREEMENT: &str = "curve25519-hkdf-sha256";
const SAS_HASH: &str = "sha256";
const SAS_MAC: &str = "hkdf-hmac-sha256.v2";
/// Only decimal is offered, the format Apprise logs.
const SAS_METHODS: [&str; 1] = ["decimal"];
/// A generous margin over the device and master keys normally proven.
const SAS_MAX_MAC_KEYS: usize = 10;
/// Bound on incoming key and MAC fields.
const SAS_MAX_FIELD_LEN: usize = 512;

fn bounded_str(value: Option<&Value>) -> Option<&str> {
  value.and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().count() <= SAS_MAX_FIELD_LEN)
}

fn offers(content: &Value, field: &str, wanted: &str) -> bool {
  content.get(field).and_then(Value::as_array).is_some_and(|list| list.iter().any(|v| v.as_str() == Some(wanted)))
}

impl SasResponder {
  /// Validate a start event and prepare a responder for it.
  pub(super) fn new(own_user_id: &str, own_device_id: &str, peer_user_id: &str, peer_device_id: &str, start_content: &Value) -> Result<Self, &'static str> {
    let transaction_id = start_content.get("transaction_id").and_then(Value::as_str).unwrap_or_default().to_string();
    if transaction_id.is_empty() || peer_device_id.is_empty() {
      return Err("Missing SAS transaction or peer device ID");
    }
    if start_content.get("from_device").and_then(Value::as_str) != Some(peer_device_id) {
      return Err("SAS start device does not match request device");
    }
    if start_content.get("method").and_then(Value::as_str) != Some(SAS_METHOD) {
      return Err("Unsupported SAS verification method");
    }
    if !offers(start_content, "key_agreement_protocols", SAS_KEY_AGREEMENT) {
      return Err("Unsupported SAS key agreement");
    }
    if !offers(start_content, "hashes", SAS_HASH) {
      return Err("Unsupported SAS hash");
    }
    if !offers(start_content, "message_authentication_codes", SAS_MAC) {
      return Err("Unsupported SAS MAC");
    }
    let sas_methods: Vec<&'static str> = SAS_METHODS.into_iter().filter(|m| offers(start_content, "short_authentication_string", m)).collect();
    if !sas_methods.contains(&"decimal") {
      return Err("SAS start does not offer decimal verification");
    }
    let sas = Sas::new();
    let public_key = sas.public_key().to_base64();
    Ok(Self {
      own_user_id: own_user_id.to_string(),
      own_device_id: own_device_id.to_string(),
      peer_user_id: peer_user_id.to_string(),
      peer_device_id: peer_device_id.to_string(),
      start_content: start_content.clone(),
      transaction_id,
      sas_methods,
      sas: Some(sas),
      established: None,
      public_key,
      peer_public_key: None,
      state: SasState::Started,
    })
  }

  /// The `m.key.verification.accept` content, committing to our key and
  /// the exact proposal.
  pub(super) fn accept_content(&mut self) -> Result<Value, &'static str> {
    if self.state != SasState::Started {
      return Err("SAS verification start was already accepted");
    }
    let mut digest = Sha256::new();
    digest.update(self.public_key.as_bytes());
    digest.update(canonical_json(&self.start_content).as_bytes());
    self.state = SasState::Accepted;
    Ok(json!({
      "transaction_id": self.transaction_id,
      "key_agreement_protocol": SAS_KEY_AGREEMENT,
      "hash": SAS_HASH,
      "message_authentication_code": SAS_MAC,
      "short_authentication_string": self.sas_methods,
      "commitment": b64enc(&digest.finalize()),
    }))
  }

  /// Take the initiator's ephemeral key and establish the shared secret.
  pub(super) fn receive_key(&mut self, peer_public_key: &Value) -> Result<(), &'static str> {
    if self.state != SasState::Accepted {
      return Err("Unexpected SAS key event");
    }
    let key = bounded_str(Some(peer_public_key)).ok_or("SAS peer key is missing or malformed")?;
    let sas = self.sas.take().ok_or("SAS key was already used")?;
    self.established = Some(sas.diffie_hellman_with_raw(key).map_err(|_| "SAS peer key is invalid")?);
    self.peer_public_key = Some(key.to_string());
    self.state = SasState::KeyReceived;
    Ok(())
  }

  /// Our `m.key.verification.key` content.
  pub(super) fn key_content(&self) -> Result<Value, &'static str> {
    if self.state != SasState::KeyReceived {
      return Err("SAS peer key has not been received");
    }
    Ok(json!({"transaction_id": self.transaction_id, "key": self.public_key}))
  }

  /// The three decimal numbers both devices display.
  pub(super) fn decimal_sas(&self) -> Result<(u16, u16, u16), &'static str> {
    let (Some(established), Some(peer_key)) = (&self.established, &self.peer_public_key) else {
      return Err("SAS shared secret has not been established");
    };
    // The peer initiated, so its identity and key come first.
    let info = format!(
      "MATRIX_KEY_VERIFICATION_SAS|{}|{}|{}|{}|{}|{}|{}",
      self.peer_user_id, self.peer_device_id, peer_key, self.own_user_id, self.own_device_id, self.public_key, self.transaction_id
    );
    Ok(established.bytes(&info).decimals())
  }

  fn mac_info(&self, key_id: &str, sender_user: &str, sender_device: &str, receiver_user: &str, receiver_device: &str) -> String {
    format!("MATRIX_KEY_VERIFICATION_MAC{sender_user}{sender_device}{receiver_user}{receiver_device}{}{key_id}", self.transaction_id)
  }

  fn own_mac(&self, value: &str, key_id: &str) -> Result<String, &'static str> {
    let established = self.established.as_ref().ok_or("SAS shared secret has not been established")?;
    let info = self.mac_info(key_id, &self.own_user_id, &self.own_device_id, &self.peer_user_id, &self.peer_device_id);
    Ok(established.calculate_mac(value, &info).to_base64())
  }

  fn peer_mac_matches(&self, value: &str, key_id: &str, received: &str) -> bool {
    let Some(established) = self.established.as_ref() else { return false };
    let Ok(mac) = Mac::from_base64(received) else { return false };
    let info = self.mac_info(key_id, &self.peer_user_id, &self.peer_device_id, &self.own_user_id, &self.own_device_id);
    established.verify_mac(value, &info, &mac).is_ok()
  }

  /// The MAC proving this device owns its signing key.
  pub(super) fn mac_content(&mut self, own_signing_key: &str) -> Result<Value, &'static str> {
    if self.state != SasState::KeyReceived {
      return Err("SAS peer key has not been received");
    }
    let key_id = format!("ed25519:{}", self.own_device_id);
    let content = json!({
      "transaction_id": self.transaction_id,
      "mac": {key_id.clone(): self.own_mac(own_signing_key, &key_id)?},
      "keys": self.own_mac(&key_id, "KEY_IDS")?,
    });
    self.state = SasState::MacSent;
    Ok(content)
  }

  /// Check the initiator's MACs against the keys queried for its device.
  pub(super) fn verify_peer_mac(&mut self, content: &Value, peer_keys: &Map<String, Value>) -> Result<(), &'static str> {
    if self.state != SasState::MacSent {
      return Err("Unexpected SAS MAC event");
    }
    if content.get("transaction_id").and_then(Value::as_str) != Some(self.transaction_id.as_str()) {
      return Err("SAS MAC transaction does not match");
    }
    let macs = content.get("mac").and_then(Value::as_object).filter(|m| !m.is_empty()).ok_or("SAS MAC event contains no keys")?;
    if macs.len() > SAS_MAX_MAC_KEYS {
      return Err("SAS MAC event carries too many keys");
    }
    for (key_id, value) in macs {
      if key_id.is_empty() || key_id.chars().count() > SAS_MAX_FIELD_LEN || bounded_str(Some(value)).is_none() {
        return Err("SAS MAC event carries a malformed entry");
      }
    }
    if !macs.contains_key(&format!("ed25519:{}", self.peer_device_id)) {
      return Err("SAS MAC omits the peer device key");
    }
    let keys_mac = bounded_str(content.get("keys")).ok_or("SAS MAC event carries a malformed key list MAC")?;
    let mut key_ids: Vec<&str> = macs.keys().map(String::as_str).collect();
    key_ids.sort();
    if !self.peer_mac_matches(&key_ids.join(","), "KEY_IDS", keys_mac) {
      return Err("SAS key list MAC does not match");
    }
    for (key_id, received) in macs {
      let public_key = bounded_str(Some(peer_keys.get(key_id).ok_or("SAS MAC references an unknown key")?)).ok_or("SAS MAC references a malformed key")?;
      if !self.peer_mac_matches(public_key, key_id, received.as_str().unwrap_or_default()) {
        return Err("SAS device key MAC does not match");
      }
    }
    self.state = SasState::Verified;
    Ok(())
  }
}

#[cfg(test)]
pub(super) mod tests;

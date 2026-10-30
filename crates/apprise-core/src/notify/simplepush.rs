//! Simplepush (simplepu.sh); every message is sent as a task.
//!
//! A personal API token sends to your own devices or to topics. An
//! organization integration token (`spi_<credential>.<seed>`) sends to
//! topics, `@members` or a broadcast. Attachments are uploaded into the
//! rows the task creates.
//!
//! End-to-end encryption matches the Simplepush clients: a password (the
//! URL user) derives an XChaCha20-Poly1305 key with Argon2id, salted with
//! the topic or the account's password salt; organization sends use the
//! organization key wrapped to the integration token with `crypto_box`.
//! It needs the `simplepush-encryption` feature (on by default).

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_bool_or, parse_list, qsd_raw, split_path, unquote};

/// Organization integration tokens are `spi_<credential>.<seed>`.
const SIMPLEPUSH_INTEGRATION_PREFIX: &str = "spi_";

/// Named priorities, matched against the start of the value in this order.
const SIMPLEPUSH_PRIORITY_MAP: &[(&str, u8)] = &[("mi", 1), ("l", 2), ("d", 3), ("n", 3), ("h", 4), ("c", 5), ("e", 5), ("ma", 5)];

/// The server accepts at most 25 links per task.
const LINKS_MAX: usize = 25;

/// Server limits, which apply to the ciphertext of an encrypted field.
const ENCRYPTED_MAXLEN: &[(&str, usize)] = &[("title", 500), ("content", 10000)];

fn b64(data: &[u8]) -> String {
  base64::engine::general_purpose::STANDARD.encode(data)
}

/// Python's non-validating `b64decode`: characters outside the alphabet are
/// dropped, URL-safe characters are accepted and padding is optional.
fn b64_lenient(data: &str) -> Option<Vec<u8>> {
  let cleaned: String = data
    .chars()
    .map(|c| match c {
      '-' => '+',
      '_' => '/',
      c => c,
    })
    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
    .collect();
  let engine = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true).with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireNone),
  );
  engine.decode(cleaned).ok()
}

/// libsodium-compatible primitives behind the `simplepush-encryption`
/// feature. Without it every function reports that encryption is
/// unavailable and encrypted sends fail closed.
mod sodium {
  /// Whether end-to-end encryption is compiled in.
  pub const AVAILABLE: bool = cfg!(feature = "simplepush-encryption");

  /// `(key, fingerprint)` for a password and salt, as every Simplepush
  /// client derives them:
  ///
  /// `sha256(salt)[:16] --Argon2id(password)--> master (64 bytes)`,
  /// `key = HKDF-SHA256-expand(master, "symmetric-key")` (32 bytes) and
  /// `fingerprint = base64(sha256(key)[:8])`.
  #[cfg(feature = "simplepush-encryption")]
  pub fn derive_key(password: &str, salt: &str) -> Option<([u8; 32], String)> {
    use hmac::{KeyInit, Mac};
    use sha2::{Digest, Sha256};

    // crypto_pwhash(opslimit=3, memlimit=64 MiB, ARGON2ID13)
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(64)).ok()?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let salt = Sha256::digest(salt.as_bytes());
    let mut master = [0u8; 64];
    argon.hash_password_into(password.as_bytes(), &salt[..16], &mut master).ok()?;

    let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&master).ok()?;
    mac.update(b"symmetric-key\x01");
    let key: [u8; 32] = mac.finalize().into_bytes().into();
    let fingerprint = super::b64(&Sha256::digest(key)[..8]);
    Some((key, fingerprint))
  }

  #[cfg(not(feature = "simplepush-encryption"))]
  pub fn derive_key(_password: &str, _salt: &str) -> Option<([u8; 32], String)> {
    None
  }

  /// XChaCha20-Poly1305 (IETF): `24 byte nonce || ciphertext || 16 byte tag`.
  #[cfg(feature = "simplepush-encryption")]
  pub fn seal(data: &[u8], key: &[u8; 32]) -> Option<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use ring::rand::SecureRandom;

    let mut nonce = [0u8; 24];
    ring::rand::SystemRandom::new().fill(&mut nonce).ok()?;
    let cipher = chacha20poly1305::XChaCha20Poly1305::new(key.into());
    let sealed = cipher.encrypt((&nonce).into(), data).ok()?;
    Some([nonce.as_slice(), &sealed].concat())
  }

  #[cfg(not(feature = "simplepush-encryption"))]
  pub fn seal(_data: &[u8], _key: &[u8; 32]) -> Option<Vec<u8>> {
    None
  }

  /// `crypto_box_seed_keypair(seed)` followed by `crypto_box_open()`: open
  /// `blob` (`24 byte nonce || tag || ciphertext`) sent by `sender_pk`.
  #[cfg(feature = "simplepush-encryption")]
  pub fn box_open(blob: &[u8], sender_pk: &[u8], seed: &[u8]) -> Option<Vec<u8>> {
    use poly1305::universal_hash::KeyInit as _;
    use salsa20::cipher::{KeyIvInit, StreamCipher};
    use sha2::{Digest, Sha512};

    if blob.len() < 24 + 16 {
      return None;
    }
    let (nonce, boxed) = blob.split_at(24);
    let (tag, ciphertext) = boxed.split_at(16);

    let secret: [u8; 32] = Sha512::digest(seed)[..32].try_into().ok()?;
    let sender: [u8; 32] = sender_pk.try_into().ok()?;
    let shared = x25519_dalek::StaticSecret::from(secret).diffie_hellman(&x25519_dalek::PublicKey::from(sender));
    // libsodium refuses an all-zero shared secret
    if !shared.was_contributory() {
      return None;
    }

    // crypto_box_beforenm: HSalsa20 of the shared secret
    let key = salsa20::hsalsa::<salsa20::cipher::consts::U10>(shared.as_bytes().into(), &[0u8; 16].into());

    // crypto_secretbox_open: the first 32 keystream bytes key Poly1305
    let nonce: [u8; 24] = nonce.try_into().ok()?;
    let mut stream = salsa20::XSalsa20::new(&key, &nonce.into());
    let mut poly_key = [0u8; 32];
    stream.apply_keystream(&mut poly_key);
    let expected = poly1305::Poly1305::new(&poly_key.into()).compute_unpadded(ciphertext);
    // Compare the tag in constant time
    if expected.len() != tag.len() || expected.iter().zip(tag).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
      return None;
    }
    let mut plain = ciphertext.to_vec();
    stream.apply_keystream(&mut plain);
    Some(plain)
  }

  #[cfg(not(feature = "simplepush-encryption"))]
  pub fn box_open(_blob: &[u8], _sender_pk: &[u8], _seed: &[u8]) -> Option<Vec<u8>> {
    None
  }
}

/// How a task is encrypted: the key and the `encryption` marker sent along.
type Encryption = Option<([u8; 32], Value)>;

/// What a `SimplePush` object learns from the server; fetched once.
#[derive(Default)]
struct SpushState {
  /// Derived (key, fingerprint) per salt; Argon2id is deliberately slow.
  keys: HashMap<String, ([u8; 32], String)>,
  /// The account's password salt, for encrypted sends to your own devices.
  account_salt: Option<String>,
  /// Organization keys by version; empty when the organization does not
  /// encrypt.
  org_keys: Option<BTreeMap<i64, [u8; 32]>>,
}

pub struct SimplePush {
  integration: bool,
  credential: String,
  seed: Option<Vec<u8>>,
  password: Option<String>,
  topics: Vec<String>,
  members: Vec<String>,
  broadcast: bool,
  shared: bool,
  priority: Option<u8>,
  critical_volume: Option<f64>,
  sptag: Option<String>,
  links: Vec<String>,
  topic_auth_token: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  state: Mutex<SpushState>,
}

impl SimplePush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // API token, or organization integration token (validate_regex)
    let token = url.host.as_deref().map(unquote).and_then(|t| t.split_whitespace().next().map(str::to_string));
    let Some(token) = token else {
      tracing::warn!("An invalid Simplepush API Token was specified.");
      return None;
    };

    // An integration token authenticates with its credential half; its seed
    // half never leaves this process and opens the organization keys
    let integration = token.starts_with(SIMPLEPUSH_INTEGRATION_PREFIX);
    let (credential, seed) = if integration {
      let (credential, seed) = token.split_once('.').unwrap_or((token.as_str(), ""));
      let seed = b64_lenient(seed).filter(|s| s.len() == 32);
      if credential == SIMPLEPUSH_INTEGRATION_PREFIX || seed.is_none() {
        tracing::warn!("An invalid Simplepush integration token was specified; it looks like spi_<credential>.<seed>.");
        return None;
      }
      (credential.to_string(), seed)
    } else {
      (token.clone(), None)
    };

    // The password is the user part of the URL, or ?password=
    let password = qsd_raw(url, "password").filter(|p| !p.is_empty()).or_else(|| url.user.clone()).filter(|p| !p.is_empty());

    let flag = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(|v| parse_bool_or(Some(v), false)).unwrap_or(false);
    let broadcast = flag("broadcast");
    let shared = flag("shared");

    // Topics and organization members, in URL order
    let mut entries = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_list([to], true, true));
    }
    let (mut topics, mut members) = (Vec::new(), Vec::new());
    for target in parse_list(entries, false, false) {
      match target.strip_prefix('@') {
        None => topics.push(target),
        Some(member) => {
          let member = member.trim();
          if member.is_empty() {
            tracing::warn!("An invalid Simplepush member ({}) was specified.", target);
            return None;
          }
          members.push(member.to_string());
        }
      }
    }

    if !integration {
      if !members.is_empty() || broadcast {
        tracing::warn!("Simplepush members and broadcast require an organization integration token (spi_...).");
        return None;
      }
    } else {
      if broadcast && (!topics.is_empty() || !members.is_empty()) {
        tracing::warn!("A Simplepush broadcast can not be combined with topics or members.");
        return None;
      }
      if !(broadcast || !topics.is_empty() || !members.is_empty()) {
        tracing::warn!("A Simplepush organization send requires a topic, a member or broadcast.");
        return None;
      }
      if password.is_some() {
        tracing::warn!("Simplepush organization sends can not be encrypted with a password; they use the organization key.");
        return None;
      }
    }

    if password.is_some() && !sodium::AVAILABLE {
      tracing::warn!("Encrypted Simplepush sends require the simplepush-encryption feature.");
      return None;
    }

    let priority = match url.get("priority").map(str::trim).filter(|p| !p.is_empty()) {
      None => None,
      Some(value) => {
        let value = value.to_lowercase();
        let priority = match value.parse::<u8>() {
          Ok(p) if (1..=5).contains(&p) && value.len() == 1 => Some(p),
          _ if !value.starts_with(|c: char| c.is_ascii_digit()) => SIMPLEPUSH_PRIORITY_MAP.iter().find(|(k, _)| value.starts_with(k)).map(|(_, v)| *v),
          _ => None,
        };
        if priority.is_none() {
          tracing::warn!("An invalid Simplepush priority ({}) was specified.", value);
          return None;
        }
        priority
      }
    };

    // Volume of the iOS critical alert sound; Android ignores it
    let critical_volume = match url.get("critical_volume").map(str::trim).filter(|v| !v.is_empty()) {
      None => None,
      Some(value) => {
        match value.parse::<f64>() {
          Ok(v) if v > 0.0 && v <= 1.0 => {}
          _ => {
            tracing::warn!("An invalid Simplepush critical volume ({}) was specified; it must be greater than 0 and at most 1.", value);
            return None;
          }
        }
        if priority != Some(5) {
          tracing::warn!("The Simplepush critical volume requires priority 5.");
          return None;
        }
        value.parse::<f64>().ok()
      }
    };

    // Links are separated by whitespace; a URL never contains any
    let links: Vec<String> = url.get("links").map(|l| l.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
    if links.len() > LINKS_MAX {
      tracing::warn!("At most {} Simplepush links can be specified.", LINKS_MAX);
      return None;
    }

    Some(Self {
      integration,
      credential,
      seed,
      password,
      topics,
      members,
      broadcast,
      shared,
      priority,
      critical_volume,
      sptag: url.get("sptag").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
      links,
      topic_auth_token: qsd_raw(url, "topic_auth_token").filter(|t| !t.is_empty()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      state: Mutex::new(SpushState::default()),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Simplepush",
      service_url: Some("https://simplepu.sh/"),
      setup_url: Some("https://appriseit.com/services/simplepush/"),
      protocols: vec!["spush", "simplepush"],
      description: "Send push notifications via Simplepush.",
      attachment_support: true,
    }
  }

  /// The sender credential header.
  fn auth_header(&self) -> (&'static str, String) {
    if self.integration { ("Authorization", format!("Bearer {}", self.credential)) } else { ("API-Token", self.credential.clone()) }
  }

  /// The server's `msg` (or the HTTP reason) followed by a separator.
  fn error_message(status: reqwest::StatusCode, body: &[u8]) -> String {
    let msg = serde_json::from_slice::<Value>(body).ok().and_then(|v| v.get("msg").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_string));
    match msg.or_else(|| status.canonical_reason().map(str::to_string)) {
      Some(msg) => format!("{}, ", msg),
      None => String::new(),
    }
  }

  /// GET a JSON object with the sender credential.
  async fn get_json(&self, client: &reqwest::Client, path: &str, what: &str) -> Option<serde_json::Map<String, Value>> {
    let (name, value) = self.auth_header();
    let url = format!("{}{}", service_base("https://api.simplepu.sh"), path);
    let resp = match client.get(&url).header("User-Agent", APP_ID).header("Accept", "application/json").header(name, value).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred fetching {}.", what);
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if status.as_u16() != 200 {
      tracing::warn!("Failed to fetch {}: {}error={}.", what, Self::error_message(status, &body), status.as_u16());
      return None;
    }
    match serde_json::from_slice::<Value>(&body) {
      Ok(Value::Object(map)) => Some(map),
      _ => {
        tracing::warn!("Simplepush returned an unreadable {}.", what);
        None
      }
    }
  }

  /// The organization keys wrapped for the integration token: `{version:
  /// key}`, empty when the organization has no encryption, `None` on
  /// failure (tried again on the next notification).
  async fn org_key_set(&self, client: &reqwest::Client, state: &mut SpushState) -> Option<BTreeMap<i64, [u8; 32]>> {
    if let Some(keys) = &state.org_keys {
      return Some(keys.clone());
    }
    let response = self.get_json(client, "/v1/org/integration/keys", "the Simplepush integration keys").await?;
    let scopes = response.get("scopes").and_then(Value::as_array);
    if !scopes.is_some_and(|s| s.iter().any(|v| v == "send")) {
      tracing::warn!("The Simplepush integration token lacks the send scope.");
      return None;
    }

    let mut keys = BTreeMap::new();
    let enabled = response.get("enabled").is_some_and(|v| match v {
      Value::Bool(b) => *b,
      Value::Null => false,
      Value::Number(n) => n.as_f64() != Some(0.0),
      Value::String(s) => !s.is_empty(),
      Value::Array(a) => !a.is_empty(),
      Value::Object(o) => !o.is_empty(),
    });
    if enabled {
      if !sodium::AVAILABLE {
        tracing::warn!("The Simplepush organization has encryption turned on, which requires the simplepush-encryption feature.");
        return None;
      }
      let wraps = response.get("wrappedKeys").and_then(Value::as_array).filter(|w| !w.is_empty());
      let Some(wraps) = wraps else {
        // Sending in the clear would leak into an encrypted organization
        tracing::warn!("The Simplepush organization has encryption turned on but holds no keys for this integration token; create a new integration token.");
        return None;
      };

      // Every key is wrapped by the organization admin to this token:
      // base64(24 byte nonce + crypto_box ciphertext)
      let seed = self.seed.as_deref()?;
      let admin = response.get("adminPubkeyB64").and_then(Value::as_str).and_then(b64_lenient);
      for wrap in wraps {
        let version = wrap.get("version").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())));
        let key =
          admin.as_deref().zip(wrap.get("blob").and_then(Value::as_str).and_then(b64_lenient)).and_then(|(admin, blob)| sodium::box_open(&blob, admin, seed));
        match (version, key.and_then(|k| <[u8; 32]>::try_from(k).ok())) {
          (Some(version), Some(key)) => {
            keys.insert(version, key);
          }
          _ => {
            tracing::warn!("Could not open the Simplepush organization keys with this integration token.");
            return None;
          }
        }
      }
    }

    state.org_keys = Some(keys.clone());
    Some(keys)
  }

  /// The key and marker to encrypt a task with. `Some(None)` sends it in
  /// the clear; `None` means the key could not be obtained.
  async fn encryption(&self, client: &reqwest::Client, state: &mut SpushState, kind: Option<&str>, target: Option<&str>) -> Option<Encryption> {
    if self.integration {
      let keys = self.org_key_set(client, state).await?;
      // New content is encrypted with the newest organization key
      return Some(keys.iter().next_back().map(|(version, key)| (*key, json!({"type": "org", "v": version}))));
    }

    let Some(password) = &self.password else { return Some(None) };

    // A topic send is salted with the topic; a send to your own devices
    // with the account password salt
    let salt = match (kind, target) {
      (Some("topic"), Some(topic)) => topic.to_string(),
      _ => match &state.account_salt {
        Some(salt) => salt.clone(),
        None => {
          let response = self.get_json(client, "/v1/user", "the Simplepush password salt").await?;
          let Some(salt) = response.get("passwordSalt").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
            tracing::warn!("Simplepush returned no password salt.");
            return None;
          };
          state.account_salt = Some(salt.clone());
          salt
        }
      },
    };

    let (key, fingerprint) = match state.keys.get(&salt) {
      Some(entry) => entry.clone(),
      None => {
        let (password, salt_owned) = (password.clone(), salt.clone());
        let derived = tokio::task::spawn_blocking(move || sodium::derive_key(&password, &salt_owned)).await.ok().flatten()?;
        state.keys.insert(salt, derived.clone());
        derived
      }
    };
    Some(Some((key, json!({"type": "personal", "keyFingerprint": fingerprint}))))
  }

  /// Create one task; returns the parsed response (`{}` when unreadable).
  async fn create_task(&self, client: &reqwest::Client, payload: &Value, target: Option<&str>) -> Option<serde_json::Map<String, Value>> {
    let (name, value) = self.auth_header();
    let url = format!("{}/v1/tasks/json", service_base("https://api.simplepu.sh"));
    let to = target.map(|t| format!(" to {}", t)).unwrap_or_default();
    tracing::debug!("Simplepush POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let body = serde_json::to_vec(payload).ok()?;
    let resp = match client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").header(name, value).body(body).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Simplepush notification.");
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if !matches!(status.as_u16(), 200 | 201) {
      tracing::warn!("Failed to send Simplepush notification{}: {}error={}.", to, Self::error_message(status, &body), status.as_u16());
      return None;
    }
    tracing::info!("Sent Simplepush notification{}.", to);
    match serde_json::from_slice::<Value>(&body) {
      Ok(Value::Object(map)) => Some(map),
      _ => Some(serde_json::Map::new()),
    }
  }

  /// Upload one attachment: presign, PUT the bytes, complete. A failed
  /// upload is reported to the server so the recipient sees it as failed.
  async fn upload(&self, client: &reqwest::Client, attachment_id: &str, blob: &[u8]) -> bool {
    let (name, value) = self.auth_header();
    let base = service_base("https://api.simplepu.sh");
    let step = |step: &'static str| {
      let url = format!("{}/v1/attachments/{}/{}", base, attachment_id, step);
      let request = client.post(url).header("User-Agent", APP_ID).header(name, value.clone());
      async move {
        let resp = request.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.bytes().await.unwrap_or_default();
        if !matches!(status.as_u16(), 200 | 201 | 204) {
          return Err(format!("{}: {}error={}", step, Self::error_message(status, &body), status.as_u16()));
        }
        Ok(body)
      }
    };

    let result: Result<(), String> = async {
      let presign = step("upload-url").await?;
      let presigned = serde_json::from_slice::<Value>(&presign)
        .ok()
        .and_then(|v| v.get("presignedPutUrl").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| "upload-url: no presignedPutUrl".to_string())?;
      let resp = client.put(presigned).header("Content-Type", "application/octet-stream").body(blob.to_vec()).send().await.map_err(|e| e.to_string())?;
      if !matches!(resp.status().as_u16(), 200 | 201 | 204) {
        return Err(format!("upload: error={}", resp.status().as_u16()));
      }
      step("complete").await?;
      Ok(())
    }
    .await;

    if let Err(e) = result {
      tracing::warn!("Failed to upload Simplepush attachment {}: {}", attachment_id, e);
      if let Err(e) = step("failed").await {
        tracing::debug!("Socket Exception: {}", e);
      }
      return false;
    }
    tracing::info!("Uploaded Simplepush attachment {}.", attachment_id);
    true
  }
}

#[async_trait]
impl Notify for SimplePush {
  fn schemas(&self) -> &[&str] {
    &["spush", "simplepush"]
  }
  fn service_name(&self) -> &str {
    "Simplepush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn attachment_support(&self) -> bool {
    true
  }
  // Encryption grows a field to base64(24 byte nonce + text + 16 byte tag);
  // these leave room for the ciphertext of ASCII text
  fn body_maxlen(&self) -> usize {
    7000
  }
  fn title_maxlen(&self) -> usize {
    300
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // (filename, content type, bytes) of every attachment
    let files: Vec<(String, String, &[u8])> = ctx
      .attachments
      .iter()
      .enumerate()
      .map(|(i, Attachment { name, mime_type, data })| {
        let name = if name.is_empty() { format!("file{:03}.dat", i + 1) } else { name.clone() };
        let mime = if mime_type.is_empty() { "application/octet-stream".to_string() } else { mime_type.clone() };
        (name, mime, data.as_slice())
      })
      .collect();

    // One task per topic and member; a single task for a broadcast or a
    // send to your own devices
    let mut targets: Vec<(Option<&str>, Option<&str>)> = self.topics.iter().map(|t| (Some("topic"), Some(t.as_str()))).collect();
    targets.extend(self.members.iter().map(|m| (Some("member"), Some(m.as_str()))));
    if targets.is_empty() {
      targets.push((None, None));
    }

    let client = build_client(self.verify_certificate)?;
    let mut state = self.state.lock().await;
    let mut has_error = false;

    for (kind, target) in targets {
      let mut payload = serde_json::Map::new();
      if let (Some(kind), Some(target)) = (kind, target) {
        payload.insert(kind.into(), json!(target));
      }
      if self.broadcast {
        payload.insert("broadcast".into(), json!(true));
      }
      if self.shared {
        payload.insert("shared".into(), json!(true));
      }
      if let Some(token) = self.topic_auth_token.as_ref().filter(|_| kind == Some("topic")) {
        payload.insert("topicAuthToken".into(), json!(token));
      }
      if let Some(priority) = self.priority {
        payload.insert("priority".into(), json!(priority));
      }
      if let Some(volume) = self.critical_volume {
        payload.insert("criticalVolume".into(), json!(volume));
      }
      if !ctx.body.is_empty() && ctx.body_format == NotifyFormat::Markdown {
        payload.insert("contentFormat".into(), json!("markdown"));
      }

      // The server requires content; a message without a body carries its
      // title there, or else the names of its attachments
      let names = files.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>().join(", ");
      let content = if !ctx.body.is_empty() {
        ctx.body.clone()
      } else if !ctx.title.is_empty() {
        ctx.title.clone()
      } else {
        names
      };
      let title = if ctx.body.is_empty() { String::new() } else { ctx.title.clone() };
      let mut fields: Vec<(&str, Option<String>)> =
        vec![("title", Some(title).filter(|t| !t.is_empty())), ("content", Some(content).filter(|c| !c.is_empty())), ("tag", self.sptag.clone())];
      let mut links = self.links.clone();
      let mut blobs: Vec<Vec<u8>> = files.iter().map(|(_, _, data)| data.to_vec()).collect();

      let Some(encryption) = self.encryption(&client, &mut state, kind, target).await else {
        has_error = true;
        continue;
      };

      if let Some((key, marker)) = encryption {
        let encrypt = |text: &str| sodium::seal(text.as_bytes(), &key).map(|sealed| b64(&sealed));
        let mut sealed_fields = Vec::new();
        for (name, value) in &fields {
          sealed_fields.push((*name, value.as_deref().map(encrypt)));
        }
        if sealed_fields.iter().any(|(_, v)| matches!(v, Some(None))) {
          tracing::warn!("Simplepush content could not be encrypted.");
          has_error = true;
          continue;
        }
        fields = sealed_fields.into_iter().map(|(name, v)| (name, v.flatten())).collect();

        let too_long = ENCRYPTED_MAXLEN.iter().find_map(|(name, max)| {
          let len = fields.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_ref()).map(String::len).unwrap_or(0);
          (len > *max).then_some((*name, len, *max))
        });
        if let Some((name, len, max)) = too_long {
          tracing::warn!("The Simplepush {} is too long once encrypted; {} characters exceed the limit of {}.", name, len, max);
          has_error = true;
          continue;
        }

        let sealed_links: Option<Vec<String>> = links.iter().map(|l| encrypt(l)).collect();
        let sealed_blobs: Option<Vec<Vec<u8>>> = blobs.iter().map(|b| sodium::seal(b, &key)).collect();
        let (Some(sealed_links), Some(sealed_blobs)) = (sealed_links, sealed_blobs) else {
          tracing::warn!("Simplepush content could not be encrypted.");
          has_error = true;
          continue;
        };
        links = sealed_links;
        blobs = sealed_blobs;
        payload.insert("encryption".into(), marker);
      }

      for (name, value) in fields {
        if let Some(value) = value {
          payload.insert(name.into(), json!(value));
        }
      }
      if !links.is_empty() {
        payload.insert("links".into(), json!(links));
      }
      if !blobs.is_empty() {
        let meta: Vec<Value> = files
          .iter()
          .zip(&blobs)
          .map(|((filename, content_type, _), blob)| {
            json!({"filename": filename, "contentType": content_type, "size": blob.len(), "checksumSha256": b64(&Sha256::digest(blob))})
          })
          .collect();
        payload.insert("files".into(), Value::Array(meta));
      }

      let Some(response) = self.create_task(&client, &Value::Object(payload), target).await else {
        has_error = true;
        continue;
      };

      // Upload the attachments into the rows the task created
      let created = response.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default();
      for (attachment, blob) in created.iter().zip(&blobs) {
        let id = attachment.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let uploaded = match id {
          Some(id) => self.upload(&client, id, blob).await,
          None => false,
        };
        if !uploaded {
          has_error = true;
        }
      }
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  // An organization key wrap made with libsodium in JavaScript, the way the
  // Simplepush CLI wraps organization keys (from upstream's tests)
  const SEED: &str = "RfD2e8N6R92NKuRlF1RHlK7HHe3g2-3pGtdcuc9hOw8";
  const ADMIN_PUBKEY: &str = "+YniNIIKOH65XTGVPLUsdswQU/1CGu2QYYkurmD52k4=";
  #[cfg_attr(not(feature = "simplepush-encryption"), allow(dead_code))]
  const WRAP: &str = "YUb0wK3wrPMFzt6i7a3gkoDmbJ/QG8gbsLvAQDEnpQKgzbJ089V6m4yGCr2uZ7dvu4Hsh1TbtFhddcrSfDBtH0xbIzRU4zzm";
  #[cfg_attr(not(feature = "simplepush-encryption"), allow(dead_code))]
  const MASTER_KEY: &str = "9g21kyfkj0gn/gllmJEtXYvUqV/BFDdcZ/wrc2qrQ0E=";

  fn spi() -> String {
    format!("spi_cred.{}", SEED)
  }

  fn spush(url: &str) -> SimplePush {
    SimplePush::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn body_json(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
  }

  /// Reverse `sodium::seal()` of a base64 field.
  #[cfg(feature = "simplepush-encryption")]
  fn decrypt(field: &Value, key: &[u8; 32]) -> String {
    use chacha20poly1305::aead::{Aead, KeyInit};
    let blob = base64::engine::general_purpose::STANDARD.decode(field.as_str().unwrap()).unwrap();
    let nonce: [u8; 24] = blob[..24].try_into().unwrap();
    let plain = chacha20poly1305::XChaCha20Poly1305::new(key.into()).decrypt((&nonce).into(), &blob[24..]).unwrap();
    String::from_utf8(plain).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let spi = spi();
    let urls = vec![
      format!("spush://{}", "A".repeat(14)),
      format!("simplepush://{}/alerts", "A".repeat(14)),
      format!("spush://{}/alerts/deploys", "B".repeat(14)),
      format!("spush://{}/?to=alerts,deploys", "B".repeat(14)),
      format!("spush://{}/alerts?priority=5&critical_volume=0.5&sptag=db&links=https://a.example%20https://b.example&shared=yes&format=text", "C".repeat(14)),
      format!("spush://{}/alerts?priority=emergency", "C".repeat(14)),
      format!("spush://{}/alerts?topic_auth_token=abcd", "C".repeat(14)),
      format!("spush://{}/@Alice/@Bob/site", spi),
      format!("spush://{}/?broadcast=yes", spi),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }

    // A password needs encryption support
    for url in [format!("spush://secret@{}/alerts", "E".repeat(14)), format!("spush://{}/alerts?password=secret", "E".repeat(14))] {
      assert_eq!(from_url(&url).is_some(), sodium::AVAILABLE, "{}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let spi = spi();
    let c14 = "C".repeat(14);
    let urls = vec![
      "spush://".to_string(),
      format!("spush://{}/alerts?priority=invalid", c14),
      format!("spush://{}/alerts?priority=10", c14),
      format!("spush://{}/alerts?priority=3.5", c14),
      format!("spush://{}/alerts?priority=4&critical_volume=0.5", c14),
      format!("spush://{}/alerts?priority=5&critical_volume=2", c14),
      format!("spush://{}/alerts?priority=5&critical_volume=loud", c14),
      format!("spush://{}/alerts?priority=5&critical_volume=0", c14),
      format!("spush://{}/alerts?links={}", c14, vec!["a:b"; 26].join("%20")),
      // Members and broadcasts need an integration token
      format!("spush://{}/@Alice", "D".repeat(14)),
      format!("spush://{}/?broadcast=yes", "D".repeat(14)),
      format!("spush://{}/@", spi),
      // Organization sends need exactly one kind of target and no password
      format!("spush://{}/site?broadcast=yes", spi),
      spi.clone(),
      format!("spush://secret@{}/site", spi),
      // Malformed integration tokens
      "spush://spi_cred/site".to_string(),
      format!("spush://spi_.{}/site", SEED),
      "spush://spi_cred.AAAA/site".to_string(),
      "spush://spi_cred.A/site".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_options() {
    for (value, expected) in
      [("1", 1), ("5", 5), ("minimal", 1), ("low", 2), ("default", 3), ("normal", 3), ("High", 4), ("critical", 5), ("emergency", 5), ("max", 5)]
    {
      assert_eq!(spush(&format!("spush://token123?priority={}", value)).priority, Some(expected), "{}", value);
    }
    let obj = spush("spush://token123/b/a/a?to=d%20c&links=https://a.example%20%20https://b.example&sptag=%20db%20");
    // Path targets keep their order (and repeats); to= entries are sorted
    assert_eq!(obj.topics, vec!["b", "a", "a", "c", "d"]);
    assert_eq!(obj.links, vec!["https://a.example", "https://b.example"]);
    assert_eq!(obj.sptag.as_deref(), Some("db"));
    assert_eq!(obj.password, None);

    let obj = spush(&format!("spush://{}/site/@Alice%20B", spi()));
    assert!(obj.integration);
    assert_eq!(obj.credential, "spi_cred");
    assert_eq!(obj.seed.as_ref().map(Vec::len), Some(32));
    assert_eq!(obj.members, vec!["Alice B"]);
  }

  #[cfg(feature = "simplepush-encryption")]
  #[test]
  fn test_password() {
    // A '+' in a password is kept
    assert_eq!(spush("spush://token?password=a+b").password.as_deref(), Some("a+b"));
    assert_eq!(spush("spush://pw@token").password.as_deref(), Some("pw"));
    assert_eq!(spush("spush://pw@token?password=other").password.as_deref(), Some("other"));
  }

  #[tokio::test]
  async fn test_send_topics() {
    // Matches pycapture.py for this URL
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v1/tasks/json"))
      .and(header("api-token", "token123"))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"notificationId": "n1"})))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url(
      "spush://token123/alerts/deploys?priority=5&critical_volume=0.5&sptag=db&links=https://a.example%20https://b.example&shared=yes&topic_auth_token=tat",
    )
    .unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
    let expected = |topic: &str| {
      json!({
        "topic": topic, "shared": true, "topicAuthToken": "tat", "priority": 5, "criticalVolume": 0.5, "contentFormat": "markdown",
        "title": "T", "content": "B", "tag": "db", "links": ["https://a.example", "https://b.example"],
      })
    };
    assert_eq!(body_json(&requests[0]), expected("alerts"));
    assert_eq!(body_json(&requests[1]), expected("deploys"));
  }

  #[tokio::test]
  async fn test_send_own_devices_and_title_only() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/tasks/json")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url("spush://token123").unwrap();
    // A message without a body carries its title as content
    let ctx = NotifyContext { title: "Only a title".into(), body: "".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(body_json(&server.received_requests().await.unwrap()[0]), json!({"content": "Only a title"}));
  }

  #[tokio::test]
  async fn test_send_attachments() {
    let server = MockServer::builder().start().await;
    let presigned = format!("{}/s3/put", server.uri());
    Mock::given(method("POST"))
      .and(path("/v1/tasks/json"))
      .respond_with(ResponseTemplate::new(201).set_body_json(json!({"taskId": "t", "attachments": [{"id": "att_1"}]})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1/attachments/att_1/upload-url"))
      .and(header("api-token", "token123"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"presignedPutUrl": presigned})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("PUT"))
      .and(path("/s3/put"))
      .and(header("content-type", "application/octet-stream"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/attachments/att_1/complete")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;

    let svc = from_url("spush://token123/a?format=text").unwrap();
    let attachment = Attachment { name: "photo.jpg".into(), data: b"jpeg-bytes".to_vec(), mime_type: "image/jpeg".into() };
    let ctx = NotifyContext { attachments: vec![attachment], ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
      body_json(&requests[0]),
      json!({"topic": "a", "content": "photo.jpg", "files": [{"filename": "photo.jpg", "contentType": "image/jpeg", "size": 10, "checksumSha256": "ARHbw5i5TqzaZ1mAnAUFMIaO5+MTszgcL5XOi1UzHFA="}]})
    );
    assert_eq!(requests[2].body, b"jpeg-bytes");
  }

  #[tokio::test]
  async fn test_failed_upload_is_reported() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v1/tasks/json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"attachments": [{"id": "att_1"}]})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/attachments/att_1/upload-url")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/v1/attachments/att_1/failed")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    let svc = from_url("spush://token123/a").unwrap();
    let ctx = NotifyContext {
      body: "B".into(),
      attachments: vec![Attachment { name: String::new(), data: b"x".to_vec(), mime_type: String::new() }],
      ..Default::default()
    };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
      body_json(&requests[0])["files"][0],
      json!({"filename": "file001.dat", "contentType": "application/octet-stream", "size": 1, "checksumSha256": b64(&Sha256::digest(b"x"))})
    );
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    for (status, body) in [(401, r#"{"error": "authorization_error", "msg": "Invalid API token"}"#), (999, ""), (500, "garbage")] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(2).mount(&server).await;
      let svc = from_url("spush://token123/a/b").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
    // An unreadable success response still counts as sent
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).expect(1).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), from_url("spush://token123").unwrap().send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_organization_without_encryption() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
      .and(path("/v1/org/integration/keys"))
      .and(header("authorization", "Bearer spi_cred"))
      .and(header("accept", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"scopes": ["send"], "enabled": false})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1/tasks/json"))
      .and(header("authorization", "Bearer spi_cred"))
      .respond_with(ResponseTemplate::new(200))
      .expect(4)
      .mount(&server)
      .await;

    let svc = from_url(&format!("spush://{}/site/@Alice%20B?format=text", spi())).unwrap();
    let ctx = NotifyContext { body: "Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // The keys are fetched once
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method == wiremock::http::Method::POST).collect();
    assert!(requests[0].headers.get("api-token").is_none());
    assert_eq!(body_json(&requests[0]), json!({"topic": "site", "content": "Body"}));
    assert_eq!(body_json(&requests[1]), json!({"member": "Alice B", "content": "Body"}));
  }

  #[tokio::test]
  async fn test_organization_key_failures() {
    let ctx = NotifyContext { body: "Body".into(), ..Default::default() };
    for response in [
      json!({"scopes": [], "enabled": false}),
      json!({"scopes": ["send"], "enabled": true}),
      json!({"scopes": ["send"], "enabled": true, "adminPubkeyB64": ADMIN_PUBKEY, "wrappedKeys": [{"version": 1, "blob": "AAAA"}]}),
    ] {
      let server = MockServer::builder().start().await;
      Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(response.clone())).mount(&server).await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
      let svc = from_url(&format!("spush://{}/?broadcast=yes", spi())).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "{}", response);
    }
    let server = MockServer::builder().start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let svc = from_url(&format!("spush://{}/?broadcast=yes", spi())).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[cfg(feature = "simplepush-encryption")]
  #[test]
  fn test_reference_vectors() {
    // The key and fingerprint every Simplepush client derives for the
    // password "secret" and the topic "alerts"
    let (key, fingerprint) = sodium::derive_key("secret", "alerts").unwrap();
    assert_eq!(b64(&key), "9mI0HX1p1U2VckiSHX8jBmHx/6gqIXYEPctmfuN9ak4=");
    assert_eq!(fingerprint, "CQaZgA2pRm0=");

    // The organization key unwrapped from a libsodium crypto_box
    let seed = b64_lenient(SEED).unwrap();
    let opened = sodium::box_open(&b64_lenient(WRAP).unwrap(), &b64_lenient(ADMIN_PUBKEY).unwrap(), &seed).unwrap();
    assert_eq!(b64(&opened), MASTER_KEY);
    // A tampered wrap is refused
    let mut tampered = b64_lenient(WRAP).unwrap();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(sodium::box_open(&tampered, &b64_lenient(ADMIN_PUBKEY).unwrap(), &seed).is_none());

    // Sealed fields: nonce(24) + ciphertext + tag(16)
    let sealed = sodium::seal(b"Body", &key).unwrap();
    assert_eq!(sealed.len(), 24 + 4 + 16);
    assert_eq!(decrypt(&json!(b64(&sealed)), &key), "Body");
  }

  #[cfg(feature = "simplepush-encryption")]
  #[tokio::test]
  async fn test_send_encrypted_topic() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/tasks/json")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    // A topic send is salted with the topic; the account salt is not fetched
    Mock::given(method("GET")).and(path("/v1/user")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;

    let svc = from_url("spush://secret@token123/alerts?sptag=db&links=https://a.example").unwrap();
    let ctx = NotifyContext { title: "Title".into(), body: "Body".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let (key, fingerprint) = sodium::derive_key("secret", "alerts").unwrap();
    let posts: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method == wiremock::http::Method::POST).collect();
    let payload = body_json(&posts[0]);
    assert_eq!(payload["topic"], "alerts");
    assert_eq!(payload["encryption"], json!({"type": "personal", "keyFingerprint": fingerprint}));
    assert_eq!(decrypt(&payload["title"], &key), "Title");
    assert_eq!(decrypt(&payload["content"], &key), "Body");
    assert_eq!(decrypt(&payload["tag"], &key), "db");
    assert_eq!(decrypt(&payload["links"][0], &key), "https://a.example");
    // The render hint stays readable
    assert_eq!(payload["contentFormat"], "markdown");
  }

  #[cfg(feature = "simplepush-encryption")]
  #[tokio::test]
  async fn test_send_encrypted_own_devices() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
      .and(path("/v1/user"))
      .and(header("api-token", "token123"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"passwordSalt": "salt123"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/tasks/json")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let svc = from_url("spush://secret@token123").unwrap();
    let ctx = NotifyContext { body: "Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // The salt is fetched once
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let (key, fingerprint) = sodium::derive_key("secret", "salt123").unwrap();
    let posts: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method == wiremock::http::Method::POST).collect();
    let payload = body_json(&posts[0]);
    assert!(payload.get("topic").is_none() && payload.get("title").is_none());
    assert_eq!(payload["encryption"]["keyFingerprint"], fingerprint);
    assert_eq!(decrypt(&payload["content"], &key), "Body");
  }

  #[cfg(feature = "simplepush-encryption")]
  #[tokio::test]
  async fn test_send_encrypted_limits_and_salt_failures() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/tasks/json")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("spush://secret@token123/alerts").unwrap();
    // Multi-byte text within the character limits is too long once encrypted
    for ctx in
      [NotifyContext { body: "日".repeat(7000), ..Default::default() }, NotifyContext { title: "日".repeat(300), body: "Body".into(), ..Default::default() }]
    {
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
    // ASCII text of the same length fits
    let ctx = NotifyContext { title: "a".repeat(300), body: "a".repeat(7000), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // The salt lookup fails, the account has no salt, or the response is unreadable
    for response in [
      ResponseTemplate::new(401).set_body_json(json!({"msg": "no"})),
      ResponseTemplate::new(200).set_body_json(json!({})),
      ResponseTemplate::new(200).set_body_string("garbage"),
    ] {
      let server = MockServer::builder().start().await;
      Mock::given(method("GET")).and(path("/v1/user")).respond_with(response).mount(&server).await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
      let svc = from_url("spush://secret@token123").unwrap();
      let ctx = NotifyContext { body: "Body".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[cfg(feature = "simplepush-encryption")]
  #[tokio::test]
  async fn test_send_organization_encrypted() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
      .and(path("/v1/org/integration/keys"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "scopes": ["send"], "enabled": true, "adminPubkeyB64": ADMIN_PUBKEY,
        "wrappedKeys": [{"version": 1, "blob": WRAP}],
      })))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/tasks/json")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let svc = from_url(&format!("spush://{}/@Alice?sptag=db&links=https://a.example", spi())).unwrap();
    let ctx = NotifyContext { title: "Title".into(), body: "Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let key: [u8; 32] = base64::engine::general_purpose::STANDARD.decode(MASTER_KEY).unwrap().try_into().unwrap();
    let posts: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method == wiremock::http::Method::POST).collect();
    let payload = body_json(&posts[0]);
    assert_eq!(payload["member"], "Alice");
    assert_eq!(payload["encryption"], json!({"type": "org", "v": 1}));
    assert_eq!(decrypt(&payload["content"], &key), "Body");
    assert_eq!(decrypt(&payload["title"], &key), "Title");
    assert_eq!(decrypt(&payload["tag"], &key), "db");
    assert_eq!(decrypt(&payload["links"][0], &key), "https://a.example");
  }

  #[test]
  fn test_service_details() {
    let details = SimplePush::static_details();
    assert_eq!(details.service_name, "Simplepush");
    assert_eq!(details.protocols, vec!["spush", "simplepush"]);
    let svc = from_url("spush://token123").unwrap();
    assert!(svc.attachment_support());
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!((svc.body_maxlen(), svc.title_maxlen()), (7000, 300));
  }
}

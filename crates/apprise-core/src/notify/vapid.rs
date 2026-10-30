//! WebPush notifications signed with VAPID.
//!
//! Every message is encrypted for its browser subscription with the
//! RFC 8291 `aes128gcm` content encoding and posted to the endpoint the
//! browser issued (RFC 8030), with a VAPID ES256 JWT addressed to that
//! endpoint's origin (RFC 8292).
//!
//! Without `?keyfile=` / `?subfile=` the private key and `subscriptions.json`
//! live in the plugin's persistent storage directory, as upstream keeps
//! them: a missing key pair is generated there and a starter subscription
//! file written for the user to fill in. Endpoints that expired in a
//! subscription file that cannot be rewritten are remembered in the
//! persistent store for 30 days (in memory when there is no store).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{aead, agreement, hkdf};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::email::{is_email, parse_list};
use crate::utils::keyfile::{pem_blocks, read_source};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::pem::{find_private_keyfile, find_public_keyfile};

/// Store key of the endpoints retired from a read-only subscription file.
const RETIRED_KEY: &str = "retired";

/// How long retired endpoints are remembered without updates (30 days).
const RETIRED_EXPIRY: std::time::Duration = std::time::Duration::from_secs(60 * 60 * 24 * 30);

/// The subscription file kept in the storage directory.
const DEFAULT_SUBFILE: &str = "subscriptions.json";

/// Upstream `VAPID_PUSH_MODES`; the first entry is the default.
const VAPID_PUSH_MODES: &[&str] = &["chrome", "firefox", "edge", "opera", "apple"];

/// Highest accepted `?ttl=` value (upstream `template_args["ttl"]["max"]`).
const VAPID_TTL_MAX: i64 = 60;

/// Lifetime of a VAPID JWT (12 hours).
const VAPID_JWT_EXPIRATION_SEC: u64 = 43200;

/// Largest private key file accepted (`ApprisePEMController.max_pem_private_key_size`).
const MAX_KEYFILE_SIZE: usize = 8000;

/// Largest subscription file accepted (`max_vapid_subfile_size`).
const MAX_SUBFILE_SIZE: usize = 5_242_880;

/// Bad entries tolerated before a subscription file is abandoned.
const MAX_LOAD_FAILURE_COUNT: usize = 3;

/// Record size announced in the `aes128gcm` header (`max_webpush_record_size`).
const WEBPUSH_RECORD_SIZE: u32 = 4096;

/// DER `AlgorithmIdentifier` for an id-ecPublicKey on prime256v1, used to
/// wrap a SEC1 `EC PRIVATE KEY` into PKCS#8.
const P256_ALGORITHM_ID: &[u8] =
  &[0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

pub struct Vapid {
  subscriber: String,
  mode: String,
  keyfile: Option<String>,
  subfile: Option<String>,
  ttl: i64,
  verify_certificate: bool,
  tags: Vec<String>,
  state: Mutex<VapidState>,
}

/// What a `Vapid` object learns while sending; loaded once, as upstream does.
struct VapidState {
  /// Remaining targets; unknown ones are dropped after their first miss.
  targets: Vec<String>,
  key_attempted: bool,
  key: Option<EcdsaKeyPair>,
  subs_attempted: bool,
  subscriptions: Vec<(String, Subscription)>,
  /// The subscription file, when it was read from the local disk.
  local_file: Option<PathBuf>,
  /// Endpoints that expired but could not be pruned from their file.
  retired: BTreeSet<String>,
}

impl VapidState {
  fn subscription(&self, name: &str) -> Option<&Subscription> {
    let name = name.to_lowercase();
    self.subscriptions.iter().find(|(n, _)| *n == name).map(|(_, s)| s)
  }
}

/// One browser subscription (`WebPushSubscription`).
#[derive(Debug, Clone)]
struct Subscription {
  endpoint: String,
  /// The JWT audience: `https://host[:port]`.
  origin: String,
  /// The endpoint's path and query, used to redirect requests in tests.
  #[cfg_attr(not(test), allow(dead_code))]
  path_and_query: String,
  /// The browser's P-256 key as an uncompressed point.
  public_key: Vec<u8>,
  auth_secret: Vec<u8>,
}

impl Subscription {
  /// Validate a subscription object as upstream `WebPushSubscription.load()`.
  fn from_json(content: &Value) -> Option<Self> {
    let content = content.as_object()?;
    let endpoint = content.get("endpoint").and_then(Value::as_str).map(str::trim)?;
    let (origin, path_and_query) = webpush_origin(endpoint).or_else(|| {
      tracing::debug!("Vapid subscription endpoint is unusable");
      None
    })?;

    let keys = content.get("keys");
    let p256dh = keys.and_then(|k| k.get("p256dh")).and_then(Value::as_str).and_then(b64_decode).filter(|k| !k.is_empty());
    let auth_secret = keys.and_then(|k| k.get("auth")).and_then(Value::as_str).and_then(b64_decode).filter(|k| !k.is_empty());
    let (Some(public_key), Some(auth_secret)) = (p256dh, auth_secret) else {
      tracing::debug!("Vapid subscription keys are missing or not base64");
      return None;
    };

    if !is_p256_point(&public_key) {
      tracing::debug!("Vapid subscription p256dh key is not usable");
      return None;
    }

    Some(Self { endpoint: endpoint.to_string(), origin, path_and_query, public_key, auth_secret })
  }

  /// Upstream `str(subscription)`: the first 16 characters of the last
  /// endpoint path entry, used to name a file holding a single entry.
  fn default_name(&self) -> String {
    self.endpoint.rsplit('/').next().unwrap_or("").chars().take(16).collect()
  }
}

/// Upstream `webpush_origin()`: the HTTPS origin of an endpoint, or `None`
/// when it is not a usable request target (not HTTPS, credentials, a
/// fragment, port 0, no host). Also returns the path and query.
fn webpush_origin(endpoint: &str) -> Option<(String, String)> {
  if endpoint.contains('#') {
    return None;
  }
  let url = url::Url::parse(endpoint).ok()?;
  if url.scheme() != "https" {
    return None;
  }
  // An empty `https://@host` still carries credentials
  let authority = endpoint.split_once("://").map(|(_, rest)| rest.split(['/', '?']).next().unwrap_or("")).unwrap_or("");
  if authority.contains('@') {
    return None;
  }
  let host = url.host_str().filter(|h| !h.is_empty())?;
  let origin = match url.port() {
    Some(0) => return None,
    Some(port) => format!("https://{}:{}", host, port),
    None => format!("https://{}", host),
  };
  let path_and_query = match url.query() {
    Some(q) => format!("{}?{}", url.path(), q),
    None => url.path().to_string(),
  };
  Some((origin, path_and_query))
}

/// Python's `urlsafe_b64decode` with the padding put back: both alphabets
/// and unpadded input are accepted.
fn b64_decode(data: &str) -> Option<Vec<u8>> {
  let engine = base64::engine::GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true).with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
  );
  let normalized: String = data
    .trim()
    .chars()
    .map(|c| match c {
      '+' => '-',
      '/' => '_',
      c => c,
    })
    .collect();
  engine.decode(normalized.trim_end_matches('=')).ok()
}

fn b64url(data: &[u8]) -> String {
  base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Whether `point` is an uncompressed P-256 public key on the curve.
fn is_p256_point(point: &[u8]) -> bool {
  let rng = SystemRandom::new();
  let Ok(probe) = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng) else { return false };
  agreement::agree_ephemeral(probe, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, point), |_| ()).is_ok()
}

// ── Keys and encryption ──────────────────────────────────────────────────────

/// DER tag-length-value.
fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
  let mut out = vec![tag];
  let len = content.len();
  if len < 0x80 {
    out.push(len as u8);
  } else {
    let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
    out.push(0x80 | bytes.len() as u8);
    out.extend(bytes);
  }
  out.extend_from_slice(content);
  out
}

/// Wrap a SEC1 `ECPrivateKey` in a PKCS#8 `PrivateKeyInfo`.
fn sec1_to_pkcs8(sec1: &[u8]) -> Vec<u8> {
  let mut content = vec![0x02, 0x01, 0x00];
  content.extend_from_slice(P256_ALGORITHM_ID);
  content.extend(der_tlv(0x04, sec1));
  der_tlv(0x30, &content)
}

/// Load a P-256 private key from PEM (PKCS#8 `PRIVATE KEY` or SEC1
/// `EC PRIVATE KEY`). The key must carry its public key, as keys written by
/// OpenSSL and Python's `cryptography` do.
fn load_private_key(pem: &[u8]) -> Option<EcdsaKeyPair> {
  let rng = SystemRandom::new();
  let text = String::from_utf8_lossy(pem);
  pem_blocks(&text).into_iter().find_map(|(label, der)| {
    let pkcs8 = match label.as_str() {
      "PRIVATE KEY" => der,
      "EC PRIVATE KEY" => sec1_to_pkcs8(&der),
      _ => return None,
    };
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).ok()
  })
}

struct HkdfLen(usize);

impl hkdf::KeyType for HkdfLen {
  fn len(&self) -> usize {
    self.0
  }
}

fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>, ring::error::Unspecified> {
  let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
  let info = [info];
  let okm = prk.expand(&info, HkdfLen(len))?;
  let mut out = vec![0u8; len];
  okm.fill(&mut out)?;
  Ok(out)
}

/// Encrypt `message` for a subscription (RFC 8291, `aes128gcm`), as
/// upstream `ApprisePEMController.encrypt_webpush()` does, with a fresh
/// sender key and salt.
fn encrypt_webpush(message: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>, ring::error::Unspecified> {
  let rng = SystemRandom::new();
  let ephemeral = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)?;
  let as_public = ephemeral.compute_public_key()?;
  let mut salt = [0u8; 16];
  rng.fill(&mut salt)?;
  agreement::agree_ephemeral(ephemeral, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public), |ecdh_secret| {
    encrypt_record(message, ua_public, as_public.as_ref(), ecdh_secret, auth_secret, &salt)
  })?
}

/// The deterministic half of [`encrypt_webpush`]: one record of
/// `salt || rs || idlen || sender key || ciphertext`.
fn encrypt_record(
  message: &[u8],
  ua_public: &[u8],
  as_public: &[u8],
  ecdh_secret: &[u8],
  auth_secret: &[u8],
  salt: &[u8],
) -> Result<Vec<u8>, ring::error::Unspecified> {
  let key_info = [b"WebPush: info\x00".as_slice(), ua_public, as_public].concat();
  let ikm = hkdf_sha256(auth_secret, ecdh_secret, &key_info, 32)?;
  let cek = hkdf_sha256(salt, &ikm, b"Content-Encoding: aes128gcm\x00", 16)?;
  let nonce = hkdf_sha256(salt, &ikm, b"Content-Encoding: nonce\x00", 12)?;

  // A single record ends with the 0x02 padding delimiter
  let mut record = Vec::with_capacity(message.len() + 17);
  record.extend_from_slice(message);
  record.push(0x02);
  let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek)?);
  key.seal_in_place_append_tag(aead::Nonce::try_assume_unique_for_key(&nonce)?, aead::Aad::empty(), &mut record)?;

  let mut out = Vec::with_capacity(21 + as_public.len() + record.len());
  out.extend_from_slice(salt);
  out.extend_from_slice(&WEBPUSH_RECORD_SIZE.to_be_bytes());
  out.push(as_public.len() as u8);
  out.extend_from_slice(as_public);
  out.extend(record);
  Ok(out)
}

/// A VAPID JWT for `audience` signed with ES256.
fn jwt_token(key: &EcdsaKeyPair, audience: &str, subscriber: &str) -> Option<String> {
  let exp = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() + VAPID_JWT_EXPIRATION_SEC;
  let header = r#"{"alg":"ES256","typ":"JWT"}"#;
  let payload =
    format!(r#"{{"aud":{},"exp":{},"sub":{}}}"#, serde_json::to_string(audience).ok()?, exp, serde_json::to_string(&format!("mailto:{}", subscriber)).ok()?);
  let signing_input = format!("{}.{}", b64url(header.as_bytes()), b64url(payload.as_bytes()));
  let signature = key.sign(&SystemRandom::new(), signing_input.as_bytes()).ok()?;
  Some(format!("{}.{}", signing_input, b64url(signature.as_ref())))
}

// ── Files ────────────────────────────────────────────────────────────────────

/// A JSON object read with its top-level entries in file order.
struct OrderedObject(Vec<(String, Value)>);

impl<'de> serde::Deserialize<'de> for OrderedObject {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
      type Value = OrderedObject;
      fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
      }
      fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<OrderedObject, A::Error> {
        let mut entries: Vec<(String, Value)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Value>()? {
          // A repeated key keeps its first position and its last value
          match entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
          }
        }
        Ok(OrderedObject(entries))
      }
    }
    deserializer.deserialize_map(Visitor)
  }
}

/// Load subscriptions as upstream `WebPushSubscriptionManager.load()`: a
/// single `{endpoint, keys}` entry or an object of named entries. A file
/// with more than [`MAX_LOAD_FAILURE_COUNT`] bad entries is abandoned.
fn parse_subscriptions(data: &[u8]) -> Option<Vec<(String, Subscription)>> {
  let Ok(OrderedObject(entries)) = serde_json::from_slice::<OrderedObject>(data) else {
    tracing::warn!("Vapid subscription file does not hold a valid JSON object");
    return None;
  };

  if entries.iter().any(|(k, _)| k == "endpoint") && entries.iter().any(|(k, _)| k == "keys") {
    let single = Value::Object(entries.into_iter().collect());
    let Some(subscription) = Subscription::from_json(&single) else {
      tracing::warn!("Vapid subscription could not be loaded");
      return None;
    };
    return Some(vec![(subscription.default_name().to_lowercase(), subscription)]);
  }

  let mut subscriptions: Vec<(String, Subscription)> = Vec::new();
  let mut errors = 0;
  for (name, content) in entries {
    let name = name.to_lowercase();
    match Subscription::from_json(&content) {
      Some(subscription) => match subscriptions.iter_mut().find(|(n, _)| *n == name) {
        Some(entry) => entry.1 = subscription,
        None => subscriptions.push((name, subscription)),
      },
      None => {
        errors += 1;
        tracing::warn!("Vapid subscription ({}) could not be loaded", name);
        if errors > MAX_LOAD_FAILURE_COUNT {
          tracing::warn!("Vapid subscription file abandoned after {} bad entries", errors);
          return None;
        }
      }
    }
  }
  tracing::debug!("Loaded {} Vapid subscription(s)", subscriptions.len());
  Some(subscriptions)
}

/// Whether a local file can be written (upstream `os.access(path, W_OK)`).
fn is_writable(path: &Path) -> bool {
  std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

/// Remove named entries from the subscription file on disk, as upstream
/// `WebPushSubscriptionManager.prune()`: the file is read again so entries
/// we could not load (and changes made since) are kept, then atomically
/// replaced. A file holding a single entry is never pruned.
fn prune_file(path: &Path, names: &[String]) -> bool {
  let Ok(data) = std::fs::read(path) else { return false };
  let Ok(OrderedObject(entries)) = serde_json::from_slice::<OrderedObject>(&data) else {
    tracing::warn!("Vapid subscription file does not hold a valid JSON object; not pruning");
    return false;
  };
  if entries.iter().any(|(k, _)| k == "endpoint") && entries.iter().any(|(k, _)| k == "keys") {
    tracing::debug!("Vapid subscription file holds a single entry; nothing to prune");
    return false;
  }

  let wanted: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
  let total = entries.len();
  let remaining: Vec<(String, Value)> = entries.into_iter().filter(|(name, _)| !wanted.contains(&name.to_lowercase())).collect();
  if remaining.len() == total {
    return true;
  }

  let pretty: Vec<String> = remaining
    .iter()
    .map(|(name, value)| {
      let value = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into()).replace('\n', "\n  ");
      format!("  {}: {}", serde_json::to_string(name).unwrap_or_default(), value)
    })
    .collect();
  let content = if pretty.is_empty() { "{}".to_string() } else { format!("{{\n{}\n}}", pretty.join(",\n")) };

  if !atomic_write(path, content.as_bytes()) {
    return false;
  }
  tracing::info!("Pruned {} expired Vapid subscription(s)", total - remaining.len());
  true
}

/// Replace `path` without exposing a partly written file: write a
/// temporary file beside the target (following symlinks), carry the
/// target's permissions over, then rename it into place.
fn atomic_write(path: &Path, content: &[u8]) -> bool {
  use std::io::Write;
  let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  let dir = target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
  let base = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
  let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
  let tmp = dir.join(format!("{}.{}{}.tmp", base, std::process::id(), nanos));

  let result = (|| -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    file.write_all(content)?;
    file.sync_all()?;
    if let Ok(meta) = std::fs::metadata(&target) {
      std::fs::set_permissions(&tmp, meta.permissions())?;
    } else {
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
      }
    }
    std::fs::rename(&tmp, &target)
  })();

  if let Err(e) = result {
    tracing::warn!("Error writing Vapid subscription file");
    tracing::debug!("I/O Exception: {}", e);
    let _ = std::fs::remove_file(&tmp);
    return false;
  }
  true
}

// ── Persistent storage ───────────────────────────────────────────────────────

/// The plugin's storage directory, when its store lives on disk.
fn store_dir() -> Option<PathBuf> {
  crate::storage::with_service_store(|s| s.path().map(Path::to_path_buf)).flatten()
}

/// `der` as a PEM block, wrapped at 64 characters as Python's
/// `cryptography` writes it.
fn pem_encode(label: &str, der: &[u8]) -> String {
  let encoded = base64::engine::general_purpose::STANDARD.encode(der);
  let mut out = format!("-----BEGIN {}-----\n", label);
  for line in encoded.as_bytes().chunks(64) {
    out.push_str(&String::from_utf8_lossy(line));
    out.push('\n');
  }
  out.push_str(&format!("-----END {}-----\n", label));
  out
}

/// The private key kept in `dir` (upstream `ApprisePEMController` with
/// `pem_autogen`): a missing key pair is generated and written there as
/// `private_key.pem` (PKCS#8) and `public_key.pem`. Nothing is generated
/// when a public key is already there on its own.
fn store_private_key(dir: &Path) -> Option<EcdsaKeyPair> {
  let dir_str = dir.to_str()?;
  if let Some(path) = find_private_keyfile(dir_str, None) {
    return std::fs::read(path).ok().and_then(|data| load_private_key(&data));
  }
  if find_public_keyfile(dir_str, None).is_some() {
    return None;
  }
  let rng = SystemRandom::new();
  let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).ok()?;
  let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).ok()?;
  let point = [&[0u8][..], key.public_key().as_ref()].concat();
  let spki = der_tlv(0x30, &[P256_ALGORITHM_ID, &der_tlv(0x03, &point)].concat());
  let (public_path, private_path) = (dir.join("public_key.pem"), dir.join("private_key.pem"));
  if let Err(e) = std::fs::write(&public_path, pem_encode("PUBLIC KEY", &spki)) {
    tracing::warn!("Error writing Public PEM file {}", public_path.display());
    tracing::debug!("I/O Exception: {}", e);
    let _ = std::fs::remove_file(&public_path);
    return None;
  }
  if let Err(e) = crate::utils::disk::write_private(&private_path, pem_encode("PRIVATE KEY", pkcs8.as_ref()).as_bytes()) {
    tracing::warn!("Error writing Private PEM file {}", private_path.display());
    tracing::debug!("I/O Exception: {}", e);
    let _ = std::fs::remove_file(&public_path);
    let _ = std::fs::remove_file(&private_path);
    return None;
  }
  Some(key)
}

/// Write a starter subscription file into `dir` for the user to fill in,
/// unless one is already there (upstream `WebPushSubscriptionManager.autogen`).
fn autogen_subscriptions(dir: &Path) {
  let path = dir.join(DEFAULT_SUBFILE);
  if path.exists() {
    return;
  }
  let template = serde_json::json!({
    "user@example.com": {
      "endpoint": "https://fcm.googleapis.com/fcm/send/abc123...",
      "keys": { "p256dh": "<place public key in base64 here>", "auth": "<place auth in base64 here>" },
    }
  });
  let content = serde_json::to_string_pretty(&template).unwrap_or_default();
  if atomic_write(&path, content.as_bytes()) {
    tracing::info!("Vapid auto-generated {}", DEFAULT_SUBFILE);
  }
}

/// The endpoints retired in the persistent store.
fn stored_retired() -> BTreeSet<String> {
  crate::storage::with_service_store(|s| s.get(RETIRED_KEY))
    .flatten()
    .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()))
    .unwrap_or_default()
}

// ── Plugin ───────────────────────────────────────────────────────────────────

impl Vapid {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone();
    let user = url.user.clone();

    let mut targets: Vec<String> = Vec::new();
    let subscriber = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => {
        // Whatever is left on the URL is a target
        match (&user, &host) {
          (Some(user), Some(host)) => targets.push(format!("{}@{}", user, host)),
          (None, Some(host)) => targets.push(host.clone()),
          _ => {}
        }
        // Upstream unquotes ?from= once more
        unquote(from)
      }
      None => format!("{}@{}", user.unwrap_or_default(), host.unwrap_or_default()),
    };

    let Some(subscriber) = is_email(&subscriber).map(|e| e.full_email) else {
      tracing::warn!("An invalid Vapid Subscriber was specified.");
      return None;
    };

    targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      targets.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    let mut targets = parse_list(&targets);
    if targets.is_empty() {
      targets.push(subscriber.clone());
    }

    let mode = match url.get("mode") {
      None => VAPID_PUSH_MODES[0].to_string(),
      Some(mode) => {
        let mode = mode.to_lowercase();
        if !VAPID_PUSH_MODES.contains(&mode.as_str()) {
          tracing::warn!("The Vapid mode specified ({}) is invalid.", mode);
          return None;
        }
        mode
      }
    };

    // An unparseable TTL keeps the default; one out of range is refused
    let ttl = match url.get("ttl").filter(|t| !t.is_empty()) {
      None => 0,
      Some(ttl) => {
        let ttl = unquote(ttl).trim().parse::<i64>().unwrap_or(0);
        if !(0..=VAPID_TTL_MAX).contains(&ttl) {
          tracing::warn!("The Vapid TTL specified ({}) is out of range.", ttl);
          return None;
        }
        ttl
      }
    };

    let keyfile = url.get("keyfile").filter(|k| !k.is_empty()).map(unquote);
    let subfile = url.get("subfile").filter(|s| !s.is_empty()).map(unquote);

    Some(Self {
      subscriber,
      mode,
      keyfile,
      subfile,
      ttl,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      state: Mutex::new(VapidState {
        targets,
        key_attempted: false,
        key: None,
        subs_attempted: false,
        subscriptions: Vec::new(),
        local_file: None,
        retired: BTreeSet::new(),
      }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Vapid Web Push Notifications",
      service_url: Some("https://datatracker.ietf.org/doc/html/draft-thomson-webpush-vapid"),
      setup_url: Some("https://appriseit.com/services/vapid/"),
      protocols: vec!["vapid"],
      description: "Send browser push notifications via WebPush/VAPID.",
      attachment_support: false,
    }
  }

  /// The URL a subscription is delivered to. Tests redirect the endpoint's
  /// origin to a mock server.
  fn delivery_url(subscription: &Subscription) -> String {
    let base = service_base(&subscription.origin);
    if base == subscription.origin { subscription.endpoint.clone() } else { format!("{}{}", base, subscription.path_and_query) }
  }
}

#[async_trait]
impl Notify for Vapid {
  fn schemas(&self) -> &[&str] {
    &["vapid"]
  }
  fn service_name(&self) -> &str {
    "Vapid Web Push Notifications"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    4000
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["vapid".into(), self.mode.clone(), self.subscriber.clone()])
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut state = self.state.lock().await;

    // Without a key or subscription file the storage directory holds them
    let dir = if self.keyfile.is_none() || self.subfile.is_none() { store_dir() } else { None };

    if !state.key_attempted {
      // The key is only loaded on the first notification
      state.key_attempted = true;
      if self.subfile.is_none()
        && let Some(dir) = &dir
      {
        autogen_subscriptions(dir);
      }
      state.key = match (&self.keyfile, &dir) {
        (Some(keyfile), _) => read_source(keyfile, MAX_KEYFILE_SIZE).await.and_then(|(data, _)| load_private_key(&data)),
        (None, Some(dir)) => store_private_key(dir),
        (None, None) => None,
      };
      if state.key.is_none() {
        tracing::warn!("Provided Vapid/WebPush (PEM) Private Key file could not be loaded.");
        return Ok(false);
      }
    }

    if state.targets.is_empty() {
      tracing::warn!("There are no Vapid targets to notify");
      return Ok(false);
    }

    let subfile = self.subfile.clone().or_else(|| dir.as_ref().map(|d| d.join(DEFAULT_SUBFILE).to_string_lossy().into_owned()));
    if let Some(subfile) = &subfile
      && !state.subs_attempted
    {
      // A failed load is not attempted again on the next notification
      state.subs_attempted = true;
      let loaded = read_source(subfile, MAX_SUBFILE_SIZE).await.and_then(|(data, path)| parse_subscriptions(&data).map(|subs| (subs, path)));
      match loaded {
        Some((subscriptions, path)) => {
          state.subscriptions = subscriptions;
          state.local_file = path;
        }
        None => {
          tracing::warn!("Provided Vapid/WebPush subscriptions file could not be loaded.");
          return Ok(false);
        }
      }
    }

    if state.subscriptions.is_empty() {
      tracing::warn!("Vapid could not load subscriptions");
      return Ok(false);
    }

    let Some(key) = state.key.as_ref() else {
      tracing::warn!("No Vapid/WebPush (PEM) Private Key file could be loaded.");
      return Ok(false);
    };
    let public_key = b64url(key.public_key().as_ref());

    let client = build_client(self.verify_certificate)?;
    let ttl = self.ttl.to_string();
    let mut has_error = false;
    let mut expired: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let (mut skipped, mut attempted, mut delivered) = (0usize, 0usize, 0usize);
    let retired = stored_retired();

    for target in state.targets.clone() {
      // A target reached on an earlier attempt still counts as delivered
      if crate::delivery::is_delivered(&target, false) {
        delivered += 1;
        continue;
      }
      let Some(entry) = state.subscription(&target).cloned() else {
        tracing::warn!("Dropped Vapid user ({}) specified - not found in subscriptions.json.", target);
        dropped.push(target);
        has_error = true;
        continue;
      };

      if retired.contains(&entry.endpoint) || state.retired.contains(&entry.endpoint) {
        tracing::debug!("Skipping retired Vapid subscription: {}", target);
        skipped += 1;
        continue;
      }

      // Each endpoint origin needs its own token (RFC 8292)
      let Some(token) = state.key.as_ref().and_then(|key| jwt_token(key, &entry.origin, &self.subscriber)) else {
        tracing::warn!("Vapid JWT could not be signed");
        has_error = true;
        continue;
      };
      let Ok(payload) = encrypt_webpush(ctx.body.as_bytes(), &entry.public_key, &entry.auth_secret) else {
        tracing::warn!("Vapid payload could not be encrypted for {}", target);
        has_error = true;
        continue;
      };

      tracing::debug!("Vapid {} POST {} -> {} (cert_verify={})", self.mode, target, entry.origin, self.verify_certificate);
      tracing::debug!("Vapid {} Encrypted Payload: {} byte(s)", self.mode, ctx.body.len());

      crate::notify::throttle().await;
      attempted += 1;
      let result = client
        .post(Self::delivery_url(&entry))
        .header("User-Agent", APP_ID)
        .header("TTL", &ttl)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", format!("vapid t={}, k={}", token, public_key))
        .body(payload)
        .send()
        .await;

      match result {
        Ok(resp) => match resp.status().as_u16() {
          // RFC 8030 uses 404 for expired subscriptions; services may use 410
          404 | 410 => {
            tracing::info!("Removing expired Vapid subscription: {}", target);
            expired.push(target);
            has_error = true;
          }
          200 | 201 | 202 | 204 => {
            tracing::info!("Sent {} Vapid notification.", self.mode);
            delivered += 1;
            crate::delivery::mark_delivered(&target, false);
          }
          status => {
            let reason = resp.status().canonical_reason().unwrap_or("");
            tracing::warn!("Failed to send {} Vapid notification: {}{}error={}.", self.mode, reason, if reason.is_empty() { "" } else { ", " }, status);
            has_error = true;
          }
        },
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Vapid notification.");
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
        }
      }
    }

    // Unknown targets are not tried again
    state.targets.retain(|t| !dropped.contains(t));

    if !expired.is_empty() {
      let endpoints: Vec<String> = expired.iter().filter_map(|name| state.subscription(name).map(|s| s.endpoint.clone())).collect();
      let names: Vec<String> = expired.iter().map(|n| n.to_lowercase()).collect();
      state.subscriptions.retain(|(name, _)| !names.contains(name));

      // Remove only the expired entries from a writable local file
      let pruned = state.local_file.as_deref().is_some_and(|path| is_writable(path) && prune_file(path, &expired));
      if !pruned {
        tracing::info!("Tracking {} retired Vapid endpoint(s) in persistent storage; the subscription file was not updated", endpoints.len());
        let all: BTreeSet<String> = retired.into_iter().chain(endpoints.iter().cloned()).collect();
        crate::storage::with_service_store(|s| s.set(RETIRED_KEY, serde_json::json!(all), Some(RETIRED_EXPIRY), true));
        state.retired.extend(endpoints);
      }
    }

    if delivered == 0 {
      tracing::warn!("No Vapid subscriptions could be notified ({} skipped, {} attempted)", skipped, attempted);
      return Ok(false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use std::sync::{Arc, Mutex as StdMutex};
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  /// The apprise-go parity key (SEC1 with its public key).
  const SEC1_KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIGIrzHBAsAawDEzaT73VAThO5V+JZ9L9tkOt8OrH0tkWoAoGCCqGSM49
AwEHoUQDQgAEL7Ovf/b1CJXJkGsaNYE8X6lqxgx05or+ak8mamO5waKdvu0Vl8ph
Oc4My77ylXst/I1fVk62i06E2YwM//ZmgQ==
-----END EC PRIVATE KEY-----
";

  fn vapid(url: &str) -> Vapid {
    Vapid::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  /// A browser-side subscription: its private key, `p256dh` and `auth`.
  struct Browser {
    private: agreement::EphemeralPrivateKey,
    p256dh: String,
    auth: Vec<u8>,
  }

  fn browser() -> Browser {
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
    let p256dh = b64url(private.compute_public_key().unwrap().as_ref());
    let mut auth = vec![0u8; 16];
    rng.fill(&mut auth).unwrap();
    Browser { private, p256dh, auth }
  }

  /// RFC 8291 decryption, as a browser does it.
  fn decrypt(browser: Browser, body: &[u8]) -> Vec<u8> {
    let salt = &body[..16];
    assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), WEBPUSH_RECORD_SIZE);
    let idlen = body[20] as usize;
    let as_public = &body[21..21 + idlen];
    let ciphertext = &body[21 + idlen..];
    let ua_public = b64_decode(&browser.p256dh).unwrap();
    let key_info = [b"WebPush: info\x00".as_slice(), &ua_public, as_public].concat();
    let ikm = agreement::agree_ephemeral(browser.private, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public), |shared| {
      hkdf_sha256(&browser.auth, shared, &key_info, 32).unwrap()
    })
    .unwrap();
    let cek = hkdf_sha256(salt, &ikm, b"Content-Encoding: aes128gcm\x00", 16).unwrap();
    let nonce = hkdf_sha256(salt, &ikm, b"Content-Encoding: nonce\x00", 12).unwrap();
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
    let mut data = ciphertext.to_vec();
    let plain = key.open_in_place(aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(), aead::Aad::empty(), &mut data).unwrap();
    assert_eq!(plain.last(), Some(&0x02), "last record delimiter");
    plain[..plain.len() - 1].to_vec()
  }

  fn subscription_json(endpoint: &str, browser: &Browser) -> Value {
    serde_json::json!({ "endpoint": endpoint, "keys": { "p256dh": browser.p256dh, "auth": b64url(&browser.auth) } })
  }

  fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "vapid://user@example.com",
      "vapid://user@example.com?keyfile=invalid&subfile=invalid",
      "vapid://user@example.com/newuser@example.com",
      "vapid://user@example.ca/newuser@example.ca",
      "vapid://user@example.uk/newuser@example.uk",
      "vapid://user@example.au/newuser@example.au",
      "vapid://user@example.com/user1?to=user2",
      "vapid://user1?to=user2&from=user@example.com",
      "vapid://?to=user2&from=user@example.com",
      "vapid://user@example.com?to=user2&from=user@example.com",
      "vapid://user@example.com/user1?to=user2&ttl=15",
      "vapid://user@example.com/user1?to=user2&ttl=",
      "vapid://user@example.com/user1?to=user2&ttl=invalid",
      "vapid://user@example.com/user1?to=user2&mode=edge",
      "vapid://user@example.com?mode=APPLE",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "vapid://",
      "vapid://:@/",
      "vapid://invalid-subscriber",
      "vapid://user@example.com/user1?to=user2&ttl=-4000",
      "vapid://user@example.com?ttl=61",
      "vapid://user@example.com/user1?to=user2&mode=",
      "vapid://user@example.com/user1?to=user2&mode=invalid",
      // Published modes that are not selectable
      "vapid://user@example.com?mode=samsung",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_targets() {
    // The subscriber is notified when no target is given
    let obj = vapid("vapid://user@example.com");
    assert_eq!(obj.subscriber, "user@example.com");
    assert_eq!(obj.mode, "chrome");
    assert_eq!(obj.ttl, 0);
    assert_eq!(obj.state.lock().await.targets, vec!["user@example.com"]);

    // Targets are sorted and unique, the host joins them when from= is set
    let obj = vapid("vapid://x@example.com/c@d.com/b@d.com?from=a@b.com&to=e@f.com,b@d.com");
    assert_eq!(obj.subscriber, "a@b.com");
    assert_eq!(obj.state.lock().await.targets, vec!["b@d.com", "c@d.com", "e@f.com", "x@example.com"]);

    let obj = vapid("vapid://user1?to=user2&from=user@example.com&ttl=15&mode=Edge");
    assert_eq!(obj.state.lock().await.targets, vec!["user1", "user2"]);
    assert_eq!(obj.ttl, 15);
    assert_eq!(obj.mode, "edge");
  }

  #[test]
  fn test_webpush_origin() {
    let origin = |e: &str| webpush_origin(e).map(|(o, _)| o);
    assert_eq!(origin("https://fcm.googleapis.com/fcm/send/abc").as_deref(), Some("https://fcm.googleapis.com"));
    assert_eq!(origin("https://Push.Example.com:443/a").as_deref(), Some("https://push.example.com"));
    assert_eq!(origin("https://push.example.com:8443/a").as_deref(), Some("https://push.example.com:8443"));
    assert_eq!(origin("https://[::1]:8443/a").as_deref(), Some("https://[::1]:8443"));
    assert_eq!(webpush_origin("https://push.example.com/a?b=c").map(|(_, p)| p).as_deref(), Some("/a?b=c"));
    for bad in [
      "http://push.example.com/a",
      "https://user:pw@push.example.com/a",
      "https://@push.example.com/a",
      "https://push.example.com/a#x",
      "https://push.example.com:0/a",
      "ftp://x",
    ] {
      assert_eq!(origin(bad), None, "{}", bad);
    }
  }

  #[test]
  fn test_private_key_formats() {
    let key = load_private_key(SEC1_KEY.as_bytes()).expect("SEC1 key");
    assert_eq!(key.public_key().as_ref().len(), 65);

    // PKCS#8, as ring generates (and upstream keygen writes)
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref()));
    assert!(load_private_key(pem.as_bytes()).is_some());

    // OpenSSL ecparam output carries an EC PARAMETERS block first
    let with_params = format!("-----BEGIN EC PARAMETERS-----\nBggqhkjOPQMBBw==\n-----END EC PARAMETERS-----\n{}", SEC1_KEY);
    assert!(load_private_key(with_params.as_bytes()).is_some());

    assert!(load_private_key(b"garbage").is_none());
    assert!(load_private_key(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n").is_none());
  }

  #[test]
  fn test_jwt_token() {
    let key = load_private_key(SEC1_KEY.as_bytes()).unwrap();
    let token = jwt_token(&key, "https://fcm.googleapis.com", "user@example.com").unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(b64_decode(parts[0]).unwrap(), br#"{"alg":"ES256","typ":"JWT"}"#);
    let claims: Value = serde_json::from_slice(&b64_decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], "https://fcm.googleapis.com");
    assert_eq!(claims["sub"], "mailto:user@example.com");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let exp = claims["exp"].as_u64().unwrap();
    assert!(exp >= now + VAPID_JWT_EXPIRATION_SEC - 5 && exp <= now + VAPID_JWT_EXPIRATION_SEC);

    // The signature verifies against the public key sent as k=
    let signature = b64_decode(parts[2]).unwrap();
    assert_eq!(signature.len(), 64);
    let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key.public_key().as_ref());
    verifier.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).expect("valid ES256 signature");
  }

  #[test]
  fn test_encrypt_round_trip() {
    let b = browser();
    let ua_public = b64_decode(&b.p256dh).unwrap();
    let body = encrypt_webpush("hello wörld".as_bytes(), &ua_public, &b.auth).unwrap();
    // salt(16) + rs(4) + idlen(1) + key(65) + plaintext + delimiter + tag(16)
    assert_eq!(body.len(), 16 + 4 + 1 + 65 + "hello wörld".len() + 1 + 16);
    assert_eq!(body[20], 65);
    assert_eq!(decrypt(b, &body), "hello wörld".as_bytes());
  }

  #[test]
  fn test_encrypt_rfc8291_vector() {
    // RFC 8291 Appendix A. Upstream encrypt_webpush() produces the same
    // body for these inputs (checked against Apprise 2.1.0 by fixing its
    // sender key and salt).
    let d = |s: &str| b64_decode(s).unwrap();
    let body = encrypt_record(
      b"When I grow up, I want to be a watermelon",
      &d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
      &d("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
      &d("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
      &d("BTBZMqHH6r4Tts7J_aSIgg"),
      &d("DGv6ra1nlYgDCS1FRnbzlw"),
    )
    .unwrap();
    assert_eq!(
      b64url(&body),
      "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
    );
  }

  #[test]
  fn test_parse_subscriptions() {
    let b = browser();
    let sub = subscription_json("https://fcm.googleapis.com/fcm/send/abcdefghijklmnopqrstuvwxyz", &b);

    // A single entry is named after its endpoint
    let subs = parse_subscriptions(sub.to_string().as_bytes()).unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].0, "abcdefghijklmnop");

    // Named entries are lower-cased; up to 3 bad ones are skipped
    let bad = serde_json::json!({"endpoint": "http://insecure/x", "keys": {"p256dh": b.p256dh, "auth": "AAAA"}});
    let file = serde_json::json!({"User@Example.com": sub, "bad1": bad, "bad2": {}, "bad3": "x"});
    let subs = parse_subscriptions(file.to_string().as_bytes()).unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].0, "user@example.com");
    assert_eq!(subs[0].1.origin, "https://fcm.googleapis.com");

    // A fourth bad entry abandons the file
    let file = serde_json::json!({"a": sub, "bad1": bad, "bad2": {}, "bad3": "x", "bad4": []});
    assert!(parse_subscriptions(file.to_string().as_bytes()).is_none());

    // Not an object, or invalid keys
    assert!(parse_subscriptions(b"[]").is_none());
    assert!(parse_subscriptions(b"{").is_none());
    let starter = serde_json::json!({"endpoint": "https://fcm.googleapis.com/fcm/send/abc123...", "keys": {"p256dh": "<place public key in base64 here>", "auth": "<place auth in base64 here>"}});
    assert!(parse_subscriptions(starter.to_string().as_bytes()).is_none());
    let off_curve = serde_json::json!({"endpoint": "https://push.example.com/a", "keys": {"p256dh": b64url(&[4u8; 65]), "auth": "AAAA"}});
    assert!(parse_subscriptions(off_curve.to_string().as_bytes()).is_none());
  }

  /// Captures every request the mock server receives.
  #[derive(Clone, Default)]
  struct Capture(Arc<StdMutex<Vec<Request>>>);

  impl wiremock::Respond for Capture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
      self.0.lock().unwrap().push(request.clone());
      ResponseTemplate::new(201)
    }
  }

  #[tokio::test]
  async fn test_send_encrypted_and_signed() {
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let b = browser();
    let subfile = write(
      &dir,
      "subscriptions.json",
      &serde_json::json!({ "user@example.com": subscription_json("https://fcm.googleapis.com/fcm/send/abc123", &b) }).to_string(),
    );

    let server = MockServer::builder().start().await;
    let capture = Capture::default();
    Mock::given(method("POST")).and(path("/fcm/send/abc123")).respond_with(capture.clone()).expect(1).mount(&server).await;

    let svc = from_url(&format!("vapid://user@example.com?keyfile={}&subfile={}&ttl=30", keyfile, subfile)).unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 4000);
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let request = capture.0.lock().unwrap().pop().unwrap();
    let h = |name: &str| request.headers.get(name).unwrap().to_str().unwrap().to_string();
    assert_eq!(h("ttl"), "30");
    assert_eq!(h("content-encoding"), "aes128gcm");
    assert_eq!(h("content-type"), "application/octet-stream");

    // Authorization: vapid t=<jwt for the endpoint origin>, k=<our public key>
    let auth = h("authorization");
    let (token, k) = auth.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
    let key = load_private_key(SEC1_KEY.as_bytes()).unwrap();
    assert_eq!(k, b64url(key.public_key().as_ref()));
    let parts: Vec<&str> = token.split('.').collect();
    let claims: Value = serde_json::from_slice(&b64_decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], "https://fcm.googleapis.com");
    assert_eq!(claims["sub"], "mailto:user@example.com");
    let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, b64_decode(k).unwrap());
    verifier.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64_decode(parts[2]).unwrap()).unwrap();

    // The body is the encrypted message, not plaintext
    assert_ne!(request.body, b"hello");
    assert_eq!(decrypt(b, &request.body), b"hello");
  }

  #[tokio::test]
  async fn test_send_requires_key_and_subscriptions() {
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let b = browser();
    let subfile = write(&dir, "subscriptions.json", &subscription_json("https://push.example.com/x", &b).to_string());
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };

    // No key file, and no persistent storage to look in
    let svc = from_url(&format!("vapid://user@example.com?subfile={}", subfile)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
    // An unreadable key file
    let svc = from_url(&format!("vapid://user@example.com?keyfile=invalid&subfile={}", subfile)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
    // No subscription file
    let svc = from_url(&format!("vapid://user@example.com?keyfile={}", keyfile)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
    // An unreadable subscription file is not read again
    let svc = from_url(&format!("vapid://user@example.com?keyfile={}&subfile=invalid", keyfile)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_single_entry_and_unknown_target() {
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let b = browser();
    // A single entry file is named after the endpoint's last path entry
    let subfile = write(&dir, "subscriptions.json", &subscription_json("https://push.example.com/send/abc123", &b).to_string());

    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/send/abc123")).and(header("ttl", "0")).respond_with(ResponseTemplate::new(204)).expect(2).mount(&server).await;

    // One known and one unknown target: delivered, but reported as an error
    let svc = vapid(&format!("vapid://user@example.com/ABC123/unknown?keyfile={}&subfile={}", keyfile, subfile));
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(svc.state.lock().await.targets, vec!["ABC123"]);
    // The unknown target was dropped, so the next send succeeds
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500, 999, 400] {
      let dir = tempfile::tempdir().unwrap();
      let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
      let b = browser();
      let subfile = write(&dir, "subscriptions.json", &serde_json::json!({ "user1": subscription_json("https://push.example.com/a", &b) }).to_string());
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/a")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("vapid://user@example.com/user1?keyfile={}&subfile={}", keyfile, subfile)).unwrap();
      let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_expired_subscription_is_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let (b1, b2) = (browser(), browser());
    let content = serde_json::json!({
      "gone": subscription_json("https://push.example.com/gone", &b1),
      "ok": subscription_json("https://push.example.com/ok", &b2),
      "broken": {"endpoint": "invalid"},
    });
    let subfile = write(&dir, "subscriptions.json", &serde_json::to_string_pretty(&content).unwrap());

    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/gone")).respond_with(ResponseTemplate::new(410)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/ok")).respond_with(ResponseTemplate::new(201)).expect(2).mount(&server).await;

    let svc = vapid(&format!("vapid://user@example.com/gone/ok?keyfile={}&subfile={}", keyfile, subfile));
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Only the expired entry left the file; the broken one is kept
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&subfile).unwrap()).unwrap();
    let names: Vec<&String> = after.as_object().unwrap().keys().collect();
    assert_eq!(names, vec!["broken", "ok"]);
    assert_eq!(after["ok"], content["ok"]);

    // The expired target is now unknown and gets dropped
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn test_expired_subscription_in_read_only_file_is_retired() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let (b1, b2) = (browser(), browser());
    let content = serde_json::json!({
      "gone": subscription_json("https://push.example.com/gone", &b1),
      "ok": subscription_json("https://push.example.com/ok", &b2),
    });
    let subfile = write(&dir, "subscriptions.json", &content.to_string());
    std::fs::set_permissions(&subfile, std::fs::Permissions::from_mode(0o400)).unwrap();
    if is_writable(Path::new(&subfile)) {
      // Running as root; permissions are not enforced
      return;
    }

    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/gone")).respond_with(ResponseTemplate::new(404)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/ok")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;

    let svc = vapid(&format!("vapid://user@example.com/gone/ok?keyfile={}&subfile={}", keyfile, subfile));
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(std::fs::read_to_string(&subfile).unwrap(), content.to_string());
    assert!(svc.state.lock().await.retired.contains("https://push.example.com/gone"));
  }

  #[tokio::test]
  async fn test_storage_holds_keys_and_subscriptions() {
    let tmp = tempfile::tempdir().unwrap();
    let asset = crate::AppriseAsset { storage_path: Some(tmp.path().to_path_buf()), ..Default::default() };
    let mut a = crate::Apprise::with_asset(asset);
    assert!(a.add("vapid://user@example.com"));
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    // The first run generates a key pair and a starter subscription file,
    // which holds no usable subscription yet
    assert!(!a.notify(&ctx).await.is_success());
    let id = a.url_id(a.services().next().unwrap()).unwrap();
    let dir = tmp.path().join(&id);
    let private = std::fs::read(dir.join("private_key.pem")).unwrap();
    assert!(String::from_utf8_lossy(&private).starts_with("-----BEGIN PRIVATE KEY-----\n"));
    assert!(load_private_key(&private).is_some());
    let public = std::fs::read_to_string(dir.join("public_key.pem")).unwrap();
    assert!(public.starts_with("-----BEGIN PUBLIC KEY-----\n"));
    let template: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(DEFAULT_SUBFILE)).unwrap()).unwrap();
    assert!(template.get("user@example.com").is_some());

    // Once the user fills it in, the stored key signs the message
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/fcm/send/abc123")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let b = browser();
    let endpoint = "https://fcm.googleapis.com/fcm/send/abc123";
    std::fs::write(dir.join(DEFAULT_SUBFILE), serde_json::json!({ "user@example.com": subscription_json(endpoint, &b) }).to_string()).unwrap();
    a.clear();
    assert!(a.add("vapid://user@example.com"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    // The key was not replaced
    assert_eq!(std::fs::read(dir.join("private_key.pem")).unwrap(), private);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let dir = tempfile::tempdir().unwrap();
    let keyfile = write(&dir, "private_key.pem", SEC1_KEY);
    let (b1, b2) = (browser(), browser());
    let content = serde_json::json!({
      "one": subscription_json("https://push.example.com/one", &b1),
      "two": subscription_json("https://push.example.com/two", &b2),
    });
    let subfile = write(&dir, "subscriptions.json", &content.to_string());
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/one")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The second subscription fails once; the retry only sends to it again.
    Mock::given(method("POST")).and(path("/two")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/two")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("vapid://user@example.com/one/two?keyfile={}&subfile={}&retry=1&wait=0", keyfile, subfile)));
    let ctx = NotifyContext { body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
  }
}

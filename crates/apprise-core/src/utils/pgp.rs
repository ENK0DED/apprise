// BSD 2-Clause License
//
// Apprise - Push Notification Library.
// Copyright (c) 2026, Chris Caron <lead2gold@gmail.com>
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! OpenPGP keys, signing and encryption (upstream `apprise/utils/pgp.py`).
//!
//! [`PgpController`] finds keys the way upstream's `ApprisePGPController`
//! does: an explicit key source (`pgppub=`, `pgpprv=`: a path or URL), else
//! a file named after the address in the service's storage directory, else
//! a Web Key Directory lookup, else (when allowed) a freshly generated key
//! pair. It signs detached (RFC 3156 `multipart/signed`), encrypts
//! (SEIPD v1, compressed literal data, as PGPy does) and builds Autocrypt
//! headers. The OpenPGP work is done by rPGP.
//!
//! rPGP's readers and writers are generic, and every reader or writer type
//! adds another copy of its parsers or serializers to the binary. So all
//! OpenPGP data is parsed from a byte slice (armor is decoded first) and
//! serialized into a `Vec<u8>` (armor is added afterwards).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use pgp::armor::{BlockType, Dearmor};
use pgp::composed::{
  DetachedSignature, EncryptionCaps, KeyType, MessageBuilder, PublicOrSecret, SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey, SubkeyParamsBuilder,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::packet::{Signature, SignatureType};
use pgp::ser::Serialize;
use pgp::types::{CompressionAlgorithm, KeyDetails, Password};
use sha1::{Digest, Sha1};

use crate::asset::AppriseAsset;
use crate::utils::cwe312::cwe312_loggable;
use crate::utils::wkd::WkdController;

/// Largest explicit public key accepted (upstream `max_pgp_public_key_size`).
pub const MAX_PGP_PUBLIC_KEY_SIZE: usize = 8000;

/// Largest explicit private key accepted; private keys may carry extra
/// identities and subkeys (upstream `max_pgp_private_key_size`).
pub const MAX_PGP_PRIVATE_KEY_SIZE: usize = 32000;

/// Largest complete Autocrypt header (Autocrypt Level 1: 10 KiB).
pub const MAX_AUTOCRYPT_HEADER_SIZE: usize = 10 * 1024;

/// Autocrypt `keydata` is folded with whitespace every 76 characters.
const AUTOCRYPT_FOLD_WIDTH: usize = 76;

/// How long a loaded key is reused.
const KEY_CACHE_EXPIRY: Duration = Duration::from_secs(86400);

/// Size of generated RSA keys (upstream generates RSA 2048).
const KEYGEN_RSA_BITS: u32 = 2048;

#[derive(Clone)]
enum CachedKey {
  Public(Box<SignedPublicKey>),
  Secret(Box<SignedSecretKey>),
}

/// Where a key comes from.
enum KeySource {
  /// Key material, with the identifier it is cached under.
  Found(Vec<u8>, String),
  /// An explicit source that could not be read (upstream returns `False`).
  Inaccessible,
  /// Nothing configured or discovered (upstream returns `None`).
  Missing,
}

/// Finds and uses PGP keys for one service (upstream
/// `ApprisePGPController`). Clones share the key cache.
#[derive(Clone)]
pub struct PgpController {
  /// The service's storage directory: where keys are discovered and
  /// generated. `None` without persistent storage.
  path: Option<PathBuf>,
  pub_keyfile: Option<String>,
  prv_keyfile: Option<String>,
  email: Option<String>,
  app_id: String,
  wkd: Option<WkdController>,
  /// Generate a key pair when one is needed and missing (upstream
  /// `AppriseAsset.pgp_autogen`, on by default).
  autogen: bool,
  lookup: Arc<Mutex<HashMap<String, (CachedKey, Instant)>>>,
}

impl std::fmt::Debug for PgpController {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PgpController").field("email", &self.email).field("wkd", &self.wkd.is_some()).finish_non_exhaustive()
  }
}

impl PgpController {
  /// A controller for the sender `email`, with optional explicit key
  /// sources (paths or URLs) and WKD discovery.
  pub fn new(pub_keyfile: Option<&str>, prv_keyfile: Option<&str>, email: Option<&str>, app_id: &str, wkd: Option<WkdController>) -> Self {
    let nonempty = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
    Self {
      path: None,
      pub_keyfile: nonempty(pub_keyfile),
      prv_keyfile: nonempty(prv_keyfile),
      email: nonempty(email),
      app_id: app_id.to_string(),
      wkd,
      autogen: true,
      lookup: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// This controller working in `path` (the service's storage directory).
  pub fn with_path(&self, path: Option<PathBuf>) -> Self {
    Self { path, ..self.clone() }
  }

  /// This controller for an Apprise using `asset` (upstream gives the
  /// controller the service's asset): generated keys are named after its
  /// `app_id`, and are only generated when its `pgp_autogen` allows it.
  pub fn with_asset(&self, asset: &AppriseAsset) -> Self {
    Self { app_id: asset.app_id.clone(), autogen: asset.pgp_autogen, ..self.clone() }
  }

  /// The WKD controller, if WKD discovery is on.
  pub fn wkd(&self) -> Option<&WkdController> {
    self.wkd.as_ref()
  }

  /// Replace the WKD controller.
  pub fn set_wkd(&mut self, wkd: Option<WkdController>) {
    self.wkd = wkd;
  }

  /// Turn key generation on or off (upstream `AppriseAsset.pgp_autogen`).
  pub fn set_autogen(&mut self, autogen: bool) {
    self.autogen = autogen;
  }

  fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, (CachedKey, Instant)>> {
    self.lookup.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Public key file names to look for, best first (upstream
  /// `_pub_key_candidates`). The sender's address is used only when no
  /// recipient address is given.
  pub fn pub_key_candidates(&self, emails: &[&str]) -> Vec<String> {
    let addresses: Vec<&str> = if emails.is_empty() { self.email.as_deref().into_iter().collect() } else { emails.to_vec() };
    let mut names: Vec<String> = ["pgp-public.asc", "pgp-pub.asc", "public.asc", "pub.asc"].iter().map(|s| s.to_string()).collect();
    for email in addresses {
      let local = email.split('@').next().unwrap_or_default().to_lowercase();
      names.insert(0, format!("{}-pub.asc", local));
      names.insert(0, format!("{}-pub.asc", email.to_lowercase()));
    }
    names
  }

  /// Private key file names to look for, best first (upstream
  /// `_prv_key_candidates`).
  pub fn prv_key_candidates(&self) -> Vec<String> {
    let mut names: Vec<String> = ["pgp-private.asc", "pgp-prv.asc", "private.asc", "prv.asc"].iter().map(|s| s.to_string()).collect();
    if let Some(email) = &self.email {
      names.insert(0, format!("{}-prv.asc", email.split('@').next().unwrap_or_default().to_lowercase()));
      names.insert(0, format!("{}-prv.asc", email.to_lowercase()));
    }
    names
  }

  fn find_file(&self, names: &[String]) -> Option<PathBuf> {
    let dir = self.path.as_deref()?;
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
  }

  /// The first discovered public key file for `emails` (upstream
  /// `public_keyfile` without an explicit `pgppub=`).
  pub fn public_keyfile(&self, emails: &[&str]) -> Option<PathBuf> {
    self.find_file(&self.pub_key_candidates(emails))
  }

  /// The first discovered private key file (upstream `private_keyfile`
  /// without an explicit `pgpprv=`).
  pub fn private_keyfile(&self) -> Option<PathBuf> {
    self.find_file(&self.prv_key_candidates())
  }

  async fn source(&self, explicit: Option<&str>, max: usize, discovered: Option<PathBuf>, prefix: &str) -> KeySource {
    if let Some(source) = explicit {
      return match crate::attachment::load_attachment(source).await {
        Ok(att) if att.data.len() <= max => KeySource::Found(att.data, format!("{}{}", prefix, source)),
        Ok(_) => {
          tracing::warn!("PGP key {} exceeds the {} byte limit", cwe312_loggable(source, true), max);
          KeySource::Inaccessible
        }
        Err(_) => {
          tracing::error!("Could not access PGP key {}.", cwe312_loggable(source, true));
          KeySource::Inaccessible
        }
      };
    }
    let Some(path) = discovered else { return KeySource::Missing };
    match std::fs::read(&path) {
      Ok(data) => KeySource::Found(data, format!("{}{}", prefix, absolute(&path).display())),
      Err(e) => {
        tracing::warn!("Error accessing PGP key file {}", cwe312_loggable(&path.to_string_lossy(), true));
        tracing::debug!("I/O Exception: {}", e);
        KeySource::Missing
      }
    }
  }

  fn searched(&self, candidates: &[String]) -> String {
    let ns = self.path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let shown: Vec<String> = candidates.iter().take(4).map(|c| format!("'{}/{}'", ns, c)).collect();
    format!("{}{}", shown.join(", "), if candidates.len() > 4 { ", ..." } else { "" })
  }

  /// The sender's private key (upstream `private_key`). `None` when it is
  /// missing, unreadable, passphrase-protected or public only.
  pub async fn private_key(&self) -> Option<SignedSecretKey> {
    let (data, id) = match self.source(self.prv_keyfile.as_deref(), MAX_PGP_PRIVATE_KEY_SIZE, self.private_keyfile(), "prv:").await {
      KeySource::Found(data, id) => (data, id),
      KeySource::Inaccessible => {
        tracing::warn!("PGP Private Key could not be accessed");
        return None;
      }
      KeySource::Missing => {
        if self.path.is_some() {
          tracing::debug!("No PGP private key found; searched: {}", self.searched(&self.prv_key_candidates()));
        } else {
          tracing::debug!("No PGP private key found");
        }
        return None;
      }
    };
    let cache_key = hex::encode(Sha1::digest(id.as_bytes()));
    let cached = self.cache().get(&cache_key).cloned();
    if let Some((CachedKey::Secret(key), expires)) = cached {
      if expires > Instant::now() {
        return Some(*key);
      }
      self.cache().remove(&cache_key);
    }
    let key = match parse_key(&data) {
      Some(PublicOrSecret::Secret(key)) => key,
      Some(PublicOrSecret::Public(_)) => {
        tracing::warn!("PGP Private Key file does not contain secret key material");
        return None;
      }
      None => {
        tracing::warn!("PGP Private Key file could not be parsed");
        return None;
      }
    };
    if key.primary_key.secret_params().is_encrypted() {
      // Apprise runs unattended, so a protected key would block sending
      tracing::warn!("PGP Private Key is passphrase-protected; Apprise does not support passphrase-protected keys");
      return None;
    }
    self.cache().insert(cache_key, (CachedKey::Secret(Box::new(key.clone())), Instant::now() + KEY_CACHE_EXPIRY));
    Some(key)
  }

  /// A detached signature over `message` and its `micalg` label (for
  /// example `pgp-sha256`), or `None` when signing is not possible.
  pub async fn sign(&self, message: &[u8]) -> Option<(String, String)> {
    let Some(key) = self.private_key().await else {
      tracing::debug!("PGP signing skipped: no usable private key");
      return None;
    };
    off_executor(message, move |message| sign_with(&key, &message)).await
  }

  /// The first key WKD publishes for `emails` (or the sender when none is
  /// given), so a missed recipient lookup never encrypts to the sender.
  async fn fetch_wkd_key(&self, emails: &[&str]) -> Option<SignedPublicKey> {
    let wkd = self.wkd.as_ref()?;
    let candidates: Vec<&str> = if emails.is_empty() { self.email.as_deref().into_iter().collect() } else { emails.to_vec() };
    for email in candidates.into_iter().filter(|e| !e.is_empty()) {
      let cache_key = hex::encode(Sha1::digest(format!("wkd:{}", email.to_lowercase()).as_bytes()));
      let cached = self.cache().get(&cache_key).cloned();
      if let Some((CachedKey::Public(key), expires)) = cached {
        if expires > Instant::now() {
          return Some(*key);
        }
        self.cache().remove(&cache_key);
      }
      let Some(data) = wkd.fetch(email).await else { continue };
      let Some(PublicOrSecret::Public(key)) = parse_key(&data) else {
        tracing::debug!("WKD key parse failed for {}; skipping", email);
        continue;
      };
      self.cache().insert(cache_key, (CachedKey::Public(Box::new(key.clone())), Instant::now() + KEY_CACHE_EXPIRY));
      tracing::debug!("Loaded PGP public key via WKD for {}", email);
      return Some(key);
    }
    None
  }

  /// The public key to encrypt to (upstream `public_key`): explicit or
  /// discovered file, then WKD, then a generated pair when `autogen` (or,
  /// when `None`, the controller's setting) allows it.
  pub async fn public_key(&self, emails: &[&str], autogen: Option<bool>) -> Option<SignedPublicKey> {
    let mut generated = false;
    loop {
      let source = self.source(self.pub_keyfile.as_deref(), MAX_PGP_PUBLIC_KEY_SIZE, self.public_keyfile(emails), "").await;
      let (data, id) = match source {
        KeySource::Found(data, id) => (data, id),
        KeySource::Inaccessible => return None,
        KeySource::Missing => {
          if let Some(key) = self.fetch_wkd_key(emails).await {
            return Some(key);
          }
          if !generated && autogen.unwrap_or(self.autogen) && self.keygen(emails.first().copied(), None, false).await {
            // Load what was just written
            generated = true;
            continue;
          }
          if self.path.is_some() {
            tracing::debug!("No PGP public key found; searched: {}", self.searched(&self.pub_key_candidates(emails)));
          } else {
            tracing::debug!("No PGP public key found");
          }
          return None;
        }
      };
      let cache_key = hex::encode(Sha1::digest(id.as_bytes()));
      let cached = self.cache().get(&cache_key).cloned();
      if let Some((CachedKey::Public(key), _)) = cached {
        return Some(*key);
      }
      let key = match parse_key(&data) {
        Some(PublicOrSecret::Public(key)) => key,
        Some(PublicOrSecret::Secret(key)) => key.to_public_key(),
        None => {
          tracing::warn!("PGP Public Key file could not be parsed");
          return None;
        }
      };
      self.cache().insert(cache_key, (CachedKey::Public(Box::new(key.clone())), Instant::now() + KEY_CACHE_EXPIRY));
      return Some(key);
    }
  }

  /// Generate a key pair for `email` (default: the sender) in the storage
  /// directory: an RSA signing and certification primary key with an RSA
  /// encryption subkey, as upstream's `keygen` does. True when the pair
  /// exists afterwards.
  pub async fn keygen(&self, email: Option<&str>, name: Option<&str>, force: bool) -> bool {
    if self.pub_keyfile.is_some() || self.path.is_none() {
      tracing::trace!("PGP keygen disabled, reason={}", if self.pub_keyfile.is_some() { "keyfile-defined" } else { "no-write-path" });
      return false;
    }
    let Some(email) = email.or(self.email.as_deref()).map(str::to_string) else { return false };
    let dir = self.path.clone().expect("checked above");
    let prefix = email.split('@').next().unwrap_or_default().to_lowercase();
    let pub_path = dir.join(format!("{}-pub.asc", prefix));
    let prv_path = dir.join(format!("{}-prv.asc", prefix));
    if pub_path.is_file() && !force {
      tracing::debug!("PGP generation skipped; Public Key already exists: {}", pub_path.display());
      return true;
    }
    self.cache().remove(&hex::encode(Sha1::digest(absolute(&pub_path).display().to_string().as_bytes())));

    let uid = format!("{} <{}>", name.unwrap_or(&self.app_id), email);
    let generated = tokio::task::spawn_blocking(move || generate_key(&uid)).await.ok().flatten();
    let Some((public, secret)) = generated else {
      tracing::warn!("PGP key generation failed");
      return false;
    };
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&pub_path, public)) {
      tracing::warn!("Error writing PGP file {}", pub_path.display());
      tracing::debug!("I/O Exception: {}", e);
      let _ = std::fs::remove_file(&pub_path);
      return false;
    }
    if let Err(e) = write_private(&prv_path, secret.as_bytes()) {
      tracing::warn!("Error writing PGP file {}", prv_path.display());
      tracing::debug!("I/O Exception: {}", e);
      let _ = std::fs::remove_file(&pub_path);
      let _ = std::fs::remove_file(&prv_path);
      return false;
    }
    tracing::info!("Wrote PGP Keys for {}/{}", dir.display(), pub_path.file_name().unwrap_or_default().to_string_lossy());
    true
  }

  /// An Autocrypt header advertising the sender's key (Autocrypt Level 1):
  /// only a key we hold the private half of, with exactly one user ID and
  /// one encryption subkey, and only when the header fits the 10 KiB limit.
  pub async fn autocrypt_header(&self) -> Option<String> {
    let email = self.email.as_deref()?;
    let mut key = self.private_key().await;
    if key.is_none() && self.autogen && self.keygen(None, None, false).await {
      key = self.private_key().await;
    }
    let key = key?;
    // Validate what is advertised: the exported public key, parsed again
    let exported = key.to_public_key().to_bytes().ok()?;
    let Some(PublicOrSecret::Public(reparsed)) = parse_key(&exported) else {
      tracing::debug!("Unable to re-parse our own exported PGP key");
      return None;
    };
    if !has_encryption_subkey(&reparsed) {
      tracing::debug!("PGP private key lacks an Autocrypt-compatible encryption subkey; omitting Autocrypt header");
      return None;
    }
    let keydata = base64::engine::general_purpose::STANDARD.encode(&exported);
    let folded: Vec<&str> = keydata.as_bytes().chunks(AUTOCRYPT_FOLD_WIDTH).map(|c| std::str::from_utf8(c).expect("base64 is ASCII")).collect();
    let header = format!("addr={}; prefer-encrypt=mutual; keydata={}", email, folded.join("\r\n "));
    let size = format!("Autocrypt: {}", header).len();
    if size > MAX_AUTOCRYPT_HEADER_SIZE {
      tracing::debug!("Autocrypt header ({} bytes) exceeds the {} byte spec limit; omitting", size, MAX_AUTOCRYPT_HEADER_SIZE);
      return None;
    }
    Some(header)
  }

  /// `message` encrypted to the key of `emails`, ASCII-armored, or `None`
  /// when no key is available. `autogen: Some(false)` uses existing keys
  /// only.
  pub async fn encrypt(&self, message: &[u8], emails: &[&str], autogen: Option<bool>) -> Option<String> {
    let key = self.public_key(emails, autogen).await?;
    off_executor(message, move |message| encrypt_to(&key, message)).await
  }

  /// Drop expired keys (and expired WKD results).
  pub fn prune(&self) {
    let now = Instant::now();
    self.cache().retain(|_, (_, expires)| *expires > now);
    if let Some(wkd) = &self.wkd {
      wkd.prune();
    }
  }
}

/// Run `work` on a copy of `message` in Tokio's blocking pool: signing,
/// compressing and encrypting a large message takes long enough to stall
/// other services and their deadlines on the executor (upstream sends each
/// service from a worker thread). A caller that stops waiting leaves the
/// work to finish unobserved.
async fn off_executor<T: Send + 'static>(message: &[u8], work: impl FnOnce(Vec<u8>) -> Option<T> + Send + 'static) -> Option<T> {
  let message = message.to_vec();
  match tokio::task::spawn_blocking(move || work(message)).await {
    Ok(result) => result,
    Err(e) => {
      tracing::debug!("PGP operation failed: {}", e);
      None
    }
  }
}

/// A detached signature over `message` with the key's signing component.
fn sign_with(key: &SignedSecretKey, message: &[u8]) -> Option<(String, String)> {
  let hash = preferred_hash(key);
  let rng = rand_core06::OsRng;
  let signature = if may_sign(primary_binding(&key.details), key.primary_key.algorithm()) {
    DetachedSignature::sign_binary_data(rng, &key.primary_key, &Password::empty(), hash, message)
  } else if let Some(sub) = key.secret_subkeys.iter().find(|s| latest_binding(&s.signatures).is_some_and(|b| b.key_flags().sign())) {
    DetachedSignature::sign_binary_data(rng, &sub.key, &Password::empty(), hash, message)
  } else {
    tracing::debug!("PGP signing failed; key may lack Sign capability");
    return None;
  };
  match signature.and_then(|s| s.to_bytes()).and_then(|packets| armored(BlockType::Signature, &packets)) {
    Ok(armored) => Some((armored, format!("pgp-{}", hash_name(hash)))),
    Err(e) => {
      tracing::debug!("PGP signing failed: {}", e);
      None
    }
  }
}

fn absolute(path: &Path) -> PathBuf {
  std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Write a private key readable by its owner only.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
  #[cfg(unix)]
  {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(data)
  }
  #[cfg(not(unix))]
  {
    std::fs::write(path, data)
  }
}

/// The first key in armored or binary OpenPGP data (PGPy `from_blob`).
fn parse_key(data: &[u8]) -> Option<PublicOrSecret> {
  // rPGP's own test: binary OpenPGP data starts with a byte whose high bit
  // is set
  let dearmored;
  let packets = if data.first()? & 0x80 != 0 {
    data
  } else {
    dearmored = dearmor_key(data)?;
    &dearmored
  };
  PublicOrSecret::from_bytes_many(packets).ok()?.next()?.ok()
}

/// The packets of the first ASCII-armored key block in `data`, decoded as
/// rPGP's `PublicOrSecret::from_armor_many_buf` does.
fn dearmor_key(data: &[u8]) -> Option<Vec<u8>> {
  let mut dearmor = Dearmor::new(data);
  dearmor.read_header().ok()?;
  if !matches!(dearmor.typ, Some(BlockType::PublicKey | BlockType::PrivateKey | BlockType::File)) {
    return None;
  }
  let mut packets = Vec::new();
  dearmor.read_to_end(&mut packets).ok()?;
  Some(packets)
}

/// Serialized packets passed through as they are, so that armoring them
/// reuses one writer.
struct Packets<'a>(&'a [u8]);

impl pgp::ser::Serialize for Packets<'_> {
  fn to_writer<W: std::io::Write>(&self, writer: &mut W) -> pgp::errors::Result<()> {
    writer.write_all(self.0)?;
    Ok(())
  }

  fn write_len(&self) -> usize {
    self.0.len()
  }
}

/// `packets` ASCII-armored as `typ`: what rPGP's `to_armored_string()`
/// writes with the default options (no headers, with a checksum).
fn armored(typ: BlockType, packets: &[u8]) -> pgp::errors::Result<String> {
  let mut out = Vec::new();
  pgp::armor::write(&Packets(packets), typ, &mut out, None, true)?;
  Ok(String::from_utf8(out).expect("ascii armor is utf8"))
}

/// A new key pair, armored (public, secret).
fn generate_key(uid: &str) -> Option<(String, String)> {
  let mut subkey = SubkeyParamsBuilder::default();
  subkey.key_type(KeyType::Rsa(KEYGEN_RSA_BITS)).can_sign(false).can_encrypt(EncryptionCaps::All);
  let mut params = SecretKeyParamsBuilder::default();
  params
    .key_type(KeyType::Rsa(KEYGEN_RSA_BITS))
    .can_certify(true)
    .can_sign(true)
    .can_encrypt(EncryptionCaps::None)
    .primary_user_id(uid.to_string())
    .preferred_symmetric_algorithms(vec![SymmetricKeyAlgorithm::AES256].into())
    .preferred_hash_algorithms(vec![HashAlgorithm::Sha256].into())
    .preferred_compression_algorithms(vec![CompressionAlgorithm::ZLIB].into())
    .subkeys(vec![subkey.build().ok()?]);
  let secret = params.build().ok()?.generate(rand_core06::OsRng).ok()?;
  let public = armored(BlockType::PublicKey, &secret.to_public_key().to_bytes().ok()?).ok()?;
  let secret = armored(BlockType::PrivateKey, &secret.to_bytes().ok()?).ok()?;
  Some((public, secret))
}

fn issued_by_primary(sig: &Signature, primary: &impl KeyDetails) -> bool {
  let id = primary.legacy_key_id();
  let fp = primary.fingerprint();
  sig.issuer_key_id().iter().any(|k| **k == id) || sig.issuer_fingerprint().iter().any(|f| **f == fp)
}

fn expired(sig: &Signature) -> bool {
  match (sig.created(), sig.signature_expiration_time()) {
    (Some(created), Some(expires)) if expires.as_secs() > 0 => {
      let until = std::time::UNIX_EPOCH + Duration::from_secs(u64::from(created.as_secs()) + u64::from(expires.as_secs()));
      until < std::time::SystemTime::now()
    }
    _ => false,
  }
}

/// The newest binding signature among `signatures`.
fn latest_binding(signatures: &[Signature]) -> Option<&Signature> {
  signatures.iter().filter(|s| s.typ() == Some(SignatureType::SubkeyBinding) && !expired(s)).max_by_key(|s| s.created().map(|c| c.as_secs()))
}

/// The self-signature carrying the primary key's preferences and flags:
/// the primary user ID's (or the first user ID's) newest certification.
fn primary_binding(details: &pgp::composed::SignedKeyDetails) -> Option<&Signature> {
  fn newest_certification(user: &pgp::types::SignedUser) -> Option<&Signature> {
    user.signatures.iter().filter(|s| s.is_certification() && !expired(s)).max_by_key(|s| s.created().map(|c| c.as_secs()))
  }
  details
    .users
    .iter()
    .find(|u| u.signatures.iter().any(|s| s.is_primary()))
    .or(details.users.first())
    .and_then(newest_certification)
    .or_else(|| details.direct_signatures.first())
}

/// Whether a binding carries no usage flags at all (old keys); the
/// algorithm's capabilities apply then.
fn no_flags(binding: Option<&Signature>) -> bool {
  binding
    .map(Signature::key_flags)
    .is_none_or(|f| !(f.certify() || f.sign() || f.encrypt_comms() || f.encrypt_storage() || f.authentication() || f.shared() || f.group()))
}

/// Whether a component may sign (PGPy's `KeyAction(KeyFlags.Sign)`).
fn may_sign(binding: Option<&Signature>, algorithm: pgp::crypto::public_key::PublicKeyAlgorithm) -> bool {
  if no_flags(binding) { algorithm.can_sign() } else { binding.is_some_and(|b| b.key_flags().sign()) }
}

/// Whether a component may encrypt (communications or storage).
fn may_encrypt(binding: Option<&Signature>, algorithm: pgp::crypto::public_key::PublicKeyAlgorithm) -> bool {
  if no_flags(binding) { algorithm.can_encrypt() } else { binding.is_some_and(|b| b.key_flags().encrypt_comms() || b.key_flags().encrypt_storage()) }
}

/// The signing hash: the key's first preferred SHA-2 hash, else SHA-256
/// (PGPy picks the first supported preference).
fn preferred_hash(key: &SignedSecretKey) -> HashAlgorithm {
  let prefs = primary_binding(&key.details).map(Signature::preferred_hash_algs).unwrap_or_default();
  prefs
    .iter()
    .copied()
    .find(|h| matches!(h, HashAlgorithm::Sha224 | HashAlgorithm::Sha256 | HashAlgorithm::Sha384 | HashAlgorithm::Sha512))
    .unwrap_or(HashAlgorithm::Sha256)
}

fn hash_name(hash: HashAlgorithm) -> &'static str {
  match hash {
    HashAlgorithm::Sha224 => "sha224",
    HashAlgorithm::Sha384 => "sha384",
    HashAlgorithm::Sha512 => "sha512",
    _ => "sha256",
  }
}

/// The cipher for `key`: its first preferred AES, Camellia or Twofish
/// cipher, else AES-256.
fn preferred_cipher(key: &SignedPublicKey) -> SymmetricKeyAlgorithm {
  use SymmetricKeyAlgorithm as S;
  let prefs = primary_binding(&key.details).map(Signature::preferred_symmetric_algs).unwrap_or_default();
  prefs
    .iter()
    .copied()
    .find(|c| matches!(c, S::AES128 | S::AES192 | S::AES256 | S::Camellia128 | S::Camellia192 | S::Camellia256 | S::Twofish))
    .unwrap_or(S::AES256)
}

/// Encrypt to the key's first encryption-capable subkey, else to the
/// primary key when it may encrypt (PGPy's `KeyAction` selection), as a
/// compressed binary literal in SEIPD v1.
fn encrypt_to(key: &SignedPublicKey, message: Vec<u8>) -> Option<String> {
  let mut rng = rand_core06::OsRng;
  let mut builder = MessageBuilder::from_bytes("", message).seipd_v1(&mut rng, preferred_cipher(key));
  builder.compression(CompressionAlgorithm::ZIP);
  let subkey = key
    .public_subkeys
    .iter()
    .filter(|s| !s.signatures.iter().any(|sig| sig.typ() == Some(SignatureType::SubkeyRevocation)))
    .find(|s| may_encrypt(latest_binding(&s.signatures), s.key.algorithm()));
  let result = match subkey {
    Some(sub) => builder.encrypt_to_key(&mut rng, &sub.key).map(|_| ()),
    None if may_encrypt(primary_binding(&key.details), key.primary_key.algorithm()) => builder.encrypt_to_key(&mut rng, &key.primary_key).map(|_| ()),
    None => {
      tracing::debug!("PGP Public Key has no encryption key; encryption not possible");
      return None;
    }
  };
  match result.and_then(|_| builder.to_vec(rng)).and_then(|packets| armored(BlockType::Message, &packets)) {
    Ok(armored) => Some(armored),
    Err(e) => {
      tracing::debug!("PGP Public Key Corruption; encryption not possible: {}", e);
      None
    }
  }
}

/// Whether `key` is shaped as Autocrypt expects (upstream
/// `_has_encryption_subkey`): no user attributes, no revocations, exactly
/// one user ID carrying exactly one signature (a self-signature), and
/// exactly one subkey with one live binding that allows encryption.
pub fn has_encryption_subkey(key: &SignedPublicKey) -> bool {
  let details = &key.details;
  if !details.user_attributes.is_empty() || !details.revocation_signatures.is_empty() {
    return false;
  }
  if details.users.len() != 1 || key.public_subkeys.len() != 1 {
    return false;
  }
  let user = &details.users[0];
  if user.signatures.len() != 1 || !issued_by_primary(&user.signatures[0], &key.primary_key) || !user.signatures[0].is_certification() {
    return false;
  }
  let subkey = &key.public_subkeys[0];
  if subkey.signatures.iter().any(|s| s.typ() == Some(SignatureType::SubkeyRevocation)) {
    return false;
  }
  let bindings: Vec<&Signature> =
    subkey.signatures.iter().filter(|s| s.typ() == Some(SignatureType::SubkeyBinding) && issued_by_primary(s, &key.primary_key) && !expired(s)).collect();
  if bindings.len() != 1 {
    return false;
  }
  let flags = bindings[0].key_flags();
  flags.encrypt_comms() || flags.encrypt_storage()
}

#[cfg(test)]
mod tests;

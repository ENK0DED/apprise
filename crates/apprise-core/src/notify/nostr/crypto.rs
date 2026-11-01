//! Signing and encryption used by the Nostr plugin (upstream
//! `apprise/plugins/nostr/crypto.py`).
//!
//! Events are signed with BIP-340 Schnorr signatures over secp256k1 (the
//! `k256` crate). Direct messages are encrypted the NIP-44 way (ChaCha20
//! with an HMAC-SHA256, inside NIP-59 seals and gift wraps) or the older
//! NIP-04 way (AES-256-CBC); files sent in NIP-17 messages use AES-256-GCM.
//!
//! - <https://github.com/nostr-protocol/nips/blob/master/01.md>
//! - <https://github.com/nostr-protocol/nips/blob/master/19.md>
//! - <https://github.com/nostr-protocol/nips/blob/master/44.md>
//! - <https://github.com/nostr-protocol/nips/blob/master/59.md>
//! - <https://github.com/bitcoin/bips/blob/master/bip-0340.mediawiki>

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit, Mac};
use k256::elliptic_curve::point::AffineCoordinates;
use k256::schnorr::{Signature, SigningKey, VerifyingKey};
use k256::{FieldBytes, ProjectivePoint, Scalar};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The characters of bech32 strings such as `npub1...` and `nsec1...`.
const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// Gift wrapped messages are back-dated by up to two days so their real send
/// time stays hidden.
const NOSTR_TIMESTAMP_JITTER: i64 = 2 * 24 * 60 * 60;

/// The NIP-44 version byte.
const NIP44_VERSION: u8 = 2;

/// A signed (or, without `sig`, unsigned) Nostr event. The fields are in
/// upstream's order, which is the order they are written in as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
  pub pubkey: String,
  pub created_at: i64,
  pub kind: i64,
  pub tags: Vec<Vec<String>>,
  pub content: String,
  pub id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sig: Option<String>,
}

impl Event {
  /// The event as JSON the way Python's `json.dumps(event,
  /// ensure_ascii=False)` writes it (`", "` and `": "` separators), which
  /// is how upstream seals messages and signs Blossom uploads.
  pub fn to_py_json(&self) -> String {
    py_dumps(self)
  }
}

/// Python's default `json.dumps()` layout: a space after every `,` and `:`.
struct PyFormatter;

impl serde_json::ser::Formatter for PyFormatter {
  fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
    if first { Ok(()) } else { writer.write_all(b", ") }
  }

  fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
    if first { Ok(()) } else { writer.write_all(b", ") }
  }

  fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
    writer.write_all(b": ")
  }
}

/// `value` as Python's `json.dumps(value, ensure_ascii=False)` writes it.
/// serde_json escapes strings exactly as Python does without
/// `ensure_ascii`: `"`, `\` and control characters only.
pub fn py_dumps<T: Serialize + ?Sized>(value: &T) -> String {
  let mut out = Vec::new();
  let mut ser = serde_json::Serializer::with_formatter(&mut out, PyFormatter);
  // Events and plain JSON values always serialize
  let _ = value.serialize(&mut ser);
  String::from_utf8(out).unwrap_or_default()
}

/// Whether `value` is 64 hexadecimal characters (upstream `IS_HEX_KEY`,
/// case-insensitive).
pub fn is_hex_key(value: &str) -> bool {
  value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `value` is a signature as it appears in an event: 128 lowercase
/// hexadecimal characters (upstream `IS_HEX_SIG`).
fn is_hex_sig(value: &str) -> bool {
  value.len() == 128 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn bech32_polymod(values: &[u8]) -> u32 {
  // The generator constants of the bech32 specification (BIP-173)
  const GENERATOR: [u32; 5] = [0x3B6A_57B2, 0x2650_8E6D, 0x1EA1_19FA, 0x3D42_33DD, 0x2A14_62B3];
  let mut chk: u32 = 1;
  for &value in values {
    let top = chk >> 25;
    chk = ((chk & 0x1FF_FFFF) << 5) ^ u32::from(value);
    for (i, g) in GENERATOR.iter().enumerate() {
      if (top >> i) & 1 == 1 {
        chk ^= g;
      }
    }
  }
  chk
}

/// The prefix (`npub`, `nsec`) in the form the checksum uses.
fn bech32_hrp_expand(hrp: &str) -> Vec<u8> {
  let bytes = hrp.as_bytes();
  let mut out: Vec<u8> = bytes.iter().map(|b| b >> 5).collect();
  out.push(0);
  out.extend(bytes.iter().map(|b| b & 31));
  out
}

/// Regroup `data` from `from_bits` to `to_bits` sized values; decoding
/// (`pad` off) refuses leftover bits.
fn convert_bits(data: &[u8], from_bits: u32, to_bits: u32, pad: bool) -> Option<Vec<u8>> {
  let mut acc: u64 = 0;
  let mut bits: u32 = 0;
  let mut result = Vec::new();
  let max_value: u64 = (1 << to_bits) - 1;
  for &value in data {
    acc = ((acc << from_bits) | u64::from(value)) & 0xFFFF_FFFF;
    bits += from_bits;
    while bits >= to_bits {
      bits -= to_bits;
      result.push(((acc >> bits) & max_value) as u8);
    }
  }
  if pad {
    if bits > 0 {
      result.push(((acc << (to_bits - bits)) & max_value) as u8);
    }
  } else if bits >= from_bits || ((acc << (to_bits - bits)) & max_value) != 0 {
    return None;
  }
  Some(result)
}

/// `data` as a bech32 string starting with `hrp` (NIP-19).
pub fn bech32_encode(hrp: &str, data: &[u8]) -> String {
  let values = convert_bits(data, 8, 5, true).unwrap_or_default();
  let mut check = bech32_hrp_expand(hrp);
  check.extend(&values);
  check.extend([0u8; 6]);
  let polymod = bech32_polymod(&check) ^ 1;
  let mut out = String::with_capacity(hrp.len() + 1 + values.len() + 6);
  out.push_str(hrp);
  out.push('1');
  out.extend(values.iter().map(|&v| BECH32_CHARSET[v as usize] as char));
  out.extend((0..6).map(|i| BECH32_CHARSET[((polymod >> (5 * (5 - i))) & 31) as usize] as char));
  out
}

/// The prefix and bytes held in a bech32 string, or `None`.
pub fn bech32_decode(value: &str) -> Option<(String, Vec<u8>)> {
  let value = value.to_lowercase();
  // The prefix is everything before the last "1"
  let pos = value.rfind('1')?;
  if pos < 1 || pos + 7 > value.len() {
    return None;
  }
  let (hrp, rest) = (&value[..pos], &value[pos + 1..]);
  let values: Vec<u8> = rest.bytes().map(|c| BECH32_CHARSET.iter().position(|&x| x == c).map(|p| p as u8)).collect::<Option<_>>()?;
  let mut check = bech32_hrp_expand(hrp);
  check.extend(&values);
  if bech32_polymod(&check) != 1 {
    return None;
  }
  let data = convert_bits(&values[..values.len() - 6], 5, 8, false)?;
  Some((hrp.to_string(), data))
}

/// A key in hex from 64 hex characters or a bech32 string with prefix `hrp`
/// (upstream `decode_key()`).
pub fn decode_key(value: &str, hrp: &str) -> Option<String> {
  // Python's str.strip() whitespace
  let value = value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
  if is_hex_key(value) {
    return Some(value.to_lowercase());
  }
  match bech32_decode(value) {
    Some((prefix, data)) if prefix == hrp && data.len() == 32 => Some(hex::encode(data)),
    _ => None,
  }
}

/// The 32 bytes of a 64 character hex key.
fn key_bytes(hex_key: &str) -> Option<[u8; 32]> {
  let mut out = [0u8; 32];
  hex::decode_to_slice(hex_key, &mut out).ok()?;
  Some(out)
}

/// The BIP-340 verifying key (the curve point with an even y) of an x-only
/// public key in hex, or `None` when no such point exists.
fn verifying_key(pubkey: &str) -> Option<VerifyingKey> {
  VerifyingKey::from_bytes(&FieldBytes::from(key_bytes(pubkey)?)).ok()
}

/// Whether `pubkey` (hex) is a usable point on the curve.
pub fn is_public_key(pubkey: &str) -> bool {
  verifying_key(pubkey).is_some()
}

/// The NIP-01 id of an event: the SHA-256 of the compact JSON array of its
/// fields.
pub fn event_id(pubkey: &str, created_at: i64, kind: i64, tags: &[Vec<String>], content: &str) -> String {
  let serialized = serde_json::to_string(&(0, pubkey, created_at, kind, tags, content)).unwrap_or_default();
  hex::encode(Sha256::digest(serialized.as_bytes()))
}

/// The event held in `value` when it has a valid id and signature (upstream
/// `verify_event()`). Every field must have the shape NIP-01 describes.
pub fn verify_event(value: &Value) -> Option<Event> {
  let obj = value.as_object()?;
  let text = |name: &str| obj.get(name).and_then(Value::as_str);
  // JSON booleans and floats are not integers
  let int = |name: &str| obj.get(name).and_then(Value::as_i64);
  let (id, pubkey, sig, content) = (text("id")?, text("pubkey")?, text("sig")?, text("content")?);
  if !is_hex_key(id) || !is_hex_key(pubkey) || !is_hex_sig(sig) {
    return None;
  }
  let (created_at, kind) = (int("created_at")?, int("kind")?);
  let tags: Vec<Vec<String>> = obj
    .get("tags")?
    .as_array()?
    .iter()
    .map(|tag| tag.as_array().and_then(|values| values.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()))
    .collect::<Option<_>>()?;

  // The id must match the content and the signature must be the author's
  let key = verifying_key(pubkey)?;
  if event_id(pubkey, created_at, kind, &tags, content) != id {
    return None;
  }
  let signature = Signature::from_slice(&hex::decode(sig).ok()?).ok()?;
  key.verify_raw(&key_bytes(id)?, &signature).ok()?;
  Some(Event { pubkey: pubkey.to_string(), created_at, kind, tags, content: content.to_string(), id: id.to_string(), sig: Some(sig.to_string()) })
}

/// The current time in seconds since the epoch.
fn now() -> i64 {
  chrono::Utc::now().timestamp()
}

/// An unsigned event with its id filled in (upstream `rumor()`).
pub fn rumor(pubkey: &str, kind: i64, content: &str, tags: Vec<Vec<String>>, created_at: Option<i64>) -> Event {
  let created_at = created_at.unwrap_or_else(now);
  let id = event_id(pubkey, created_at, kind, &tags, content);
  Event { pubkey: pubkey.to_string(), created_at, kind, tags, content: content.to_string(), id, sig: None }
}

/// A secp256k1 key pair used to sign Nostr events.
#[derive(Clone)]
pub struct NostrKey {
  signing: SigningKey,
  public_key: String,
}

impl std::fmt::Debug for NostrKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Never print the secret
    f.debug_struct("NostrKey").field("public_key", &self.public_key).finish_non_exhaustive()
  }
}

impl NostrKey {
  /// Load a secret key; `None` unless it is between 1 and the group order.
  pub fn from_secret(secret: &[u8; 32]) -> Option<Self> {
    let signing = SigningKey::from_bytes(&FieldBytes::from(*secret)).ok()?;
    let public_key = hex::encode(signing.verifying_key().to_bytes());
    Some(Self { signing, public_key })
  }

  /// A new random key, as used for each gift wrap envelope.
  pub fn generate() -> Self {
    loop {
      if let Some(key) = Self::from_secret(&rand::random()) {
        return key;
      }
    }
  }

  /// Our public key: the x coordinate of our point, in hex.
  pub fn public_key(&self) -> &str {
    &self.public_key
  }

  /// A new signed event, with fresh randomness for the nonce.
  pub fn sign(&self, kind: i64, content: &str, tags: Vec<Vec<String>>, created_at: Option<i64>) -> Event {
    self.sign_with_aux(kind, content, tags, created_at, &rand::random())
  }

  /// [`sign`](Self::sign) with the BIP-340 auxiliary randomness given.
  pub fn sign_with_aux(&self, kind: i64, content: &str, tags: Vec<Vec<String>>, created_at: Option<i64>, aux: &[u8; 32]) -> Event {
    let mut event = rumor(&self.public_key, kind, content, tags, created_at);
    let msg = key_bytes(&event.id).unwrap_or_default();
    // Only fails for a nonce or signature of zero, which no hash produces
    event.sig = self.signing.sign_raw(&msg, aux).ok().map(|sig| hex::encode(sig.to_bytes()));
    event
  }

  /// The ECDH shared x coordinate with `pubkey` (unhashed, as NIP-04 and
  /// NIP-44 use it).
  pub fn shared_x(&self, pubkey: &str) -> Option<[u8; 32]> {
    let point = ProjectivePoint::from(*verifying_key(pubkey)?.as_affine());
    let secret: Scalar = *self.signing.as_nonzero_scalar().as_ref();
    // The x coordinate is the same for our secret and its negation
    Some((point * secret).to_affine().x().into())
  }

  /// The NIP-44 conversation key shared with `pubkey`: an HKDF-extract with
  /// the salt `nip44-v2`.
  pub fn conversation_key(&self, pubkey: &str) -> Option<[u8; 32]> {
    Some(hmac_sha256(b"nip44-v2", &[&self.shared_x(pubkey)?]))
  }
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
  // HMAC takes keys of any length
  let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
  for part in parts {
    mac.update(part);
  }
  mac.finalize().into_bytes().into()
}

/// The NIP-44 padded size of a `length` byte message.
fn nip44_padded_len(length: usize) -> usize {
  if length <= 32 {
    return 32;
  }
  // The smallest power of two above length - 1 (Python's 1 << bit_length)
  let next_power = 1usize << (usize::BITS - (length - 1).leading_zeros());
  let chunk = if next_power <= 256 { 32 } else { next_power / 8 };
  chunk * ((length - 1) / chunk + 1)
}

/// This message's ChaCha20 key and nonce and HMAC key (an HKDF-expand of the
/// conversation key with the message nonce).
fn nip44_message_keys(conversation_key: &[u8; 32], nonce: &[u8; 32]) -> ([u8; 32], [u8; 12], [u8; 32]) {
  let mut keys = Vec::with_capacity(96);
  let mut block: Vec<u8> = Vec::new();
  for counter in 1u8..=3 {
    block = hmac_sha256(conversation_key, &[&block, nonce, &[counter]]).to_vec();
    keys.extend_from_slice(&block);
  }
  let mut chacha_key = [0u8; 32];
  let mut chacha_nonce = [0u8; 12];
  let mut hmac_key = [0u8; 32];
  chacha_key.copy_from_slice(&keys[..32]);
  chacha_nonce.copy_from_slice(&keys[32..44]);
  hmac_key.copy_from_slice(&keys[44..76]);
  (chacha_key, chacha_nonce, hmac_key)
}

fn chacha20(key: &[u8; 32], nonce: &[u8; 12], data: &mut [u8]) {
  use chacha20::cipher::{KeyIvInit, StreamCipher};
  // The IETF variant: a 4 byte block counter starting at zero
  chacha20::ChaCha20::new(key.into(), nonce.into()).apply_keystream(data);
}

/// `plaintext` encrypted with NIP-44 version 2, with a fresh random nonce
/// unless one is given.
pub fn nip44_encrypt(plaintext: &str, conversation_key: &[u8; 32], nonce: Option<[u8; 32]>) -> String {
  let nonce = nonce.unwrap_or_else(rand::random);
  let (chacha_key, chacha_nonce, hmac_key) = nip44_message_keys(conversation_key, &nonce);

  // Prefix the length and pad the message so its size is hidden; upstream
  // writes a longer prefix for messages NIP-44 itself does not allow
  let data = plaintext.as_bytes();
  let mut padded = match u16::try_from(data.len()) {
    Ok(len) => len.to_be_bytes().to_vec(),
    Err(_) => [0u8, 0].into_iter().chain((data.len() as u32).to_be_bytes()).collect(),
  };
  padded.extend_from_slice(data);
  padded.resize(padded.len() + nip44_padded_len(data.len()) - data.len(), 0);

  chacha20(&chacha_key, &chacha_nonce, &mut padded);
  let mac = hmac_sha256(&hmac_key, &[&nonce, &padded]);

  let mut payload = Vec::with_capacity(1 + 32 + padded.len() + 32);
  payload.push(NIP44_VERSION);
  payload.extend_from_slice(&nonce);
  payload.extend_from_slice(&padded);
  payload.extend_from_slice(&mac);
  B64.encode(payload)
}

/// Decrypt a NIP-44 version 2 payload (for tests, which read what was sent).
#[cfg(test)]
pub fn nip44_decrypt(payload: &str, conversation_key: &[u8; 32]) -> Option<String> {
  let raw = B64.decode(payload).ok()?;
  if raw.len() < 1 + 32 + 32 + 2 || raw[0] != NIP44_VERSION {
    return None;
  }
  let nonce: [u8; 32] = raw[1..33].try_into().ok()?;
  let (ciphertext, mac) = raw[33..].split_at(raw.len() - 33 - 32);
  let (chacha_key, chacha_nonce, hmac_key) = nip44_message_keys(conversation_key, &nonce);
  if hmac_sha256(&hmac_key, &[&nonce, ciphertext]) != mac {
    return None;
  }
  let mut padded = ciphertext.to_vec();
  chacha20(&chacha_key, &chacha_nonce, &mut padded);
  let size = usize::from(u16::from_be_bytes([padded[0], padded[1]]));
  let (start, size) = if size == 0 { (6, u32::from_be_bytes(padded.get(2..6)?.try_into().ok()?) as usize) } else { (2, size) };
  String::from_utf8(padded.get(start..start + size)?.to_vec()).ok()
}

/// `plaintext` encrypted the older NIP-04 way: AES-256-CBC keyed with the
/// shared x coordinate, with a fresh random IV unless one is given.
pub fn nip04_encrypt(plaintext: &str, shared_x: &[u8; 32], iv: Option<[u8; 16]>) -> String {
  use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
  let iv = iv.unwrap_or_else(rand::random);
  let ciphertext = cbc::Encryptor::<aes_gcm::aes::Aes256>::new(shared_x.into(), (&iv).into()).encrypt_padded_vec::<Pkcs7>(plaintext.as_bytes());
  // The IV travels alongside the message like a query string
  format!("{}?iv={}", B64.encode(ciphertext), B64.encode(iv))
}

/// Decrypt a NIP-04 payload (for tests, which read what was sent).
#[cfg(test)]
pub fn nip04_decrypt(payload: &str, shared_x: &[u8; 32]) -> Option<String> {
  use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
  let (ciphertext, iv) = payload.split_once("?iv=")?;
  let (ciphertext, iv) = (B64.decode(ciphertext).ok()?, B64.decode(iv).ok()?);
  let plain = cbc::Decryptor::<aes_gcm::aes::Aes256>::new_from_slices(shared_x, &iv).ok()?.decrypt_padded_vec::<Pkcs7>(&ciphertext).ok()?;
  String::from_utf8(plain).ok()
}

/// AES-256-GCM with the 16 byte nonce NIP-17 clients such as Amethyst use.
pub type FileCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, aes_gcm::aead::consts::U16>;

/// `data` encrypted with AES-256-GCM, and its key and nonce in hex (upstream
/// `encrypt_file()`). Each file gets its own random key and nonce unless
/// they are given.
pub fn encrypt_file(data: &[u8], secrets: Option<([u8; 32], [u8; 16])>) -> (Vec<u8>, String, String) {
  use aes_gcm::aead::{Aead, KeyInit};
  let (key, nonce) = secrets.unwrap_or_else(|| (rand::random(), rand::random()));
  let cipher = FileCipher::new(&key.into());
  // Encrypting in memory only fails for messages longer than GCM allows
  let encrypted = cipher.encrypt(&nonce.into(), data).unwrap_or_default();
  (encrypted, hex::encode(key), hex::encode(nonce))
}

/// `message` sealed (kind 13) and gift wrapped (kind 1059) for `pubkey`
/// (NIP-59). The seal proves who sent the message while hiding what it
/// says; a throwaway key wraps the seal so the sender stays hidden too.
pub fn gift_wrap(sender: &NostrKey, pubkey: &str, message: &Event) -> Option<Event> {
  let now = now();
  let seal = sender.sign(
    13,
    &nip44_encrypt(&message.to_py_json(), &sender.conversation_key(pubkey)?, None),
    Vec::new(),
    Some(now - rand::random_range(0..NOSTR_TIMESTAMP_JITTER)),
  );
  let wrapper = NostrKey::generate();
  Some(wrapper.sign(
    1059,
    &nip44_encrypt(&seal.to_py_json(), &wrapper.conversation_key(pubkey)?, None),
    vec![vec!["p".to_string(), pubkey.to_string()]],
    Some(now - rand::random_range(0..NOSTR_TIMESTAMP_JITTER)),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn key(byte: u8) -> NostrKey {
    NostrKey::from_secret(&[byte; 32]).unwrap()
  }

  fn vectors() -> Value {
    serde_json::from_str(include_str!("testdata/upstream_vectors.json")).unwrap()
  }

  /// Upstream `test_plugin_nostr_crypto`: signing and key helpers match their
  /// specifications.
  #[test]
  fn test_keys_and_bech32() {
    // A BIP-340 test vector (index 0)
    let mut three = [0u8; 32];
    three[31] = 3;
    assert_eq!(NostrKey::from_secret(&three).unwrap().public_key(), "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9");

    // Zero and the group order are not secret keys
    assert!(NostrKey::from_secret(&[0; 32]).is_none());
    assert!(NostrKey::from_secret(&key_bytes("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141").unwrap()).is_none());

    // A secret key whose public point has an odd y is still usable
    let odd = key(0x22);
    assert!(verify_event(&serde_json::to_value(odd.sign(1, "x", Vec::new(), None)).unwrap()).is_some());

    // A random key is made when none is given
    assert_ne!(NostrKey::generate().public_key(), NostrKey::generate().public_key());

    // The NIP-19 example key
    let hexkey = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
    let npub = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    assert_eq!(bech32_encode("npub", &hex::decode(hexkey).unwrap()), npub);
    assert_eq!(decode_key(npub, "npub").as_deref(), Some(hexkey));
    assert_eq!(decode_key(&npub.to_uppercase(), "npub").as_deref(), Some(hexkey));
    assert_eq!(decode_key(&hexkey.to_uppercase(), "nsec").as_deref(), Some(hexkey));
    assert_eq!(decode_key(&format!("  {npub}\n"), "npub").as_deref(), Some(hexkey));

    // Data that fits 5-bit groups exactly needs no padding
    assert_eq!(bech32_decode(&bech32_encode("x", &[0; 5])), Some(("x".to_string(), vec![0; 5])));

    // Things that are not keys
    assert_eq!(decode_key(npub, "nsec"), None);
    assert_eq!(decode_key(&format!("{}q", &npub[..npub.len() - 1]), "npub"), None);
    assert_eq!(decode_key("npub1b", "npub"), None);
    assert_eq!(decode_key(&format!("npub1{}!", "b".repeat(10)), "npub"), None);
    assert_eq!(decode_key("1qqqqqqqq", "npub"), None);
    assert_eq!(decode_key(&bech32_encode("npub", &[1]), "npub"), None);
    assert_eq!(decode_key("", "npub"), None);

    // Leftover bits are not allowed when decoding
    assert_eq!(convert_bits(&[31, 31], 5, 8, false), None);
    assert_eq!(convert_bits(&[1], 5, 8, false), None);
    let mut check = bech32_hrp_expand("a");
    check.extend([1, 0, 0, 0, 0, 0, 0]);
    let polymod = bech32_polymod(&check) ^ 1;
    let mut crafted = String::from("a1");
    crafted.push(BECH32_CHARSET[1] as char);
    crafted.extend((0..6).map(|i| BECH32_CHARSET[((polymod >> (5 * (5 - i))) & 31) as usize] as char));
    assert_eq!(bech32_decode(&crafted), None);

    // Points that are not on the curve
    assert!(!is_public_key("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f"));
    assert!(!is_public_key(&format!("{:064x}", 5)));
    assert!(!is_public_key(&"f".repeat(64)));
    assert!(is_public_key(key(0x11).public_key()));
  }

  /// Upstream `test_plugin_nostr_verify`: only well formed, correctly signed
  /// events are trusted.
  #[test]
  fn test_verify_event() {
    let sender = key(0x11);
    let event = serde_json::to_value(sender.sign(1, "hello", vec![vec!["t".into(), "x".into()]], None)).unwrap();
    assert!(verify_event(&event).is_some());
    let with = |changes: Value| {
      let mut e = event.clone();
      for (k, v) in changes.as_object().unwrap() {
        e[k] = v.clone();
      }
      verify_event(&e)
    };

    // Anything changed or missing breaks it
    let id = event["id"].as_str().unwrap();
    let sig = event["sig"].as_str().unwrap();
    for changes in [
      json!({"content": "bye"}),
      json!({"sig": "00"}),
      json!({"pubkey": "f".repeat(64)}),
      json!({"sig": "zz"}),
      // Fields of the wrong type are refused before anything else
      json!({"tags": 1}),
      json!({"tags": [1]}),
      json!({"tags": [["t", 1]]}),
      json!({"created_at": []}),
      json!({"created_at": true}),
      json!({"created_at": 1.0}),
      json!({"kind": "1"}),
      json!({"content": null}),
      json!({"id": format!("{}0", id.to_uppercase())}),
      json!({"id": id.to_uppercase()}),
      json!({"pubkey": 1}),
      json!({"sig": sig.to_uppercase()}),
    ] {
      assert!(with(changes.clone()).is_none(), "{changes}");
    }
    assert!(verify_event(&json!({"pubkey": event["pubkey"]})).is_none());
    assert!(verify_event(&Value::Null).is_none());
    assert!(verify_event(&json!([event])).is_none());

    // A signature from someone else
    let other = serde_json::to_value(key(0x33).sign(1, "hello", vec![vec!["t".into(), "x".into()]], event["created_at"].as_i64())).unwrap();
    assert!(with(json!({"sig": other["sig"]})).is_none());

    // Signature halves outside of their allowed range
    assert!(with(json!({"sig": format!("{}{}", "f".repeat(64), &sig[64..])})).is_none());
    assert!(with(json!({"sig": format!("{}{}", &sig[..64], "f".repeat(64))})).is_none());
  }

  /// Upstream `test_plugin_nostr_nip44`: NIP-44 encryption matches the
  /// official test vectors (paulmillr/nip44, nip44.vectors.json).
  #[test]
  fn test_nip44() {
    let mut one = [0u8; 32];
    one[31] = 1;
    let mut two = [0u8; 32];
    two[31] = 2;
    let (sec1, sec2) = (NostrKey::from_secret(&one).unwrap(), NostrKey::from_secret(&two).unwrap());
    let conversation_key = sec1.conversation_key(sec2.public_key()).unwrap();
    assert_eq!(hex::encode(conversation_key), "c41c775356fd92eadc63ff5a0dc1da211b268cbea22316767095b2871ea1412d");
    assert_eq!(
      nip44_encrypt("a", &conversation_key, Some(one)),
      "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABee0G5VSK0/9YypIObAtDKfYEAjD35uVkHyB0F4DwrcNaCXlCWZKaArsGrY6M9wnuTMxWfp1RTN9Xga8no+kF5Vsb"
    );

    // The two keys always agree on the conversation key
    assert_eq!(sec2.conversation_key(sec1.public_key()), Some(conversation_key));

    // Padding sizes (and the NIP-44 reference values around each step)
    for (len, padded) in [(1, 32), (32, 32), (33, 64), (64, 64), (65, 96), (256, 256), (257, 320), (320, 320), (321, 384), (65535, 65536), (65536, 65536)] {
      assert_eq!(nip44_padded_len(len), padded, "{len}");
    }

    // A round trip, and a payload whose MAC was tampered with
    let payload = nip44_encrypt("Hello é 😀", &conversation_key, None);
    assert_eq!(nip44_decrypt(&payload, &conversation_key).as_deref(), Some("Hello é 😀"));
    let mut raw = B64.decode(&payload).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 1;
    assert_eq!(nip44_decrypt(&B64.encode(raw), &conversation_key), None);

    // Very long messages use upstream's longer length prefix
    let payload = nip44_encrypt(&"x".repeat(70000), &conversation_key, None);
    let raw = B64.decode(&payload).unwrap();
    assert_eq!(raw.len(), 1 + 32 + 6 + nip44_padded_len(70000) + 32);
    assert_eq!(nip44_decrypt(&payload, &conversation_key), Some("x".repeat(70000)));
  }

  /// Events, ids and signatures match upstream's byte for byte, given the
  /// same key, time and BIP-340 auxiliary randomness.
  #[test]
  fn test_events_match_upstream() {
    let v = vectors();
    let keys = [("sender", key(0x11)), ("recipient", key(0x22)), ("other", key(0x33))];
    for (name, key) in &keys {
      let k = &v["keys"][name];
      assert_eq!(key.public_key(), k["pubkey"].as_str().unwrap());
      assert_eq!(bech32_encode("npub", &hex::decode(key.public_key()).unwrap()), k["npub"].as_str().unwrap());
      let secret = decode_key(k["nsec"].as_str().unwrap(), "nsec").unwrap();
      assert_eq!(NostrKey::from_secret(&key_bytes(&secret).unwrap()).unwrap().public_key(), key.public_key());
    }
    let aux: [u8; 32] = std::array::from_fn(|i| i as u8);
    for case in v["events"].as_array().unwrap() {
      let expected: Event = serde_json::from_value(case["event"].clone()).unwrap();
      let signer = &keys.iter().find(|(_, k)| k.public_key() == expected.pubkey).unwrap().1;
      let event = signer.sign_with_aux(expected.kind, &expected.content, expected.tags.clone(), Some(expected.created_at), &aux);
      assert_eq!(event, expected);
      // The relay frame and the JSON a seal or Blossom permission holds
      assert_eq!(serde_json::to_string(&("EVENT", &event)).unwrap(), case["compact"].as_str().unwrap());
      assert_eq!(event.to_py_json(), case["py_default"].as_str().unwrap());
      assert!(verify_event(&case["event"]).is_some());
    }
  }

  /// NIP-44, NIP-04 and file payloads match upstream's given the same
  /// randomness, and the port opens upstream's random ones.
  #[test]
  fn test_encryption_matches_upstream() {
    let v = vectors();
    let (sender, recipient) = (key(0x11), key(0x22));
    let conversation_key = sender.conversation_key(recipient.public_key()).unwrap();
    assert_eq!(hex::encode(conversation_key), v["conversation_key"].as_str().unwrap());
    assert_eq!(hex::encode(sender.shared_x(recipient.public_key()).unwrap()), v["shared_x"].as_str().unwrap());
    assert_eq!(recipient.conversation_key(sender.public_key()), Some(conversation_key));

    for case in v["nip44"].as_array().unwrap() {
      let nonce: [u8; 32] = key_bytes(case["nonce"].as_str().unwrap()).unwrap();
      let plaintext = match case.get("plaintext_repeat") {
        Some(r) => r[0].as_str().unwrap().repeat(r[1].as_u64().unwrap() as usize),
        None => case["plaintext"].as_str().unwrap().to_string(),
      };
      let payload = nip44_encrypt(&plaintext, &conversation_key, Some(nonce));
      match case.get("payload") {
        Some(expected) => assert_eq!(payload, expected.as_str().unwrap()),
        None => assert_eq!(hex::encode(Sha256::digest(payload.as_bytes())), case["payload_sha256"].as_str().unwrap()),
      }
      assert_eq!(nip44_decrypt(&payload, &conversation_key), Some(plaintext));
    }

    let shared = sender.shared_x(recipient.public_key()).unwrap();
    let iv: [u8; 16] = std::array::from_fn(|i| i as u8);
    for case in v["nip04"].as_array().unwrap() {
      let plaintext = case["plaintext"].as_str().unwrap();
      assert_eq!(nip04_encrypt(plaintext, &shared, Some(iv)), case["payload"].as_str().unwrap());
    }
    let random = &v["nip04_random"];
    let theirs = recipient.shared_x(sender.public_key()).unwrap();
    assert_eq!(nip04_decrypt(random["payload"].as_str().unwrap(), &theirs).as_deref(), random["plaintext"].as_str());

    let file = &v["file"];
    let data = hex::decode(file["data"].as_str().unwrap()).unwrap();
    let file_key: [u8; 32] = std::array::from_fn(|i| 200 + i as u8);
    let (encrypted, k, n) = encrypt_file(&data, Some((file_key, iv)));
    assert_eq!(hex::encode(&encrypted), file["encrypted"].as_str().unwrap());
    assert_eq!((k.as_str(), n.as_str()), (file["key"].as_str().unwrap(), file["nonce"].as_str().unwrap()));
    // A random key and nonce each time
    let (a, ka, na) = encrypt_file(&data, None);
    let (b, kb, nb) = encrypt_file(&data, None);
    assert!(a != b && ka != kb && na != nb);
    assert_eq!(a.len(), data.len() + 16);

    // A gift wrap made by upstream opens to the message it hides
    let wrap = verify_event(&v["gift_wrap"]["wrap"]).unwrap();
    assert_eq!(wrap.kind, 1059);
    let seal_json = nip44_decrypt(&wrap.content, &recipient.conversation_key(&wrap.pubkey).unwrap()).unwrap();
    let seal = verify_event(&serde_json::from_str(&seal_json).unwrap()).unwrap();
    assert_eq!((seal.kind, seal.pubkey.as_str()), (13, sender.public_key()));
    let message: Event = serde_json::from_str(&nip44_decrypt(&seal.content, &recipient.conversation_key(&seal.pubkey).unwrap()).unwrap()).unwrap();
    assert_eq!(message, serde_json::from_value::<Event>(v["gift_wrap"]["message"].clone()).unwrap());
  }

  /// Our gift wraps hide the message from everyone but the recipient, and
  /// back-date both layers by up to two days.
  #[test]
  fn test_gift_wrap() {
    let (sender, recipient) = (key(0x11), key(0x22));
    let message = rumor(sender.public_key(), 14, "Hello é", vec![vec!["p".into(), recipient.public_key().into()]], None);
    let wrap = gift_wrap(&sender, recipient.public_key(), &message).unwrap();
    assert!(verify_event(&serde_json::to_value(&wrap).unwrap()).is_some());
    assert_eq!(wrap.kind, 1059);
    assert_eq!(wrap.tags, vec![vec!["p".to_string(), recipient.public_key().to_string()]]);
    assert_ne!(wrap.pubkey, sender.public_key());
    let now = chrono::Utc::now().timestamp();
    assert!((now - NOSTR_TIMESTAMP_JITTER..=now).contains(&wrap.created_at));

    let seal_json = nip44_decrypt(&wrap.content, &recipient.conversation_key(&wrap.pubkey).unwrap()).unwrap();
    // The seal is written as Python's json.dumps() does
    assert!(seal_json.starts_with(&format!("{{\"pubkey\": \"{}\", \"created_at\": ", sender.public_key())));
    let seal = verify_event(&serde_json::from_str(&seal_json).unwrap()).unwrap();
    assert_eq!((seal.kind, seal.tags.len()), (13, 0));
    assert!((now - NOSTR_TIMESTAMP_JITTER..=now).contains(&seal.created_at));
    let inner = nip44_decrypt(&seal.content, &recipient.conversation_key(sender.public_key()).unwrap()).unwrap();
    assert_eq!(inner, message.to_py_json());
    assert!(!inner.contains("\"sig\""));

    // Nobody else can open it
    assert_eq!(nip44_decrypt(&wrap.content, &key(0x33).conversation_key(&wrap.pubkey).unwrap()), None);
    // A recipient that is not a point on the curve can not be wrapped for
    assert!(gift_wrap(&sender, &"f".repeat(64), &message).is_none());
  }

  #[test]
  fn test_py_dumps() {
    assert_eq!(py_dumps(&json!(["a", {"b": [1, 2]}, "é\n\u{1}"])), "[\"a\", {\"b\": [1, 2]}, \"é\\n\\u0001\"]");
    // The secret never shows up in debug output
    let debug = format!("{:?}", key(0x11));
    assert!(debug.contains(key(0x11).public_key()) && !debug.contains("1111111111"));
  }
}

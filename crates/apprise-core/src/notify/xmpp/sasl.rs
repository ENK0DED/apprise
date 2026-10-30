//! SASL mechanisms for the XMPP client: PLAIN and SCRAM (RFC 5802), with
//! the `-PLUS` channel-binding variants using `tls-exporter` (RFC 9266).
//!
//! The choice of mechanism follows slixmpp, which upstream uses: the highest
//! security score wins, every `-PLUS` variant scores 10 above its plain
//! SCRAM, and PLAIN is the last resort.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit, Mac};

/// Hash functions SCRAM can use here, with slixmpp's scores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScramHash {
  Sha1,
  Sha224,
  Sha256,
  Sha384,
  Sha512,
}

impl ScramHash {
  fn from_name(name: &str) -> Option<Self> {
    match name {
      "SHA-1" => Some(Self::Sha1),
      "SHA-224" => Some(Self::Sha224),
      "SHA-256" => Some(Self::Sha256),
      "SHA-384" => Some(Self::Sha384),
      "SHA-512" => Some(Self::Sha512),
      _ => None,
    }
  }

  /// slixmpp: SCRAM is 60 plus the hash's position in its list of hashlib
  /// algorithms.
  fn score(self) -> u32 {
    60 + match self {
      Self::Sha1 => 2,
      Self::Sha224 => 3,
      Self::Sha256 => 4,
      Self::Sha384 => 5,
      Self::Sha512 => 10,
    }
  }

  pub(super) fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
    macro_rules! mac {
      ($d:ty) => {{
        let mut mac = Hmac::<$d>::new_from_slice(key).expect("HMAC accepts any key size");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
      }};
    }
    match self {
      Self::Sha1 => mac!(sha1::Sha1),
      Self::Sha224 => mac!(sha2::Sha224),
      Self::Sha256 => mac!(sha2::Sha256),
      Self::Sha384 => mac!(sha2::Sha384),
      Self::Sha512 => mac!(sha2::Sha512),
    }
  }

  pub(super) fn hash(self, data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    match self {
      Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
      Self::Sha224 => sha2::Sha224::digest(data).to_vec(),
      Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
      Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
      Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
    }
  }

  /// PBKDF2 with this hash's HMAC (`Hi()` in RFC 5802).
  pub(super) fn hi(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    let mut block = salt.to_vec();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = self.hmac(password, &block);
    let mut result = u.clone();
    for _ in 1..iterations {
      u = self.hmac(password, &u);
      result.iter_mut().zip(&u).for_each(|(r, x)| *r ^= x);
    }
    result
  }
}

/// A mechanism this client can use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mechanism {
  Scram { hash: ScramHash, plus: bool },
  Plain,
}

impl Mechanism {
  pub fn parse(name: &str) -> Option<Self> {
    if name == "PLAIN" {
      return Some(Self::Plain);
    }
    let rest = name.strip_prefix("SCRAM-")?;
    let (hash, plus) = match rest.strip_suffix("-PLUS") {
      Some(h) => (h, true),
      None => (rest, false),
    };
    Some(Self::Scram { hash: ScramHash::from_name(hash)?, plus })
  }

  pub fn name(&self) -> String {
    match self {
      Self::Plain => "PLAIN".into(),
      Self::Scram { hash, plus } => {
        let hash = match hash {
          ScramHash::Sha1 => "SHA-1",
          ScramHash::Sha224 => "SHA-224",
          ScramHash::Sha256 => "SHA-256",
          ScramHash::Sha384 => "SHA-384",
          ScramHash::Sha512 => "SHA-512",
        };
        format!("SCRAM-{}{}", hash, if *plus { "-PLUS" } else { "" })
      }
    }
  }

  fn score(&self) -> u32 {
    match self {
      Self::Plain => 2,
      Self::Scram { hash, plus } => hash.score() + if *plus { 10 } else { 0 },
    }
  }
}

/// Pick the strongest mechanism the server offers. `-PLUS` variants are
/// only considered when channel binding data is available.
pub fn choose(offered: &[String], can_bind: bool) -> Option<Mechanism> {
  offered.iter().filter_map(|m| Mechanism::parse(m)).filter(|m| can_bind || !matches!(m, Mechanism::Scram { plus: true, .. })).max_by_key(Mechanism::score)
}

/// SASL PLAIN: an empty authorization identity, the user and the password.
pub fn plain(username: &str, password: &str) -> Vec<u8> {
  format!("\0{}\0{}", username, password).into_bytes()
}

/// RFC 5802 `saslname`: `,` and `=` are escaped.
fn saslname(value: &str) -> String {
  value.replace('=', "=3D").replace(',', "=2C")
}

/// One SCRAM exchange.
pub struct Scram {
  hash: ScramHash,
  gs2_header: String,
  channel_binding: Vec<u8>,
  client_first_bare: String,
  client_nonce: String,
  password: String,
  server_signature: Vec<u8>,
}

impl Scram {
  /// `gs2_flag` is `p=tls-exporter` with channel binding (whose data is
  /// `channel_binding`), `y` when the client could bind but the server did
  /// not offer it, and `n` otherwise.
  pub fn new(hash: ScramHash, username: &str, password: &str, gs2_flag: &str, channel_binding: Vec<u8>) -> Self {
    let nonce: [u8; 18] = rand::random();
    Self::with_nonce(hash, username, password, gs2_flag, channel_binding, &B64.encode(nonce))
  }

  fn with_nonce(hash: ScramHash, username: &str, password: &str, gs2_flag: &str, channel_binding: Vec<u8>, nonce: &str) -> Self {
    Self {
      hash,
      gs2_header: format!("{},,", gs2_flag),
      channel_binding,
      client_first_bare: format!("n={},r={}", saslname(username), nonce),
      client_nonce: nonce.to_string(),
      password: password.to_string(),
      server_signature: Vec::new(),
    }
  }

  pub fn client_first(&self) -> Vec<u8> {
    format!("{}{}", self.gs2_header, self.client_first_bare).into_bytes()
  }

  /// Answer the server-first message with the client proof.
  pub fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, String> {
    let server_first = std::str::from_utf8(server_first).map_err(|_| "server-first message is not UTF-8")?;
    let attr = |key: &str| server_first.split(',').find_map(|item| item.strip_prefix(key).and_then(|v| v.strip_prefix('=')));
    if attr("m").is_some() {
      return Err("received reserved attribute".into());
    }
    let nonce = attr("r").ok_or("server-first message has no nonce")?;
    if !nonce.starts_with(&self.client_nonce) {
      return Err("invalid nonce".into());
    }
    let salt = B64.decode(attr("s").ok_or("server-first message has no salt")?).map_err(|_| "invalid salt")?;
    let iterations: u32 = attr("i").and_then(|i| i.parse().ok()).filter(|i| *i > 0).ok_or("invalid iteration count")?;

    let mut cbind_input = self.gs2_header.clone().into_bytes();
    cbind_input.extend_from_slice(&self.channel_binding);
    let without_proof = format!("c={},r={}", B64.encode(cbind_input), nonce);

    let salted = self.hash.hi(self.password.as_bytes(), &salt, iterations);
    let client_key = self.hash.hmac(&salted, b"Client Key");
    let stored_key = self.hash.hash(&client_key);
    let auth_message = format!("{},{},{}", self.client_first_bare, server_first, without_proof);
    let client_signature = self.hash.hmac(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(&client_signature).map(|(a, b)| a ^ b).collect();
    let server_key = self.hash.hmac(&salted, b"Server Key");
    self.server_signature = self.hash.hmac(&server_key, auth_message.as_bytes());

    Ok(format!("{},p={}", without_proof, B64.encode(proof)).into_bytes())
  }

  /// Check the server-final message (`v=` the server signature).
  pub fn verify_server_final(&self, server_final: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(server_final).map_err(|_| "server-final message is not UTF-8")?;
    if let Some(error) = text.split(',').find_map(|i| i.strip_prefix("e=")) {
      return Err(error.to_string());
    }
    let verifier = text.split(',').find_map(|i| i.strip_prefix("v=")).ok_or("server-final message has no verifier")?;
    // Before client-final there is no signature to compare with, and an
    // empty `v=` would match the empty one: the server must not skip ahead.
    if self.server_signature.is_empty() {
      return Err("server-final message arrived before the server-first message".into());
    }
    if B64.decode(verifier).ok().as_deref() != Some(self.server_signature.as_slice()) {
      return Err("mutual authentication failed".into());
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_choose() {
    let offered = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let all = offered(&["PLAIN", "SCRAM-SHA-1", "SCRAM-SHA-1-PLUS", "SCRAM-SHA-256", "SCRAM-SHA-256-PLUS", "SCRAM-SHA-512", "DIGEST-MD5"]);
    assert_eq!(choose(&all, true), Some(Mechanism::Scram { hash: ScramHash::Sha256, plus: true }));
    assert_eq!(choose(&all, false), Some(Mechanism::Scram { hash: ScramHash::Sha512, plus: false }));
    // slixmpp scores SCRAM-SHA-1-PLUS (72) above SCRAM-SHA-512 (70)
    assert_eq!(choose(&offered(&["SCRAM-SHA-512", "SCRAM-SHA-1-PLUS"]), true), Some(Mechanism::Scram { hash: ScramHash::Sha1, plus: true }));
    assert_eq!(choose(&offered(&["PLAIN", "X-OAUTH2"]), true), Some(Mechanism::Plain));
    assert_eq!(choose(&offered(&["DIGEST-MD5", "SCRAM-SHA-1-PLUS"]), false), None);
    assert_eq!(Mechanism::Scram { hash: ScramHash::Sha512, plus: true }.name(), "SCRAM-SHA-512-PLUS");
  }

  #[test]
  fn test_plain() {
    assert_eq!(plain("user", "pencil"), b"\0user\0pencil");
  }

  #[test]
  fn test_scram_sha1_rfc5802_vector() {
    // RFC 5802, section 5
    let mut scram = Scram::with_nonce(ScramHash::Sha1, "user", "pencil", "n", Vec::new(), "fyko+d2lbbFgONRv9qkxdawL");
    assert_eq!(scram.client_first(), b"n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL");
    let client_final = scram.client_final(b"r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096").unwrap();
    assert_eq!(client_final, b"c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=");
    assert!(scram.verify_server_final(b"v=rmF9pqV8S7suAoZWja4dJRkFsKQ=").is_ok());
    assert!(scram.verify_server_final(b"v=AAAA").is_err());
    assert_eq!(scram.verify_server_final(b"e=invalid-proof"), Err("invalid-proof".into()));
  }

  #[test]
  fn test_scram_sha256_rfc7677_vector() {
    // RFC 7677, section 3
    let mut scram = Scram::with_nonce(ScramHash::Sha256, "user", "pencil", "n", Vec::new(), "rOprNGfwEbeRWgbNEkqO");
    let client_final = scram.client_final(b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
    assert_eq!(client_final, b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
    assert!(scram.verify_server_final(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").is_ok());
  }

  #[test]
  fn test_scram_channel_binding_input() {
    let mut scram = Scram::with_nonce(ScramHash::Sha256, "us,er=", "pw", "p=tls-exporter", vec![1, 2, 3], "abc");
    assert_eq!(scram.client_first(), b"p=tls-exporter,,n=us=2Cer=3D,r=abc");
    let client_final = String::from_utf8(scram.client_final(b"r=abcdef,s=c2FsdA==,i=1").unwrap()).unwrap();
    let mut cbind = b"p=tls-exporter,,".to_vec();
    cbind.extend_from_slice(&[1, 2, 3]);
    assert!(client_final.starts_with(&format!("c={},r=abcdef,p=", B64.encode(cbind))), "{}", client_final);

    // A server nonce that does not extend ours is rejected
    let mut scram = Scram::with_nonce(ScramHash::Sha1, "u", "p", "n", vec![], "abc");
    assert!(scram.client_final(b"r=xyz,s=c2FsdA==,i=1").is_err());
    assert!(scram.client_final(b"m=ext,r=abcd,s=c2FsdA==,i=1").is_err());
  }

  #[test]
  fn test_server_final_before_server_first_is_rejected() {
    let scram = Scram::new(ScramHash::Sha256, "user", "pencil", "n", Vec::new());
    // An empty verifier would equal the not-yet-computed signature
    assert!(scram.verify_server_final(b"v=").is_err());
    assert!(scram.verify_server_final(b"v=AAAA").is_err());
  }
}

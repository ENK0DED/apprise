use pgp::composed::{ArmorOptions, Deserializable, Message};
use pgp::crypto::ecc_curve::ECCCurve;

use super::*;

const VALID_PRV: &str = include_str!("testdata/valid-prv.asc");
const VALID_PUB: &str = include_str!("testdata/valid-pub.asc");
const CORRUPT_PUB: &str = include_str!("testdata/corrupt-pub.asc");
/// Made by upstream's `keygen()` (PGPy): Autocrypt-shaped.
const PGPY_PRV: &str = include_str!("testdata/pgpy-prv.asc");
const PGPY_PUB: &str = include_str!("testdata/pgpy-pub.asc");

fn controller(dir: &tempfile::TempDir, email: Option<&str>) -> PgpController {
  PgpController::new(None, None, email, "Apprise", None).with_path(Some(dir.path().to_path_buf()))
}

fn write(dir: &tempfile::TempDir, name: &str, data: &str) -> String {
  let path = dir.path().join(name);
  std::fs::write(&path, data).unwrap();
  path.to_string_lossy().to_string()
}

/// A quick Ed25519/X25519 key in the given shape.
fn ed25519_key(user_ids: &[&str], with_subkey: bool, passphrase: Option<&str>) -> SignedSecretKey {
  let mut params = SecretKeyParamsBuilder::default();
  params.key_type(KeyType::Ed25519Legacy).can_certify(true).can_sign(true).primary_user_id(user_ids[0].to_string());
  params.user_ids(user_ids[1..].iter().map(|s| s.to_string()).collect());
  params.passphrase(passphrase.map(str::to_string));
  if with_subkey {
    let mut sub = SubkeyParamsBuilder::default();
    sub.key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy)).can_encrypt(EncryptionCaps::All).passphrase(passphrase.map(str::to_string));
    params.subkeys(vec![sub.build().unwrap()]);
  }
  params.build().unwrap().generate(rand_core06::OsRng).unwrap()
}

fn armored_key(key: &SignedSecretKey) -> String {
  key.to_armored_string(ArmorOptions::default()).unwrap()
}

#[test]
fn test_key_candidates() {
  // Upstream test_pub_key_candidates_* / test_prv_key_candidates_*
  let dir = tempfile::tempdir().unwrap();
  assert_eq!(controller(&dir, None).pub_key_candidates(&[]), ["pgp-public.asc", "pgp-pub.asc", "public.asc", "pub.asc"]);
  let c = controller(&dir, Some("Chris@Nuxref.com"));
  assert_eq!(c.pub_key_candidates(&[]), ["chris@nuxref.com-pub.asc", "chris-pub.asc", "pgp-public.asc", "pgp-pub.asc", "public.asc", "pub.asc"]);
  // A recipient address replaces the sender's
  assert_eq!(c.pub_key_candidates(&["bob@example.com"]), ["bob@example.com-pub.asc", "bob-pub.asc", "pgp-public.asc", "pgp-pub.asc", "public.asc", "pub.asc"]);
  assert_eq!(controller(&dir, None).prv_key_candidates(), ["pgp-private.asc", "pgp-prv.asc", "private.asc", "prv.asc"]);
  assert_eq!(c.prv_key_candidates(), ["chris@nuxref.com-prv.asc", "chris-prv.asc", "pgp-private.asc", "pgp-prv.asc", "private.asc", "prv.asc"]);
}

#[test]
fn test_keyfile_discovery() {
  let dir = tempfile::tempdir().unwrap();
  let c = controller(&dir, Some("user@example.com"));
  assert!(c.private_keyfile().is_none());
  assert!(c.public_keyfile(&[]).is_none());
  write(&dir, "prv.asc", VALID_PRV);
  assert!(c.private_keyfile().unwrap().ends_with("prv.asc"));
  write(&dir, "user-prv.asc", VALID_PRV);
  assert!(c.private_keyfile().unwrap().ends_with("user-prv.asc"));
  write(&dir, "user@example.com-pub.asc", VALID_PUB);
  assert!(c.public_keyfile(&[]).unwrap().ends_with("user@example.com-pub.asc"));
  // Without a storage directory nothing is discovered
  let c = PgpController::new(None, None, Some("user@example.com"), "Apprise", None);
  assert!(c.private_keyfile().is_none());
}

#[tokio::test]
async fn test_private_key_loading() {
  let dir = tempfile::tempdir().unwrap();
  let path = write(&dir, "explicit.asc", VALID_PRV);
  let c = PgpController::new(None, Some(&path), Some("user@example.com"), "Apprise", None);
  let key = c.private_key().await.expect("fixture loads");
  // Cached by source: a changed file is not parsed again until it expires
  let other = ed25519_key(&["O <o@example.com>"], true, None);
  std::fs::write(&path, armored_key(&other)).unwrap();
  assert_eq!(c.private_key().await.unwrap().fingerprint(), key.fingerprint());
  c.cache().values_mut().for_each(|(_, expires)| *expires = Instant::now() - Duration::from_secs(1));
  assert_eq!(c.private_key().await.unwrap().fingerprint(), other.fingerprint());
  // As upstream, a key file that disappears is not usable any more
  std::fs::remove_file(&path).unwrap();
  assert!(c.private_key().await.is_none());

  // An explicit key that cannot be read, or none at all
  assert!(PgpController::new(None, Some("/nonexistent/key.asc"), None, "Apprise", None).private_key().await.is_none());
  assert!(controller(&dir, Some("user@example.com")).private_key().await.is_none());

  // Public-only, malformed and passphrase-protected files are refused
  let public = write(&dir, "public.asc", VALID_PUB);
  assert!(PgpController::new(None, Some(&public), None, "Apprise", None).private_key().await.is_none());
  let garbage = write(&dir, "garbage.asc", "not a key");
  assert!(PgpController::new(None, Some(&garbage), None, "Apprise", None).private_key().await.is_none());
  let protected = write(&dir, "protected.asc", &armored_key(&ed25519_key(&["P <p@example.com>"], true, Some("secret"))));
  assert!(PgpController::new(None, Some(&protected), None, "Apprise", None).private_key().await.is_none());

  // An explicit private key larger than 32000 bytes is refused
  let big = write(&dir, "big.asc", &format!("{}\n{}", VALID_PRV, " ".repeat(MAX_PGP_PRIVATE_KEY_SIZE)));
  assert!(PgpController::new(None, Some(&big), None, "Apprise", None).private_key().await.is_none());
}

#[tokio::test]
async fn test_sign() {
  let dir = tempfile::tempdir().unwrap();
  let path = write(&dir, "prv.asc", VALID_PRV);
  let c = PgpController::new(None, Some(&path), None, "Apprise", None);
  let (signature, micalg) = c.sign(b"Hello, World!\r\n").await.unwrap();
  assert!(signature.starts_with("-----BEGIN PGP SIGNATURE-----"));
  assert_eq!(micalg, "pgp-sha256");
  let (sig, _) = DetachedSignature::from_string(&signature).unwrap();
  let public = SignedSecretKey::from_string(VALID_PRV).unwrap().0.to_public_key();
  sig.verify(&public, &b"Hello, World!\r\n"[..]).unwrap();
  assert!(sig.verify(&public, &b"Hello, World!\n"[..]).is_err());

  // No key, no signature
  let empty = tempfile::tempdir().unwrap();
  assert!(controller(&empty, None).sign(b"x").await.is_none());
  // A key that may not sign
  let mut params = SecretKeyParamsBuilder::default();
  params.key_type(KeyType::Ed25519Legacy).can_certify(true).can_sign(false).primary_user_id("A <a@example.com>".to_string());
  let cert_only = params.build().unwrap().generate(rand_core06::OsRng).unwrap();
  let path = write(&dir, "cert-only.asc", &armored_key(&cert_only));
  assert!(PgpController::new(None, Some(&path), None, "Apprise", None).sign(b"x").await.is_none());
}

#[tokio::test]
async fn test_public_key_sources() {
  let dir = tempfile::tempdir().unwrap();
  // An explicit key, then the cached copy
  let path = write(&dir, "explicit.asc", PGPY_PUB);
  let c = PgpController::new(Some(&path), None, Some("user@example.com"), "Apprise", None);
  let key = c.public_key(&[], Some(false)).await.unwrap();
  assert_eq!(key.details.users[0].id.as_str(), Some("Recipient <recipient@example.com>"));
  // Never generated when pgppub= is given
  assert!(!c.keygen(None, None, false).await);

  // A corrupt or oversized explicit key
  let corrupt = write(&dir, "corrupt.asc", CORRUPT_PUB);
  assert!(PgpController::new(Some(&corrupt), None, None, "Apprise", None).public_key(&[], None).await.is_none());
  let big = write(&dir, "big.asc", &format!("{}{}", PGPY_PUB, " ".repeat(MAX_PGP_PUBLIC_KEY_SIZE)));
  assert!(PgpController::new(Some(&big), None, None, "Apprise", None).public_key(&[], None).await.is_none());

  // A discovered file named after the recipient; a malformed one is ignored
  let c = controller(&dir, Some("user@example.com"));
  assert!(c.public_key(&["bob@example.com"], Some(false)).await.is_none());
  write(&dir, "bob-pub.asc", PGPY_PUB);
  assert!(c.public_key(&["bob@example.com"], Some(false)).await.is_some());
  write(&dir, "eve-pub.asc", "garbage");
  assert!(c.public_key(&["eve@example.com"], Some(false)).await.is_none());
  // A private key file works as a public key source too
  write(&dir, "carol-pub.asc", PGPY_PRV);
  assert!(c.public_key(&["carol@example.com"], Some(false)).await.is_some());
}

#[tokio::test]
async fn test_public_key_from_wkd() {
  use wiremock::matchers::{method, path_regex};
  use wiremock::{Mock, MockServer, ResponseTemplate};
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path_regex(r"^/openpgpkey\.example\.com/"))
    .respond_with(ResponseTemplate::new(200).set_body_string(PGPY_PUB))
    .mount(&server)
    .await;
  Mock::given(method("GET")).and(path_regex(r"^/example\.com/")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
  let dir = tempfile::tempdir().unwrap();
  let mut c = controller(&dir, Some("sender@other.test"));
  c.set_wkd(Some(WkdController::for_test(&server.uri())));
  // The recipient's key, never the sender's
  assert!(c.public_key(&["friend@example.com"], Some(false)).await.is_some());
  assert!(c.public_key(&["friend@other.test"], Some(false)).await.is_none());
  // Cached, and pruned with the WKD cache once expired
  assert!(c.fetch_wkd_key(&["friend@example.com"]).await.is_some());
  c.cache().values_mut().for_each(|(_, expires)| *expires = Instant::now() - Duration::from_secs(1));
  c.prune();
  assert!(c.cache().is_empty());
}

#[tokio::test]
async fn test_keygen_and_autocrypt() {
  let dir = tempfile::tempdir().unwrap();
  let c = controller(&dir, Some("user@example.com"));
  // Without a storage directory nothing is generated
  assert!(!PgpController::new(None, None, Some("user@example.com"), "Apprise", None).keygen(None, None, false).await);
  assert!(PgpController::new(None, None, Some("user@example.com"), "Apprise", None).autocrypt_header().await.is_none());

  // autocrypt_header() generates the sender's pair when allowed
  let header = c.autocrypt_header().await.expect("generated key is Autocrypt-shaped");
  assert!(dir.path().join("user-pub.asc").is_file() && dir.path().join("user-prv.asc").is_file());
  assert!(header.starts_with("addr=user@example.com; prefer-encrypt=mutual; keydata="));
  let keydata = header.split("keydata=").nth(1).unwrap();
  assert!(keydata.split("\r\n ").all(|l| l.len() <= AUTOCRYPT_FOLD_WIDTH));
  assert!(keydata.split("\r\n ").take(2).all(|l| l.len() == AUTOCRYPT_FOLD_WIDTH));
  let raw = base64::engine::general_purpose::STANDARD.decode(keydata.replace("\r\n ", "")).unwrap();
  let advertised = SignedPublicKey::from_bytes(std::io::Cursor::new(raw)).unwrap();
  assert!(has_encryption_subkey(&advertised));
  let generated = c.private_key().await.unwrap();
  assert_eq!(advertised.fingerprint(), generated.fingerprint());
  assert_eq!(generated.details.users[0].id.as_str(), Some("Apprise <user@example.com>"));

  // An existing pair is kept unless forced
  let before = std::fs::read(dir.path().join("user-pub.asc")).unwrap();
  assert!(c.keygen(None, None, false).await);
  assert_eq!(std::fs::read(dir.path().join("user-pub.asc")).unwrap(), before);

  // Encrypting to the generated key opens with its private half
  let armored = c.encrypt(b"top secret", &[], Some(false)).await.unwrap();
  let (message, _) = Message::from_string(&armored).unwrap();
  let decrypted = message.decrypt(&Password::empty(), &generated).unwrap();
  let mut data = if decrypted.is_compressed() { decrypted.decompress().unwrap() } else { decrypted };
  assert_eq!(data.as_data_vec().unwrap(), b"top secret");

  // Autogen off: nothing is created
  let other = tempfile::tempdir().unwrap();
  let mut c = controller(&other, Some("user@example.com"));
  c.set_autogen(false);
  assert!(c.autocrypt_header().await.is_none());
  assert!(c.public_key(&[], None).await.is_none());
  assert!(std::fs::read_dir(other.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn test_autocrypt_needs_our_own_suitable_key() {
  let dir = tempfile::tempdir().unwrap();
  // Upstream's own keygen output qualifies
  let prv = write(&dir, "pgpy.asc", PGPY_PRV);
  let c = PgpController::new(None, Some(&prv), Some("recipient@example.com"), "Apprise", None);
  assert!(c.autocrypt_header().await.is_some());
  // No sender address
  assert!(PgpController::new(None, Some(&prv), None, "Apprise", None).autocrypt_header().await.is_none());
  // A recipient's public key (pgppub=) is never advertised
  let public = write(&dir, "pub.asc", PGPY_PUB);
  let mut c = PgpController::new(Some(&public), None, Some("recipient@example.com"), "Apprise", None).with_path(Some(dir.path().to_path_buf()));
  c.set_autogen(false);
  assert!(c.autocrypt_header().await.is_none());
  // No encryption subkey (the upstream fixture is a single RSA key)
  let fixture = write(&dir, "valid.asc", VALID_PRV);
  assert!(PgpController::new(None, Some(&fixture), Some("test@example.com"), "Apprise", None).autocrypt_header().await.is_none());
  // Two user IDs
  let two = write(&dir, "two.asc", &armored_key(&ed25519_key(&["A <a@example.com>", "B <b@example.com>"], true, None)));
  assert!(PgpController::new(None, Some(&two), Some("a@example.com"), "Apprise", None).autocrypt_header().await.is_none());
  // One user ID and one encryption subkey: accepted
  let one = write(&dir, "one.asc", &armored_key(&ed25519_key(&["A <a@example.com>"], true, None)));
  assert!(PgpController::new(None, Some(&one), Some("a@example.com"), "Apprise", None).autocrypt_header().await.is_some());
  // No subkey at all
  let none = write(&dir, "none.asc", &armored_key(&ed25519_key(&["A <a@example.com>"], false, None)));
  assert!(PgpController::new(None, Some(&none), Some("a@example.com"), "Apprise", None).autocrypt_header().await.is_none());
}

#[test]
fn test_has_encryption_subkey_shapes() {
  let one = ed25519_key(&["A <a@example.com>"], true, None).to_public_key();
  assert!(has_encryption_subkey(&one));
  // A second subkey
  let mut two_subkeys = one.clone();
  two_subkeys.public_subkeys.push(one.public_subkeys[0].clone());
  assert!(!has_encryption_subkey(&two_subkeys));
  // A second signature on the user ID
  let mut extra_sig = one.clone();
  let sig = extra_sig.details.users[0].signatures[0].clone();
  extra_sig.details.users[0].signatures.push(sig);
  assert!(!has_encryption_subkey(&extra_sig));
  // A second subkey binding
  let mut extra_binding = one.clone();
  let binding = extra_binding.public_subkeys[0].signatures[0].clone();
  extra_binding.public_subkeys[0].signatures.push(binding);
  assert!(!has_encryption_subkey(&extra_binding));
  // A user ID signed by someone else only
  let mut third_party = one.clone();
  third_party.details.users[0].signatures = ed25519_key(&["A <a@example.com>"], true, None).to_public_key().details.users[0].signatures.clone();
  assert!(!has_encryption_subkey(&third_party));
  // A signing-only subkey
  let mut params = SecretKeyParamsBuilder::default();
  let mut sub = SubkeyParamsBuilder::default();
  sub.key_type(KeyType::Ed25519Legacy).can_sign(true);
  params.key_type(KeyType::Ed25519Legacy).can_certify(true).can_sign(true).primary_user_id("A <a@example.com>".to_string()).subkeys(vec![sub.build().unwrap()]);
  let signing_only = params.build().unwrap().generate(rand_core06::OsRng).unwrap().to_public_key();
  assert!(!has_encryption_subkey(&signing_only));
}

#[test]
fn test_upstream_fixtures_parse() {
  assert!(matches!(parse_key(VALID_PRV.as_bytes()), Some(PublicOrSecret::Secret(_))));
  assert!(matches!(parse_key(VALID_PUB.as_bytes()), Some(PublicOrSecret::Public(_))));
  assert!(matches!(parse_key(PGPY_PRV.as_bytes()), Some(PublicOrSecret::Secret(_))));
  assert!(parse_key(CORRUPT_PUB.as_bytes()).is_none());
  assert!(parse_key(b"").is_none());
  // Binary keys too
  let binary = SignedPublicKey::from_string(PGPY_PUB).unwrap().0.to_bytes().unwrap();
  assert!(matches!(parse_key(&binary), Some(PublicOrSecret::Public(_))));
}

/// The first key as rPGP's reader-based parser finds it (what
/// [`parse_key`] did before it decoded armor itself): secret or not, and the
/// key's packets.
fn parse_key_reader(data: &[u8]) -> Option<(bool, Vec<u8>)> {
  let (mut keys, _) = PublicOrSecret::from_reader_many(std::io::Cursor::new(data)).ok()?;
  key_packets(keys.next()?.ok()?)
}

fn key_packets(key: PublicOrSecret) -> Option<(bool, Vec<u8>)> {
  match key {
    PublicOrSecret::Public(k) => Some((false, k.to_bytes().ok()?)),
    PublicOrSecret::Secret(k) => Some((true, k.to_bytes().ok()?)),
  }
}

/// Parsing from a byte slice (armor decoded first) finds the same key as
/// rPGP's reader-based parser, armored or binary, malformed or not.
#[test]
fn test_parse_key_matches_rpgp_reader() {
  let binary = SignedPublicKey::from_string(PGPY_PUB).unwrap().0.to_bytes().unwrap();
  let mut two_binary = binary.clone();
  two_binary.extend(SignedPublicKey::from_string(VALID_PUB).unwrap().0.to_bytes().unwrap());
  let with_headers = VALID_PUB.replacen("BLOCK-----\n", "BLOCK-----\nComment: hello\nVersion: 1\n", 1);
  let crlf = VALID_PRV.replace('\n', "\r\n");
  let no_checksum: String = VALID_PUB.lines().filter(|l| !l.starts_with('=')).map(|l| format!("{}\n", l)).collect();
  let bad_checksum = VALID_PUB.replace("=ZY2q", "=AAAA");
  let no_footer = VALID_PUB.replace("-----END PGP PUBLIC KEY BLOCK-----", "");
  let wrong_footer = VALID_PUB.replace("-----END PGP PUBLIC KEY BLOCK-----", "-----END PGP MESSAGE-----");
  let truncated = &VALID_PUB[..VALID_PUB.len() / 2];
  let message = VALID_PUB.replace("PUBLIC KEY BLOCK", "MESSAGE");
  let file = VALID_PUB.replace("PUBLIC KEY BLOCK", "ARMORED FILE");
  let leading = format!("Some text first\n\n{}", VALID_PUB);
  let trailing = format!("{}trailing text\n", VALID_PUB);
  let two_armored = format!("{}{}", PGPY_PUB, VALID_PUB);
  let cases: Vec<(&str, &[u8])> = vec![
    ("valid-pub", VALID_PUB.as_bytes()),
    ("valid-prv", VALID_PRV.as_bytes()),
    ("pgpy-pub", PGPY_PUB.as_bytes()),
    ("pgpy-prv", PGPY_PRV.as_bytes()),
    ("corrupt-pub", CORRUPT_PUB.as_bytes()),
    ("binary", &binary),
    ("two binary keys", &two_binary),
    ("truncated binary", &binary[..binary.len() / 2]),
    ("armor headers", with_headers.as_bytes()),
    ("crlf", crlf.as_bytes()),
    ("no checksum", no_checksum.as_bytes()),
    ("bad checksum", bad_checksum.as_bytes()),
    ("no footer", no_footer.as_bytes()),
    ("wrong footer", wrong_footer.as_bytes()),
    ("truncated armor", truncated.as_bytes()),
    ("message block", message.as_bytes()),
    ("file block", file.as_bytes()),
    ("text before the block", leading.as_bytes()),
    ("text after the block", trailing.as_bytes()),
    ("two armored keys", two_armored.as_bytes()),
    ("empty", b""),
    ("garbage", b"not a key at all"),
    ("binary garbage", &[0x99, 0x00, 0x01, 0x02]),
  ];
  for (name, data) in cases {
    let expected = parse_key_reader(data);
    assert_eq!(parse_key(data).and_then(key_packets), expected, "{}", name);
  }
}

/// A reproducible RNG, so two encryptions can be compared byte for byte.
struct CountingRng(u64);

impl rand_core06::RngCore for CountingRng {
  fn next_u32(&mut self) -> u32 {
    self.next_u64() as u32
  }
  fn next_u64(&mut self) -> u64 {
    self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    self.0
  }
  fn fill_bytes(&mut self, dest: &mut [u8]) {
    for chunk in dest.chunks_mut(8) {
      let n = chunk.len();
      chunk.copy_from_slice(&self.next_u64().to_le_bytes()[..n]);
    }
  }
  fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core06::Error> {
    self.fill_bytes(dest);
    Ok(())
  }
}

impl rand_core06::CryptoRng for CountingRng {}

/// Armoring packets serialized into a `Vec` gives exactly what rPGP's own
/// `to_armored_string()` writes, for keys, signatures and messages.
#[test]
fn test_armor_matches_rpgp() {
  let (secret, _) = SignedSecretKey::from_string(PGPY_PRV).unwrap();
  let public = secret.to_public_key();
  assert_eq!(armored(BlockType::PrivateKey, &secret.to_bytes().unwrap()).unwrap(), secret.to_armored_string(ArmorOptions::default()).unwrap());
  assert_eq!(armored(BlockType::PublicKey, &public.to_bytes().unwrap()).unwrap(), public.to_armored_string(ArmorOptions::default()).unwrap());
  let signature = DetachedSignature::sign_binary_data(CountingRng(1), &secret.primary_key, &Password::empty(), HashAlgorithm::Sha256, &b"data"[..]).unwrap();
  assert_eq!(armored(BlockType::Signature, &signature.to_bytes().unwrap()).unwrap(), signature.to_armored_string(ArmorOptions::default()).unwrap());

  // A message as encrypt_to() builds it, larger than one 512 KiB partial
  // chunk, written both ways from the same random numbers
  let message: Vec<u8> = (0..600 * 1024u32).map(|i| (i % 251) as u8).collect();
  let build = |seed: u64| {
    let mut rng = CountingRng(seed);
    let mut builder = MessageBuilder::from_bytes("", message.clone()).seipd_v1(&mut rng, preferred_cipher(&public));
    builder.compression(CompressionAlgorithm::ZIP);
    builder.encrypt_to_key(&mut rng, &public.public_subkeys[0].key).unwrap();
    (builder, rng)
  };
  let (builder, rng) = build(7);
  let ours = armored(BlockType::Message, &builder.to_vec(rng).unwrap()).unwrap();
  let (builder, rng) = build(7);
  assert_eq!(ours, builder.to_armored_string(rng, ArmorOptions::default()).unwrap());
}

/// Signing and encryption run in the blocking pool, so a current-thread
/// executor keeps serving other tasks and deadlines meanwhile (the
/// GPT-6.1-Sol review's RT5).
#[tokio::test]
async fn test_sign_and_encrypt_leave_the_executor_free() {
  use std::sync::atomic::{AtomicUsize, Ordering};
  let dir = tempfile::tempdir().unwrap();
  write(&dir, "pgp-public.asc", PGPY_PUB);
  write(&dir, "pgp-private.asc", PGPY_PRV);
  let pgp = controller(&dir, Some("user@nuxref.com"));
  // Load the keys first, so only the PGP work itself is left
  assert!(pgp.public_key(&[], Some(false)).await.is_some());
  assert!(pgp.private_key().await.is_some());
  let message = vec![0x5a; 256 * 1024];

  // A deadline that has already passed fires once the work yields
  assert!(tokio::time::timeout(Duration::ZERO, pgp.encrypt(&message, &[], Some(false))).await.is_err());
  assert!(tokio::time::timeout(Duration::ZERO, pgp.sign(&message)).await.is_err());

  // Another task keeps running while the work is in progress
  let ticks = Arc::new(AtomicUsize::new(0));
  let heartbeat = tokio::spawn({
    let ticks = ticks.clone();
    async move {
      loop {
        ticks.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
      }
    }
  });
  assert!(pgp.encrypt(&message, &[], Some(false)).await.is_some());
  let after_encrypt = ticks.load(Ordering::SeqCst);
  assert!(after_encrypt > 0);
  assert!(pgp.sign(&message).await.is_some());
  assert!(ticks.load(Ordering::SeqCst) > after_encrypt);
  heartbeat.abort();
}

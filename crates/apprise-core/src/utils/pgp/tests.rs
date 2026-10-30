use pgp::composed::{Deserializable, Message};
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

fn armored(key: &SignedSecretKey) -> String {
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
  std::fs::write(&path, armored(&other)).unwrap();
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
  let protected = write(&dir, "protected.asc", &armored(&ed25519_key(&["P <p@example.com>"], true, Some("secret"))));
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
  let path = write(&dir, "cert-only.asc", &armored(&cert_only));
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
  let advertised = SignedPublicKey::from_bytes(Cursor::new(raw)).unwrap();
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
  let two = write(&dir, "two.asc", &armored(&ed25519_key(&["A <a@example.com>", "B <b@example.com>"], true, None)));
  assert!(PgpController::new(None, Some(&two), Some("a@example.com"), "Apprise", None).autocrypt_header().await.is_none());
  // One user ID and one encryption subkey: accepted
  let one = write(&dir, "one.asc", &armored(&ed25519_key(&["A <a@example.com>"], true, None)));
  assert!(PgpController::new(None, Some(&one), Some("a@example.com"), "Apprise", None).autocrypt_header().await.is_some());
  // No subkey at all
  let none = write(&dir, "none.asc", &armored(&ed25519_key(&["A <a@example.com>"], false, None)));
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

//! Bark (iOS) notifications through a Bark server.
//!
//! With `?key=` every request is encrypted with AES-GCM, as the Bark app
//! expects: the parameters are sealed as compact JSON under a fresh
//! 12-character IV and sent as `{device_key, ciphertext, iv}`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm};
use async_trait::async_trait;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list};

type Aes192Gcm = AesGcm<aes_gcm::aes::Aes192, aes_gcm::aead::consts::U12>;

/// Sounds shipped with the Bark app (`BARK_SOUNDS`).
const BARK_SOUNDS: &[&str] = &[
  "alarm.caf",
  "anticipate.caf",
  "bell.caf",
  "birdsong.caf",
  "bloom.caf",
  "calypso.caf",
  "chime.caf",
  "choo.caf",
  "descent.caf",
  "electronic.caf",
  "fanfare.caf",
  "glass.caf",
  "gotosleep.caf",
  "healthnotification.caf",
  "horn.caf",
  "ladder.caf",
  "mailsent.caf",
  "minuet.caf",
  "multiwayinvitation.caf",
  "newmail.caf",
  "newsflash.caf",
  "noir.caf",
  "paymentsuccess.caf",
  "shake.caf",
  "sherwoodforest.caf",
  "silence.caf",
  "spell.caf",
  "suspense.caf",
  "telegraph.caf",
  "tiptoes.caf",
  "typewriters.caf",
  "update.caf",
];

/// Interruption levels (`BARK_LEVELS`), matched on their first character.
const BARK_LEVELS: &[&str] = &["active", "timeSensitive", "passive", "critical"];

/// Random bytes behind each IV; URL-safe base64 turns 9 into 12 characters.
const BARK_GCM_IV_RANDOM_BYTES: usize = 9;

/// The AES-GCM cipher for a 16, 24 or 32 byte key.
enum BarkCipher {
  Aes128(Box<Aes128Gcm>),
  Aes192(Box<Aes192Gcm>),
  Aes256(Box<Aes256Gcm>),
}

impl BarkCipher {
  fn new(key: &[u8]) -> Option<Self> {
    match key.len() {
      16 => Aes128Gcm::new_from_slice(key).ok().map(|c| Self::Aes128(Box::new(c))),
      24 => Aes192Gcm::new_from_slice(key).ok().map(|c| Self::Aes192(Box::new(c))),
      32 => Aes256Gcm::new_from_slice(key).ok().map(|c| Self::Aes256(Box::new(c))),
      _ => None,
    }
  }

  /// Seal `plaintext`; the tag is appended to the ciphertext.
  fn encrypt(&self, iv: &[u8; 12], plaintext: &[u8]) -> Option<Vec<u8>> {
    let nonce = iv.into();
    match self {
      Self::Aes128(c) => c.encrypt(nonce, plaintext),
      Self::Aes192(c) => c.encrypt(nonce, plaintext),
      Self::Aes256(c) => c.encrypt(nonce, plaintext),
    }
    .ok()
  }
}

pub struct Bark {
  host: String,
  port: Option<u16>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  targets: Vec<String>,
  include_image: bool,
  sound: Option<&'static str>,
  category: Option<String>,
  group: Option<String>,
  level: Option<&'static str>,
  click: Option<String>,
  badge: Option<u64>,
  volume: Option<u8>,
  icon: Option<String>,
  call: bool,
  cipher: Option<BarkCipher>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Compact JSON with keys in the given order, as upstream's `json.dumps()`
/// of an insertion-ordered dict.
fn ordered_json(entries: &[(&str, Value)]) -> String {
  let body: Vec<String> = entries.iter().map(|(k, v)| format!("{}:{}", Value::String(k.to_string()), v)).collect();
  format!("{{{}}}", body.join(","))
}

impl Bark {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;
    let qsd = |k: &str| url.get(k).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);

    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.push(to.to_string());
    }

    let badge = qsd("badge").and_then(|b| match b.parse::<i64>() {
      Ok(n) if n >= 0 => Some(n as u64),
      _ => {
        tracing::warn!("The specified Bark badge ({}) is not valid", b);
        None
      }
    });

    let sound = qsd("sound").and_then(|s| {
      let s = s.to_lowercase();
      let found = BARK_SOUNDS.iter().copied().find(|f| f.starts_with(&s));
      if found.is_none() {
        tracing::warn!("The specified Bark sound ({}) was not found", s);
      }
      found
    });

    let volume = qsd("volume").and_then(|v| match v.parse::<i64>() {
      Ok(n) if (0..=10).contains(&n) => Some(n as u8),
      _ => {
        tracing::warn!("The specified Bark volume ({}) is not valid. Must be between 0 and 10", v);
        None
      }
    });

    let level = qsd("level").and_then(|l| {
      let found = BARK_LEVELS.iter().copied().find(|f| f.chars().next() == l.chars().next());
      if found.is_none() {
        tracing::warn!("The specified Bark level ({}) is not valid", l);
      }
      found
    });

    // Encryption is off unless a key was explicitly given; a '+' in the key
    // is kept literally, as upstream reads it
    let cipher = match url.get("key").filter(|k| !k.is_empty()) {
      None => None,
      Some(key) => {
        if !key.is_ascii() {
          tracing::warn!("The Bark encryption key must contain only ASCII characters.");
          return None;
        }
        let Some(cipher) = BarkCipher::new(key.as_bytes()) else {
          tracing::warn!("The Bark encryption key must contain exactly 16, 24, or 32 ASCII characters.");
          return None;
        };
        Some(cipher)
      }
    };

    Some(Self {
      host,
      port: url.port,
      secure: url.schema == "barks",
      user: url.user.clone(),
      password: url.password.clone(),
      targets: parse_list(&targets.join(" ")),
      include_image: url.get("image").map(parse_bool).unwrap_or(true),
      sound,
      category: qsd("category"),
      group: qsd("group"),
      level,
      click: qsd("click"),
      badge,
      volume,
      icon: qsd("icon"),
      call: url.get("call").is_some_and(parse_bool),
      cipher,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Bark",
      service_url: Some("https://github.com/Finb/Bark"),
      setup_url: Some("https://appriseit.com/services/bark/"),
      protocols: vec!["bark", "barks"],
      description: "Send notifications to iOS devices via Bark.",
      attachment_support: false,
    }
  }

  /// The parameters every device receives, in upstream's order.
  fn parameters(&self, ctx: &NotifyContext) -> Vec<(&'static str, Value)> {
    let title = if ctx.title.is_empty() { ctx.asset.app_desc.clone() } else { ctx.title.clone() };
    let mut params: Vec<(&'static str, Value)> = vec![("title", json!(title))];
    params.push((if ctx.body_format == NotifyFormat::Markdown { "markdown" } else { "body" }, json!(ctx.body)));

    // A custom icon wins over the theme image
    let icon = self.icon.clone().or_else(|| if self.include_image { ctx.image_url(crate::asset::NotifyImageSize::Xy128) } else { None });
    if let Some(icon) = icon {
      params.push(("icon", json!(icon)));
    }
    if let Some(sound) = self.sound {
      params.push(("sound", json!(sound)));
    }
    if let Some(click) = &self.click {
      params.push(("url", json!(click)));
    }
    if let Some(badge) = self.badge.filter(|b| *b > 0) {
      params.push(("badge", json!(badge)));
    }
    if let Some(level) = self.level {
      params.push(("level", json!(level)));
    }
    if let Some(category) = &self.category {
      params.push(("category", json!(category)));
    }
    if let Some(group) = &self.group {
      params.push(("group", json!(group)));
    }
    if let Some(volume) = self.volume.filter(|v| *v > 0) {
      params.push(("volume", json!(volume)));
    }
    if self.call {
      params.push(("call", json!(1)));
    }
    params
  }

  /// Encrypt the parameters with a fresh IV; returns (base64 ciphertext, IV).
  fn encrypt(cipher: &BarkCipher, params: &[(&str, Value)]) -> Option<(String, String)> {
    let mut random = [0u8; BARK_GCM_IV_RANDOM_BYTES];
    SystemRandom::new().fill(&mut random).ok()?;
    // Bark rebuilds the IV from these 12 ASCII characters
    let iv = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    let iv_bytes: [u8; 12] = iv.as_bytes().try_into().ok()?;
    let ciphertext = cipher.encrypt(&iv_bytes, ordered_json(params).as_bytes())?;
    Some((base64::engine::general_purpose::STANDARD.encode(ciphertext), iv))
  }
}

#[async_trait]
impl Notify for Bark {
  fn schemas(&self) -> &[&str] {
    &["bark", "barks"]
  }
  fn service_name(&self) -> &str {
    "Bark"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Bark devices to notify");
      return Ok(false);
    }

    let url = format!("{}://{}{}/push", if self.secure { "https" } else { "http" }, self.host, self.port.map(|p| format!(":{}", p)).unwrap_or_default());
    let params = self.parameters(ctx);
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    // Upstream pops targets from the end of its sorted list
    for target in self.targets.iter().rev() {
      // Skip a device that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let device_key = ("device_key", json!(target));
      let body = match &self.cipher {
        Some(cipher) => {
          // Fail closed; never fall back to plaintext
          let Some((ciphertext, iv)) = Self::encrypt(cipher, &params) else {
            tracing::warn!("Failed to encrypt Bark notification.");
            has_error = true;
            continue;
          };
          ordered_json(&[device_key, ("ciphertext", json!(ciphertext)), ("iv", json!(iv))])
        }
        None => {
          let mut payload = vec![device_key];
          payload.extend(params.iter().cloned());
          ordered_json(&payload)
        }
      };

      tracing::debug!("Bark POST URL: {} (cert_verify={}, encrypted={})", url, self.verify_certificate, self.cipher.is_some());
      let mut request = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json; charset=utf-8").body(body);
      if let Some(user) = &self.user {
        request = request.basic_auth(user, Some(self.password.as_deref().unwrap_or("")));
      }

      crate::notify::throttle().await;
      match request.send().await {
        Ok(resp) if resp.status().as_u16() == 200 => {
          tracing::info!("Sent Bark notification.");
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          let reason = resp.status().canonical_reason().unwrap_or("");
          tracing::warn!("Failed to send Bark notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, resp.status().as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Bark notification.");
          // The error text may echo request fragments once encryption is on
          if self.cipher.is_none() {
            tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          }
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
  use crate::types::NotifyType;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn bark(url: &str) -> Bark {
    Bark::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn mock_url(server: &MockServer, rest: &str) -> String {
    let addr = server.address();
    format!("bark://{}:{}/{}", addr.ip(), addr.port(), rest)
  }

  fn body_json(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "bark://localhost",
      "bark://192.168.0.6:8081/device_key",
      "bark://user@192.168.0.6:8081/device_key",
      "bark://192.168.0.6:8081/device_key/?sound=invalid",
      "bark://192.168.0.6:8081/device_key/?sound=alarm",
      "bark://192.168.0.6:8081/device_key/?sound=NOiR.cAf",
      "bark://192.168.0.6:8081/device_key/?badge=100",
      "barks://192.168.0.6:8081/device_key/?badge=invalid",
      "barks://192.168.0.6:8081/device_key/?badge=-12",
      "bark://192.168.0.6:8081/device_key/?category=apprise",
      "bark://192.168.0.6:8081/device_key/?image=no",
      "bark://192.168.0.6:8081/device_key/?group=apprise",
      "bark://192.168.0.6:8081/device_key/?level=invalid",
      "bark://192.168.0.6:8081/?to=device_key",
      "bark://192.168.0.6:8081/device_key/?click=http://localhost",
      "bark://192.168.0.6:8081/device_key/?level=active",
      "bark://192.168.0.6:8081/device_key/?level=critical",
      "bark://192.168.0.6:8081/device_key/?level=critical&volume=10",
      "bark://192.168.0.6:8081/device_key/?level=critical&volume=invalid",
      "bark://192.168.0.6:8081/device_key/?level=critical&volume=11",
      "bark://192.168.0.6:8081/device_key/?level=critical&volume=-1",
      "bark://192.168.0.6:8081/device_key/?level=critical&volume=",
      "bark://user:pass@192.168.0.5:8086/device_key/device_key2/",
      "barks://192.168.0.7/device_key/",
      "bark://192.168.0.7/device_key",
      "bark://192.168.0.6:8081/device_key/?icon=https://example.com/icon.png",
      "bark://192.168.0.6:8081/device_key/?icon=https://example.com/icon.png&image=no",
      "bark://192.168.0.6:8081/device_key/?call=1",
      "bark://192.168.0.6:8081/device_key/?call=1&sound=alarm&level=critical",
      "bark://192.168.0.6:8081/device_key/?format=markdown",
      "bark://192.168.0.6:8081/device_key/?format=text",
      "bark://localhost/device_key/?key=0123456789abcdef",
      "bark://localhost/device_key/?key=0123456789abcdef01234567",
      "bark://localhost/device_key/?key=0123456789abcdef0123456789abcdef",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "bark://",
      "bark://:@/",
      // Encryption keys must be 16, 24 or 32 ASCII characters
      "bark://localhost/device_key/?key=short",
      "bark://localhost/device_key/?key=0123456789abcdef0",
      "bark://localhost/device_key/?key=%C3%A9123456789abcde",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_verifies_host() {
    // Upstream parses this URL with host verification
    crate::notify::custom_json::assert_verifies_host("bark://{h}/device");
  }

  #[test]
  fn test_options() {
    let b = bark("bark://h/k?sound=NOiR.cAf&badge=100&volume=10&level=critical&call=yes");
    assert_eq!(b.sound, Some("noir.caf"));
    assert_eq!(b.badge, Some(100));
    assert_eq!(b.volume, Some(10));
    assert_eq!(b.level, Some("critical"));
    assert!(b.call);
    assert!(b.include_image);

    let b = bark("bark://h/k?sound=invalid&badge=-12&volume=11&level=invalid&image=no");
    assert_eq!((b.sound, b.badge, b.volume, b.level, b.include_image), (None, None, None, None, false));
    assert_eq!(bark("bark://h/k?sound=al").sound, Some("alarm.caf"));
    assert_eq!(bark("bark://h/k?level=t").level, Some("timeSensitive"));
    // Unrecognised image= values fall back to "no", as upstream parses them
    assert!(!bark("bark://h/k?image=random").include_image);
    // Targets are sorted and unique
    assert_eq!(bark("bark://h/b/a?to=c,a").targets, vec!["a", "b", "c"]);
    // A '+' in the key is kept
    assert!(bark("bark://h/k?key=0123456789abcde+").cipher.is_some());
  }

  #[tokio::test]
  async fn test_send_matches_upstream() {
    // pycapture.py for bark://user@host:8080/k2/k1?sound=NOiR&badge=3&volume=5&level=critical&click=http://x&category=c&group=g&call=yes
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/push"))
      .and(header("content-type", "application/json; charset=utf-8"))
      .and(header("authorization", "Basic dXNlcjo="))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let addr = server.address();
    let url = format!("bark://user@{}:{}/k2/k1?sound=NOiR&badge=3&volume=5&level=critical&click=http://x&category=c&group=g&call=yes", addr.ip(), addr.port());
    let svc = from_url(&url).unwrap();
    let ctx = NotifyContext { body: "B ü".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());

    let requests = push_requests(&server).await;
    let expected = |key: &str| {
      json!({
        "device_key": key, "title": "Apprise Notifications", "body": "B ü",
        "icon": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-128x128.png",
        "sound": "noir.caf", "url": "http://x", "badge": 3, "level": "critical", "category": "c", "group": "g", "volume": 5, "call": 1,
      })
    };
    assert_eq!(body_json(&requests[0]), expected("k2"));
    assert_eq!(body_json(&requests[1]), expected("k1"));
    // Keys keep upstream's order
    assert!(String::from_utf8_lossy(&requests[0].body).starts_with(r#"{"device_key":"k2","title":"Apprise Notifications","body":"#));
  }

  #[tokio::test]
  async fn test_send_markdown_and_icon() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/push"))
      .and(header("authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let addr = server.address();
    let svc = bark(&format!("bark://user:pass@{}:{}/k1?icon=http://i/x.png", addr.ip(), addr.port()));
    let ctx =
      NotifyContext { title: "T".into(), body: "**B**".into(), body_format: NotifyFormat::Markdown, notify_type: NotifyType::Failure, ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
    let requests = push_requests(&server).await;
    assert_eq!(body_json(&requests[0]), json!({"device_key": "k1", "title": "T", "markdown": "**B**", "icon": "http://i/x.png"}));
  }

  /// Decrypt a Bark request the way the app does.
  fn decrypt(key: &str, request: &wiremock::Request) -> (String, Value) {
    let payload = body_json(request);
    let iv = payload["iv"].as_str().unwrap();
    assert_eq!(iv.len(), 12);
    let ciphertext = base64::engine::general_purpose::STANDARD.decode(payload["ciphertext"].as_str().unwrap()).unwrap();
    let cipher = BarkCipher::new(key.as_bytes()).unwrap();
    let iv_bytes: [u8; 12] = iv.as_bytes().try_into().unwrap();
    let plain = match cipher {
      BarkCipher::Aes128(c) => c.decrypt((&iv_bytes).into(), ciphertext.as_slice()),
      BarkCipher::Aes192(c) => c.decrypt((&iv_bytes).into(), ciphertext.as_slice()),
      BarkCipher::Aes256(c) => c.decrypt((&iv_bytes).into(), ciphertext.as_slice()),
    }
    .unwrap();
    let plain = String::from_utf8(plain).unwrap();
    (plain.clone(), serde_json::from_str(&plain).unwrap())
  }

  #[tokio::test]
  async fn test_send_encrypted() {
    for key in ["0123456789abcdef", "0123456789abcdef01234567", "0123456789abcdef0123456789abcde+"] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/push")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
      let svc = from_url(&mock_url(&server, &format!("k1/k2?icon=http://i/x.png&key={}", key))).unwrap();
      let ctx = NotifyContext { title: "Secret".into(), body: "B ü".into(), ..Default::default() };
      assert!(svc.send(&ctx).await.unwrap());

      let requests = push_requests(&server).await;
      let mut ivs = Vec::new();
      for (request, device) in requests.iter().zip(["k2", "k1"]) {
        let payload = body_json(request);
        // Only the device key is readable
        assert_eq!(payload.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["ciphertext", "device_key", "iv"]);
        assert_eq!(payload["device_key"], device);
        let (plain, params) = decrypt(key, request);
        assert_eq!(params, json!({"title": "Secret", "body": "B ü", "icon": "http://i/x.png"}));
        // Compact JSON in upstream's order, non-ASCII kept
        assert_eq!(plain, r#"{"title":"Secret","body":"B ü","icon":"http://i/x.png"}"#);
        ivs.push(payload["iv"].as_str().unwrap().to_string());
      }
      // Every request gets a fresh IV
      assert_ne!(ivs[0], ivs[1]);
    }
  }

  #[test]
  fn test_decrypts_upstream_ciphertext() {
    // Produced by Apprise 2.1.0 for bark://host/k1?icon=http://i/x.png&key=0123456789abcdef
    let cipher = BarkCipher::new(b"0123456789abcdef").unwrap();
    let ciphertext = base64::engine::general_purpose::STANDARD
      .decode("c7Gwape4PlmwcHxNvWV+Paole7y2xkcESMZbCUAG7Y0Z3tPfr5Hm+5N+NmDYpqcnkripXGpKwK+vkkl+zT4szpctb7tLzkAu4GUQ37A2HMejq9nLM71d")
      .unwrap();
    let BarkCipher::Aes128(c) = cipher else { panic!() };
    let plain = c.decrypt(b"LutFoORuhIey".into(), ciphertext.as_slice()).unwrap();
    assert_eq!(String::from_utf8(plain).unwrap(), r#"{"title":"Apprise Notifications","body":"B ü","icon":"http://i/x.png"}"#);
    // Our encryption gives the same result for the same IV
    let params = vec![("title", json!("Apprise Notifications")), ("body", json!("B ü")), ("icon", json!("http://i/x.png"))];
    let ours = BarkCipher::new(b"0123456789abcdef").unwrap().encrypt(b"LutFoORuhIey", ordered_json(&params).as_bytes()).unwrap();
    assert_eq!(ours, ciphertext);
  }

  /// The pushes the mock received, in order, without stray requests.
  async fn push_requests(server: &MockServer) -> Vec<wiremock::Request> {
    crate::notify::custom_json::requests_to(server, "POST", "/push").await
  }

  #[tokio::test]
  async fn test_retry_skips_devices_that_accepted() {
    let server = MockServer::builder().start().await;
    let device = |key: &str| Mock::given(method("POST")).and(path("/push")).and(wiremock::matchers::body_partial_json(json!({"device_key": key})));
    device("k2").respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second device fails once, then accepts the retry
    device("k1").respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    device("k1").respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&mock_url(&server, "k1/k2?retry=1&wait=0")));
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(apprise.notify(&ctx).await.is_success());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    // No device keys
    assert!(!bark("bark://localhost").send(&ctx).await.unwrap());

    for status in [500, 999, 201] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/push")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url(&mock_url(&server, "a/b")).unwrap();
      assert!(!svc.send(&ctx).await.unwrap(), "status {}", status);
    }
  }

  #[test]
  fn test_service_details() {
    let details = Bark::static_details();
    assert_eq!(details.service_name, "Bark");
    assert_eq!(details.protocols, vec!["bark", "barks"]);
  }
}

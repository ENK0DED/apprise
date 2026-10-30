//! Firebase Cloud Messaging.
//!
//! `fcm://{project}/{targets}?keyfile={keyfile}` sends through the HTTP v1
//! API, authorised with an OAuth2 access token obtained from a Google
//! service account key (an RS256-signed JWT exchanged at its `token_uri`).
//! `fcm://{apikey}/{targets}` keeps the legacy server-key API upstream
//! still offers. Targets starting with `#` are topics.
//!
//! Upstream reloads the key file and fetches a new access token on every
//! notification; this port keeps the token in memory while it is valid and
//! the key file is unchanged.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::keyfile::{pem_blocks, read_source};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_bool_or, parse_list, prefixed_args, qsd_bool, split_path, unquote};

/// Largest key file accepted (`max_fcm_keyfile_size`).
const MAX_KEYFILE_SIZE: usize = 5000;

/// The token endpoint used when the key file names none.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// The OAuth2 scope FCM needs.
const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

/// Lifetime requested for the JWT assertion.
const ACCESS_TOKEN_LIFETIME_SEC: u64 = 3600;

/// Grace taken off every token lifetime (`GoogleOAuth.clock_skew`).
const CLOCK_SKEW: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FcmMode {
  Legacy,
  OAuth2,
}

impl FcmMode {
  fn as_str(&self) -> &'static str {
    match self {
      Self::Legacy => "legacy",
      Self::OAuth2 => "oauth2",
    }
  }
}

/// Upstream `FCM_PRIORITIES`, matched on the first two characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FcmPriority {
  Min,
  Low,
  Normal,
  High,
  Max,
}

impl FcmPriority {
  const ALL: [(&'static str, Self); 5] = [("min", Self::Min), ("low", Self::Low), ("normal", Self::Normal), ("high", Self::High), ("max", Self::Max)];

  fn parse(value: &str) -> Option<Self> {
    let prefix: String = value.chars().take(2).collect::<String>().to_lowercase();
    Self::ALL.iter().find(|(name, _)| name.starts_with(&prefix)).map(|(_, p)| *p)
  }

  /// The payload merged into every message (`FCMPriorityManager.payload()`).
  fn payload(&self, mode: FcmMode) -> Value {
    let high = matches!(self, Self::High | Self::Max);
    match mode {
      FcmMode::Legacy => json!({ "priority": if high { "high" } else { "normal" } }),
      FcmMode::OAuth2 => {
        let urgency = match self {
          Self::Min => "very-low",
          Self::Low => "low",
          Self::Normal => "normal",
          Self::High | Self::Max => "high",
        };
        // apns-push-type keeps iOS delivering high priority alerts (upstream ab293c79)
        let apns = if high { json!({ "apns-priority": "10", "apns-push-type": "alert" }) } else { json!({ "apns-priority": "5" }) };
        json!({ "message": {
          "android": { "priority": if high { "HIGH" } else { "NORMAL" } },
          "apns": { "headers": apns },
          "webpush": { "headers": { "Urgency": urgency } },
        }})
      }
    }
  }
}

/// `?color=`: off, the Apprise colour of the notification type, or a fixed
/// `rrggbb` (`FCMColorManager`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FcmColor {
  Off,
  Asset,
  Rgb(String),
}

impl FcmColor {
  fn parse(value: Option<&str>) -> Self {
    let Some(value) = value else { return Self::Off };
    // `#?rrggbb` or `#?rgb`, matched at the start like upstream's re.match
    let hex: Vec<char> = value.strip_prefix('#').unwrap_or(value).chars().take_while(char::is_ascii_hexdigit).take(6).collect();
    if hex.len() == 6 {
      return Self::Rgb(hex.iter().collect::<String>().to_lowercase());
    }
    if hex.len() >= 3 {
      return Self::Rgb(hex[..3].iter().flat_map(|c| [*c, *c]).collect::<String>().to_lowercase());
    }
    if parse_bool_or(Some(value), false) { Self::Asset } else { Self::Off }
  }

  fn get(&self, notify_type: &NotifyType) -> Option<String> {
    match self {
      Self::Off => None,
      Self::Asset => Some(notify_type.color_hex()),
      Self::Rgb(rgb) => Some(format!("#{}", rgb)),
    }
  }
}

/// A Google service account key file (`GoogleOAuth.load()`).
struct ServiceAccount {
  client_email: String,
  private_key_id: String,
  project_id: String,
  token_uri: String,
  key: RsaKeyPair,
}

impl ServiceAccount {
  fn parse(data: &[u8]) -> Option<Self> {
    let content: Value = serde_json::from_slice(data).ok().filter(Value::is_object).or_else(|| {
      tracing::debug!("FCM keyfile is not a JSON object");
      None
    })?;
    let field = |k: &str| content.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let (Some(client_email), Some(private_key_id), Some(private_key), Some(kind), Some(project_id)) =
      (field("client_email"), field("private_key_id"), field("private_key"), field("type"), field("project_id"))
    else {
      tracing::debug!("FCM keyfile is missing required information");
      return None;
    };
    if kind != "service_account" {
      tracing::debug!("FCM keyfile is not of type service_account");
      return None;
    }

    // An RSA key in PKCS#8 (as Google issues them) or PKCS#1 PEM
    let key = pem_blocks(&private_key).into_iter().find_map(|(label, der)| match label.as_str() {
      "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der).ok(),
      "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der).ok(),
      _ => None,
    });
    let Some(key) = key else {
      tracing::error!("FCM provided private key is invalid.");
      return None;
    };

    let token_uri = content.get("token_uri").and_then(Value::as_str).unwrap_or(DEFAULT_TOKEN_URI).to_string();
    Some(Self { client_email, private_key_id, project_id, token_uri, key })
  }

  /// The signed JWT bearer assertion. The JSON keeps Python's default
  /// `json.dumps()` spacing, as upstream sends it.
  fn assertion(&self, now: u64) -> Option<String> {
    let s = |v: &str| serde_json::to_string(v).ok();
    let header = format!(r#"{{"typ": "JWT", "alg": "RS256", "kid": {}}}"#, s(&self.private_key_id)?);
    let payload = format!(
      r#"{{"iat": {}, "exp": {}, "iss": {}, "aud": {}, "scope": {}}}"#,
      now,
      now + ACCESS_TOKEN_LIFETIME_SEC,
      s(&self.client_email)?,
      s(&self.token_uri)?,
      s(FCM_SCOPE)?
    );
    let b64 = |d: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d);
    let signing_input = format!("{}.{}", b64(header.as_bytes()), b64(payload.as_bytes()));
    let mut signature = vec![0u8; self.key.public().modulus_len()];
    self.key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature).ok()?;
    Some(format!("{}.{}", signing_input, b64(&signature)))
  }
}

/// An access token and the key file content it was issued for.
struct CachedToken {
  keyfile: Vec<u8>,
  token: String,
  expires: Instant,
}

pub struct Fcm {
  mode: FcmMode,
  apikey: Option<String>,
  project: Option<String>,
  keyfile: Option<String>,
  targets: Vec<String>,
  data: Vec<(String, String)>,
  include_image: bool,
  image_src: Option<String>,
  priority: Option<FcmPriority>,
  color: FcmColor,
  verify_certificate: bool,
  tags: Vec<String>,
  token: Mutex<Option<CachedToken>>,
}

/// Upstream `dict_full_update()`: merge `update` into `base`, recursing into
/// objects present in both.
fn merge(base: &mut Value, update: &Value) {
  if let (Some(base), Some(update)) = (base.as_object_mut(), update.as_object()) {
    for (k, v) in update {
      match base.get_mut(k) {
        Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
        _ => {
          base.insert(k.clone(), v.clone());
        }
      }
    }
  }
}

impl Fcm {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.as_deref().map(unquote).unwrap_or_default();
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(str::to_string);
    let apikey = qsd("apikey").unwrap_or_else(|| host.clone());
    let project = qsd("project").unwrap_or(host);
    let keyfile = qsd("keyfile");

    let mode = match url.get("mode") {
      None => {
        if keyfile.is_some() {
          FcmMode::OAuth2
        } else {
          FcmMode::Legacy
        }
      }
      Some(m) => match m.to_lowercase().as_str() {
        "legacy" => FcmMode::Legacy,
        "oauth2" => FcmMode::OAuth2,
        _ => {
          tracing::warn!("The FCM mode specified ({}) is invalid.", m);
          return None;
        }
      },
    };

    // Upstream validate_regex(): the first non-whitespace run
    let token = |v: &str| v.split_whitespace().next().map(str::to_string);
    let (apikey, project) = match mode {
      FcmMode::OAuth2 => {
        let Some(project) = token(&project) else {
          tracing::warn!("An invalid FCM Project ID was specified.");
          return None;
        };
        if keyfile.is_none() {
          tracing::warn!("No FCM JSON KeyFile was specified.");
          return None;
        }
        (None, Some(project))
      }
      FcmMode::Legacy => {
        let Some(apikey) = token(&apikey) else {
          tracing::warn!("An invalid FCM API key was specified.");
          return None;
        };
        (Some(apikey), None)
      }
    };

    let mut targets = split_path(url);
    if let Some(to) = qsd("to") {
      targets.push(to);
    }
    let targets = parse_list(targets, true, true);

    let priority = match qsd("priority") {
      None => None,
      Some(p) => match FcmPriority::parse(&p) {
        Some(p) => Some(p),
        None => {
          tracing::warn!("An invalid FCM Priority ({}) was specified.", p);
          return None;
        }
      },
    };

    // A custom image turns images on unless image= says otherwise
    let image_src = url.get("image_url").map(str::to_string);
    let include_image = qsd_bool(url, "image", image_src.is_some());

    Some(Self {
      mode,
      apikey,
      project,
      keyfile: if mode == FcmMode::OAuth2 { keyfile } else { None },
      targets,
      data: prefixed_args(url, '+'),
      include_image,
      image_src,
      priority,
      color: FcmColor::parse(qsd("color").as_deref()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      token: Mutex::new(None),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Firebase Cloud Messaging",
      service_url: Some("https://firebase.google.com"),
      setup_url: Some("https://appriseit.com/services/fcm/"),
      protocols: vec!["fcm"],
      description: "Send push notifications via Google FCM.",
      attachment_support: false,
    }
  }

  /// An OAuth2 access token for the key file (`NotifyFCM.access_token`),
  /// or `None` when it could not be obtained.
  async fn access_token(&self, client: &reqwest::Client) -> Option<String> {
    let source = self.keyfile.as_deref()?;
    let Some((data, _)) = read_source(source, MAX_KEYFILE_SIZE).await else {
      tracing::error!("Could not access FCM keyfile.");
      return None;
    };

    let mut cached = self.token.lock().await;
    if let Some(token) = cached.as_ref()
      && token.keyfile == data
      && token.expires > Instant::now()
    {
      return Some(token.token.clone());
    }

    let Some(account) = ServiceAccount::parse(&data) else {
      tracing::error!("FCM keyfile could not be loaded.");
      return None;
    };
    if self.project.as_deref() != Some(account.project_id.as_str()) {
      tracing::error!("FCM keyfile identifies itself for a different project");
      return None;
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let requested = Instant::now();
    let assertion = account.assertion(now)?;

    tracing::info!("Refreshing FCM Access Token");
    let form = [("assertion", assertion.as_str()), ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")];
    let resp = match client.post(&account.token_uri).header("User-Agent", APP_ID).form(&form).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred refreshing FCM Access Token.");
        tracing::debug!("Socket Exception: {}", e);
        return None;
      }
    };
    if resp.status().as_u16() != 200 {
      tracing::warn!("Failed to update FCM Access Token error={}.", resp.status().as_u16());
      return None;
    }

    let response: Value = resp.json().await.ok()?;
    let Some(token) = response.get("access_token").and_then(Value::as_str).map(str::to_string) else {
      tracing::warn!("FCM token response holds no access_token.");
      return None;
    };
    let expires = match response.get("expires_in").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))) {
      Some(secs) => Instant::now() + Duration::from_secs(secs),
      None => requested + Duration::from_secs(ACCESS_TOKEN_LIFETIME_SEC),
    }
    .checked_sub(CLOCK_SKEW)
    .unwrap_or_else(Instant::now);

    tracing::debug!("FCM Access Token successfully acquired");
    *cached = Some(CachedToken { keyfile: data, token: token.clone(), expires });
    Some(token)
  }

  fn payload(&self, ctx: &NotifyContext, recipient: &str, image: Option<&str>) -> Value {
    let color = self.color.get(&ctx.notify_type);
    let image = image.filter(|_| self.include_image);
    let data: serde_json::Map<String, Value> = self.data.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();

    let mut payload = match self.mode {
      FcmMode::OAuth2 => {
        let mut message = json!({
          "token": Value::Null,
          "notification": { "title": ctx.title, "body": ctx.body },
        });
        if let Some(color) = color {
          message["android"] = json!({ "notification": { "color": color } });
        }
        if let Some(image) = image {
          message["notification"]["image"] = json!(image);
        }
        if !data.is_empty() {
          message["data"] = Value::Object(data);
        }
        match recipient.strip_prefix('#') {
          Some(topic) => message["topic"] = json!(topic),
          None => message["token"] = json!(recipient),
        }
        json!({ "message": message })
      }
      FcmMode::Legacy => {
        let mut payload = json!({ "notification": { "notification": { "title": ctx.title, "body": ctx.body } } });
        if let Some(color) = color {
          payload["notification"]["notification"]["color"] = json!(color);
        }
        if let Some(image) = image {
          payload["notification"]["notification"]["image"] = json!(image);
        }
        if !data.is_empty() {
          payload["data"] = Value::Object(data);
        }
        // Upstream keeps the '#' in a legacy topic
        payload["to"] = json!(if recipient.starts_with('#') { format!("/topics/{}", recipient) } else { recipient.to_string() });
        payload
      }
    };

    if let Some(priority) = self.priority {
      merge(&mut payload, &priority.payload(self.mode));
    }
    payload
  }
}

#[async_trait]
impl Notify for Fcm {
  fn schemas(&self) -> &[&str] {
    &["fcm"]
  }
  fn service_name(&self) -> &str {
    "Firebase Cloud Messaging"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1024
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no FCM devices or topics to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let base = service_base("https://fcm.googleapis.com");
    let (url, authorization) = match self.mode {
      FcmMode::OAuth2 => {
        let Some(token) = self.access_token(&client).await else { return Ok(false) };
        (format!("{}/v1/projects/{}/messages:send", base, self.project.as_deref().unwrap_or_default()), format!("Bearer {}", token))
      }
      FcmMode::Legacy => (format!("{}/fcm/send", base), format!("key={}", self.apikey.as_deref().unwrap_or_default())),
    };

    let image = self.image_src.clone().or_else(|| ctx.image_url(crate::asset::NotifyImageSize::Xy256));

    let mut has_error = false;
    for recipient in &self.targets {
      let payload = self.payload(ctx, recipient, image.as_deref());
      tracing::debug!("FCM {} POST URL: {} (cert_verify={})", self.mode.as_str(), url, self.verify_certificate);

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Authorization", &authorization)
        .body(serde_json::to_vec(&payload)?)
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 204) => {
          tracing::info!("Sent {} FCM notification.", self.mode.as_str());
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          let reason = match status {
            400 => "A bad request was made to the server.",
            401 => "The provided API Key was not valid.",
            404 => "The token could not be registered.",
            _ => resp.status().canonical_reason().unwrap_or(""),
          };
          tracing::warn!("Failed to send {} FCM notification: {}{}error={}.", self.mode.as_str(), reason, if reason.is_empty() { "" } else { ", " }, status);
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending FCM notification.");
          tracing::debug!("Socket Exception: {}", e);
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
  use wiremock::matchers::{body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_fcm(url: &str) -> Fcm {
    Fcm::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn body_json(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
  }

  /// A service account key file for `project` holding [`TEST_RSA_KEY`].
  fn keyfile(dir: &tempfile::TempDir, project: &str, token_uri: &str) -> String {
    let content = json!({
      "type": "service_account",
      "project_id": project,
      "private_key_id": "kid123",
      "private_key": TEST_RSA_KEY,
      "client_email": "svc@apprise-demo.iam.gserviceaccount.com",
      "token_uri": token_uri,
    });
    let path = dir.path().join("keyfile.json");
    std::fs::write(&path, content.to_string()).unwrap();
    path.to_string_lossy().into_owned()
  }

  /// An RSA-2048 key generated for these tests only.
  const TEST_RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC+HJVpzyNvA4qr
3cszIeottDI/MABfAWjTLFH1VQkA2sjUDDnW+2CbTnsWioelaPRG9AHfawNrAryR
3AZr0Tn2o10yPcnwfS4+N3BjrhjOsy7d9p/5HAv/Bh0tGjdA3yUdSmNLRFMJlGzC
oNTa1eyJgzQm+BuoJOVdzfKatuYJ5Bso9FxkICDbOkeewIloJwEajY3vkcVCcc9P
vntPLbrO4+5rLpG68qME1Ac0vaqKSp/zRVS1YZOnwRHqWIQQPFBPB2FIKRojEtHV
62xHpCuK0MrSa+n8O5s2IvbsfchEJK7yo+wyFFl20W+rDLj30U0Q9bEHBWj7XFdo
Wi1l/MuzAgMBAAECggEAWYDujSqixYS22DRrEs8wR8Uy9NG+EvyAZA9VJLocq4TV
kROSdZbSxDueMp4Uv7jabL87VHq22L4olkku0rrl3arK2TkC5lChIVSLerp/3my4
r3KJnUfpk+7GdLIHemi0OgaIGer4ZyW0UKtUmxSlMJaPSpt7td1tCQ1JXzKludiU
zR3DxdcqiO5+FPX0zQnBCSdS8t1BXRfFiEAymMX/5ai/VoQJf6N5SB+4P2CzJYe7
Bpcwpjp4M35bapCjNUGzzD3m6jh8QhY05zjcgsUluD8tt1w/AjxlmA+5NFzRKd3k
nrCR+PM3EFNiSqNBdJWdtBJgaLA3sMwDpTh+BRIVHQKBgQDvgR+7la6Co4CgD8r8
taS+ep8f4xmSKlfc9e/KifJH0ePInJ+Ka5MrhhnqjK34pnA3xqMNMP6Y03PMQHHG
tjp/RpoNbT4I76Of0n/rsgeEwejr5GlYlCBPegw35fnBV+MPOut2sZxrMKlxqCUR
evK3IME7eqm0+X0g/TFTx5UUZQKBgQDLNJSYz7a5j0VWs0mM696mpVrIpoSzh2eS
i4q5AnWNPAtPs07nrz68WeA2fhhxocj8pYk9vFL2iCF01TLlRxUV/FQP1uQPf9qu
5GdNVB0xZk0B6vpQRWrpQxUp1QUOx1Nt7Cfy3HOh/sPazHWk8SF1CgMYpnnkj6/T
H0lUs0AiNwKBgCC1K5Y8jMZ+ebRiM8BrNgJlUJHUO8vASJt+Yq7wQ8wXF9pvuUH4
CvMoryVe7OtGH5EeaWoFxN8uEWRVcLRIcpirFaK7xnsF7vyGjL2tZGV27c45NUHJ
iNgRvNVtbLxAult8GmftPYVN8fX5loZamgb0TC9JQx/Z6oK00SW13eAZAoGAKqPF
6qrrdNSTgDXu4Rs9hUp9Wk1W2zU2jpGpclkJwbEvznGZxRQI3g0d91FOX8nLGvjB
a6rw3bqDCrkSDPp1ej+QhCJbiMUTZk+XParA1XTaA+UC0YU/geMJtdV1UjyRTSeY
2mVX/k4vWuI8bXgLxTtsmueT/FoKUwrloFhcxNECgYEAm1zkbO+Vpp2tTPhR5Uzr
OFujmmDouVGTCeWTGi+Cuzy0fNco3Kq7UIrJCpFds/Uop7Uv5rkkyf/IZ0ClOo+7
ORhLPuBVA+eLw73wx7FgbyPuxxVY6JN1M7xsFy4GSXFH4Ub3TV67PsgUVr7H7Btw
4FZa07+PLKPMmbfX+5l6F5s=
-----END PRIVATE KEY-----
";

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "fcm://apikey/",
      "fcm://apikey/device",
      "fcm://apikey/#topic",
      "fcm://apikey/#topic1/device/%20/",
      "fcm://apikey?to=#topic1,device",
      "fcm://?apikey=abc123&to=device",
      "fcm://?apikey=abc123&to=device&image=yes",
      "fcm://?apikey=abc123&to=device&color=no",
      "fcm://?apikey=abc123&to=device&color=aabbcc",
      "fcm://?apikey=abc123&to=device&image_url=http://example.com/interesting.jpg",
      "fcm://?apikey=abc123&to=device&image_url=http://example.com/interesting.jpg&image=no",
      "fcm://?apikey=abc123&to=device&+key=value&+key2=value2",
      "fcm://project_id?to=device&keyfile=/invalid/path",
      "fcm://?to=device&project=project_id&keyfile=/invalid/path",
      "fcm://project_id?to=device&mode=oauth2&keyfile=/invalid/path",
      "fcm://apikey/#topic1/device/?mode=legacy",
      "fcm://apikey/device?priority=max",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "fcm://",
      "fcm://:@/",
      "fcm://project@%20%20/",
      "fcm://apikey/device?mode=invalid",
      "fcm://apikey/device?mode=",
      "fcm://%20?to=device&keyfile=/invalid/path",
      "fcm://project_id?to=device&mode=oauth2",
      "fcm://apikey/device?priority=invalid",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let f = parse_fcm("fcm://apikey/device");
    assert_eq!(f.mode, FcmMode::Legacy);
    assert_eq!(f.apikey.as_deref(), Some("apikey"));
    assert_eq!(f.targets, vec!["device"]);
    assert_eq!(f.priority, None);
    assert_eq!(f.color, FcmColor::Off);
    assert!(!f.include_image);

    // Targets are sorted and unique
    let f = parse_fcm("fcm://apikey/device/%23topic1/device?to=b,%23topic1");
    assert_eq!(f.targets, vec!["#topic1", "b", "device"]);

    let f = parse_fcm("fcm://?apikey=abc123&to=device&project=myproject");
    assert_eq!(f.apikey.as_deref(), Some("abc123"));
    assert_eq!(f.project, None);

    // A key file selects OAuth2 and the host is the project
    let f = parse_fcm("fcm://myproject/device?keyfile=/tmp/x.json");
    assert_eq!(f.mode, FcmMode::OAuth2);
    assert_eq!(f.project.as_deref(), Some("myproject"));
    let f = parse_fcm("fcm://ignored/device?keyfile=/tmp/x.json&project=other");
    assert_eq!(f.project.as_deref(), Some("other"));

    // image_url turns images on unless image= is given
    assert!(parse_fcm("fcm://k/d?image_url=http://x/i.png").include_image);
    assert!(!parse_fcm("fcm://k/d?image_url=http://x/i.png&image=no").include_image);
    assert!(parse_fcm("fcm://k/d?image=yes").include_image);
  }

  #[test]
  fn test_data_args() {
    let f = parse_fcm("fcm://apikey/device?+key1=val1&+Key2=Val+2&%2Bkey3=a%20b&-other=x");
    assert_eq!(f.data, vec![("key1".to_string(), "val1".to_string()), ("Key2".to_string(), "Val+2".to_string()), ("key3".to_string(), "a b".to_string())]);
  }

  #[test]
  fn test_priorities() {
    let p = |v: &str| FcmPriority::parse(v);
    assert_eq!(p("min"), Some(FcmPriority::Min));
    assert_eq!(p("m"), Some(FcmPriority::Min));
    assert_eq!(p("MAX"), Some(FcmPriority::Max));
    assert_eq!(p("lo"), Some(FcmPriority::Low));
    assert_eq!(p("normal"), Some(FcmPriority::Normal));
    assert_eq!(p("h"), Some(FcmPriority::High));
    assert_eq!(p("x"), None);

    assert_eq!(FcmPriority::Low.payload(FcmMode::Legacy), json!({"priority": "normal"}));
    assert_eq!(FcmPriority::Max.payload(FcmMode::Legacy), json!({"priority": "high"}));
    assert_eq!(
      FcmPriority::Min.payload(FcmMode::OAuth2),
      json!({"message": {"android": {"priority": "NORMAL"}, "apns": {"headers": {"apns-priority": "5"}}, "webpush": {"headers": {"Urgency": "very-low"}}}})
    );
    assert_eq!(
      FcmPriority::High.payload(FcmMode::OAuth2),
      json!({"message": {"android": {"priority": "HIGH"}, "apns": {"headers": {"apns-priority": "10", "apns-push-type": "alert"}}, "webpush": {"headers": {"Urgency": "high"}}}})
    );
  }

  #[test]
  fn test_colors() {
    assert_eq!(FcmColor::parse(None), FcmColor::Off);
    assert_eq!(FcmColor::parse(Some("yes")), FcmColor::Asset);
    assert_eq!(FcmColor::parse(Some("no")), FcmColor::Off);
    assert_eq!(FcmColor::parse(Some("invalid")), FcmColor::Off);
    assert_eq!(FcmColor::parse(Some("AA001b")), FcmColor::Rgb("aa001b".into()));
    assert_eq!(FcmColor::parse(Some("#12AAbb")), FcmColor::Rgb("12aabb".into()));
    assert_eq!(FcmColor::parse(Some("#f0A")), FcmColor::Rgb("ff00aa".into()));
    assert_eq!(FcmColor::parse(Some("abcd")), FcmColor::Rgb("aabbcc".into()));
    assert_eq!(FcmColor::Asset.get(&NotifyType::Warning).as_deref(), Some("#CACF29"));
  }

  #[test]
  fn test_service_account() {
    let good = json!({"type": "service_account", "project_id": "p", "private_key_id": "k", "private_key": TEST_RSA_KEY, "client_email": "e@x"});
    let account = ServiceAccount::parse(good.to_string().as_bytes()).unwrap();
    assert_eq!(account.token_uri, DEFAULT_TOKEN_URI);

    let mut wrong_type = good.clone();
    wrong_type["type"] = json!("user");
    assert!(ServiceAccount::parse(wrong_type.to_string().as_bytes()).is_none());
    for missing in ["client_email", "private_key_id", "private_key", "type", "project_id"] {
      let mut entry = good.clone();
      entry[missing] = json!("");
      assert!(ServiceAccount::parse(entry.to_string().as_bytes()).is_none(), "{}", missing);
    }
    let mut bad_key = good.clone();
    bad_key["private_key"] = json!("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n");
    assert!(ServiceAccount::parse(bad_key.to_string().as_bytes()).is_none());
    assert!(ServiceAccount::parse(b"[]").is_none());
    assert!(ServiceAccount::parse(b"{").is_none());
  }

  #[tokio::test]
  async fn test_legacy_send() {
    // Matches pycapture.py for this URL (targets sorted, '#' kept in topics)
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/fcm/send"))
      .and(header("authorization", "key=apikey"))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url("fcm://apikey/device1/%23topic1?color=yes&priority=high&image=yes&+key1=val1&+Key2=Val+2").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let image = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-256x256.png";
    let expected = |to: &str| {
      json!({
        "notification": {"notification": {"title": "T", "body": "B", "color": "#CACF29", "image": image}},
        "data": {"key1": "val1", "Key2": "Val+2"},
        "to": to,
        "priority": "high",
      })
    };
    assert_eq!(body_json(&requests[0]), expected("/topics/#topic1"));
    assert_eq!(body_json(&requests[1]), expected("device1"));
    assert_eq!(requests[0].headers.get("content-type").unwrap(), "application/json");
  }

  #[tokio::test]
  async fn test_legacy_default_payload() {
    // apprise-go parity golden "legacy-basic"
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/fcm/send")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("fcm://apikey/device-token").unwrap();
    let ctx = NotifyContext { title: "apprise parity".into(), body: "hello from python".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
      body_json(&requests[0]),
      json!({"notification": {"notification": {"title": "apprise parity", "body": "hello from python"}}, "to": "device-token"})
    );
  }

  #[tokio::test]
  async fn test_oauth2_send() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/token"))
      .and(header("content-type", "application/x-www-form-urlencoded"))
      .and(body_string_contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "tok123", "expires_in": 3600})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1/projects/apprise-demo/messages:send"))
      .and(header("authorization", "Bearer tok123"))
      .respond_with(ResponseTemplate::new(200))
      .expect(4)
      .mount(&server)
      .await;

    let dir = tempfile::tempdir().unwrap();
    let keyfile = keyfile(&dir, "apprise-demo", &format!("{}/token", server.uri()));
    let svc =
      from_url(&format!("fcm://apprise-demo/device1/%23topic1?keyfile={}&color=%23f0A&priority=max&image_url=https://ex.com/i.png&+key1=val1", keyfile))
        .unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // The token is reused while valid
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    // The assertion is an RS256 JWT for the service account
    let form = String::from_utf8(requests[0].body.clone()).unwrap();
    let assertion = form.strip_prefix("assertion=").unwrap().split('&').next().unwrap();
    let parts: Vec<&str> = assertion.split('.').collect();
    let decode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    assert_eq!(decode(parts[0]), br#"{"typ": "JWT", "alg": "RS256", "kid": "kid123"}"#);
    let claims: Value = serde_json::from_slice(&decode(parts[1])).unwrap();
    assert_eq!(claims["iss"], "svc@apprise-demo.iam.gserviceaccount.com");
    assert_eq!(claims["aud"], format!("{}/token", server.uri()));
    assert_eq!(claims["scope"], FCM_SCOPE);
    assert_eq!(claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(), 3600);
    let public_key = ServiceAccount::parse(std::fs::read(&keyfile).unwrap().as_slice()).unwrap().key.public().as_ref().to_vec();
    ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, public_key)
      .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &decode(parts[2]))
      .expect("valid RS256 signature");

    // Messages match pycapture.py for this URL
    let message = |target: (&str, &str)| {
      let mut message = json!({
        "token": Value::Null,
        "notification": {"title": "T", "body": "B", "image": "https://ex.com/i.png"},
        "android": {"notification": {"color": "#ff00aa"}, "priority": "HIGH"},
        "data": {"key1": "val1"},
        "apns": {"headers": {"apns-priority": "10", "apns-push-type": "alert"}},
        "webpush": {"headers": {"Urgency": "high"}},
      });
      message[target.0] = json!(target.1);
      json!({ "message": message })
    };
    assert_eq!(body_json(&requests[1]), message(("topic", "topic1")));
    assert_eq!(body_json(&requests[2]), message(("token", "device1")));
  }

  #[tokio::test]
  async fn test_oauth2_failures() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(400)).mount(&server).await;
    Mock::given(method("POST")).and(path("/badjson")).respond_with(ResponseTemplate::new(200).set_body_string("{")).mount(&server).await;
    Mock::given(method("POST")).and(path("/notoken")).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    let dir = tempfile::tempdir().unwrap();

    for token_path in ["/token", "/badjson", "/notoken"] {
      let keyfile = keyfile(&dir, "apprise-demo", &format!("{}{}", server.uri(), token_path));
      let svc = from_url(&format!("fcm://apprise-demo/device?keyfile={}", keyfile)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "{}", token_path);
    }

    // A key file for another project, or one that cannot be read
    let keyfile = keyfile(&dir, "other-project", &format!("{}/token", server.uri()));
    let svc = from_url(&format!("fcm://apprise-demo/device?keyfile={}", keyfile)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let svc = from_url("fcm://apprise-demo/device?keyfile=/invalid/path").unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    // No targets
    assert!(!from_url("fcm://apikey/").unwrap().send(&ctx).await.unwrap());

    for status in [400, 401, 404, 500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url("fcm://apikey/a/b").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }

    // 204 is accepted
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), from_url("fcm://apikey/a").unwrap().send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_service_details() {
    let details = Fcm::static_details();
    assert_eq!(details.service_name, "Firebase Cloud Messaging");
    assert_eq!(details.protocols, vec!["fcm"]);
    assert_eq!(from_url("fcm://apikey/device").unwrap().body_maxlen(), 1024);
  }
}

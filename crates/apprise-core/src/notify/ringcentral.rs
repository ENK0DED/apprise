use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};

/// Access token lifetime requested by the password grant (60 minutes).
const ACCESS_TOKEN_TTL: u32 = 3600;

/// Refresh token lifetime requested by the password grant (1 week).
const REFRESH_TOKEN_TTL: u32 = 604800;

/// Tokens longer than this are taken to be JWTs when no ?mode= is given.
const JWT_DETECT_LEN: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
  /// Username (source phone) + password grant
  Basic,
  /// JWT bearer grant
  Jwt,
}

impl AuthMode {
  fn as_str(&self) -> &'static str {
    match self {
      Self::Basic => "basic",
      Self::Jwt => "jwt",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Environment {
  Production,
  Sandbox,
}

impl Environment {
  /// The infix used in the API host name.
  fn host_suffix(&self) -> &'static str {
    match self {
      Self::Production => "",
      Self::Sandbox => ".devtest",
    }
  }
}

/// Upstream matches a choice whose name starts with the given value, so
/// `j` selects `jwt` and `sand` selects `sandbox`.
fn match_prefix<T: Copy>(value: &str, choices: &[(&str, T)]) -> Option<T> {
  let value = value.trim().to_lowercase();
  if value.is_empty() {
    return None;
  }
  choices.iter().find(|(name, _)| name.starts_with(&value)).map(|(_, choice)| *choice)
}

/// `^[a-z0-9_-]+$` (case-insensitive)
fn is_client_token(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `^[a-z0-9._-]+$` (case-insensitive)
fn is_jwt(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// RingCentral SMS/MMS
/// (`ringc://{from_phone}:{token}@{client_id}/{secret}/{targets}`).
pub struct RingCentral {
  client_id: String,
  client_secret: String,
  /// JWT, or the user password in BASIC mode
  token: String,
  mode: AuthMode,
  environment: Environment,
  /// Source phone number (digits only)
  source: String,
  /// Target phone numbers (digits only)
  targets: Vec<String>,
  /// Access token and its expiry, kept for later sends
  access_token: Mutex<Option<(String, Instant)>>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl RingCentral {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let mut entries = url.path_parts.clone();
    let client_id = url.host.clone().unwrap_or_default();
    // Upstream unquotes these query values once more
    let qsd = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);

    // The first path entry is the client secret
    let mut client_secret = if entries.is_empty() { None } else { Some(entries.remove(0)) };

    let (mut source, mut token) = match url.password.as_deref().filter(|p| !p.is_empty()) {
      // ringc://source_phone:token@client_id/secret[/targets]
      Some(password) => (url.user.clone(), password.to_string()),
      // ringc://token@client_id/secret/source_phone[/targets]
      None => {
        let source = if entries.is_empty() { None } else { Some(entries.remove(0)) };
        (source, url.user.clone().unwrap_or_default())
      }
    };

    // ?token= is applied before the mode is detected so that a JWT given
    // this way is recognised
    if let Some(t) = qsd("token") {
      token = t;
    }

    let mode = match qsd("mode") {
      Some(requested) => match match_prefix(&requested, &[("basic", AuthMode::Basic), ("jwt", AuthMode::Jwt)]) {
        Some(mode) => mode,
        None => {
          tracing::warn!("An invalid RingCentral Authentication Mode ({}) was specified.", requested);
          return None;
        }
      },
      // JWT tokens are significantly longer than passwords
      None if token.chars().count() > JWT_DETECT_LEN => AuthMode::Jwt,
      None => AuthMode::Basic,
    };

    if let Some(secret) = qsd("secret") {
      client_secret = Some(secret);
    }
    if let Some(from) = qsd("from") {
      source = Some(from);
    }
    if let Some(from) = qsd("source") {
      source = Some(from);
    }

    let mut raw_targets = entries;
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    if !is_client_token(&client_id) {
      tracing::warn!("An invalid RingCentral Client ID was specified.");
      return None;
    }

    let Some(client_secret) = client_secret.filter(|s| is_client_token(s)) else {
      tracing::warn!("An invalid RingCentral Client Secret was specified.");
      return None;
    };

    // Only a JWT is validated; a BASIC mode password is taken as-is
    if mode == AuthMode::Jwt && !is_jwt(&token) {
      tracing::warn!("An invalid RingCentral JWT Token was specified.");
      return None;
    }

    let Some(source) = source.as_deref().and_then(|s| is_phone_no(s, 10)).map(|p| p.full) else {
      tracing::warn!("The RingCentral source (From) phone # ({}) is invalid.", source.unwrap_or_default());
      return None;
    };

    let environment = match qsd("env") {
      Some(requested) => match match_prefix(&requested, &[("prod", Environment::Production), ("sandbox", Environment::Sandbox)]) {
        Some(environment) => environment,
        None => {
          tracing::warn!("An invalid RingCentral environment ({}) was specified.", requested);
          return None;
        }
      },
      None => Environment::Production,
    };

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid RingCentral phone # ({}) specified.", target),
      }
    }

    Some(Self {
      client_id,
      client_secret,
      token,
      mode,
      environment,
      source,
      targets,
      access_token: Mutex::new(None),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "RingCentral",
      service_url: Some("https://ringcentral.com/"),
      setup_url: Some("https://appriseit.com/services/ringcentral/"),
      protocols: vec!["ringc"],
      description: "Send SMS (or MMS with attachments) via RingCentral.",
      attachment_support: true,
    }
  }

  /// The API host for the configured environment.
  fn platform_url(&self) -> String {
    format!("https://platform{}.ringcentral.com", self.environment.host_suffix())
  }

  /// Return a valid access token, authenticating with the OAuth token
  /// endpoint when none is cached or the cached one has expired.
  async fn login(&self, client: &reqwest::Client, base: &str) -> Option<String> {
    if let Some((token, expires)) = self.access_token.lock().unwrap().as_ref()
      && Instant::now() <= *expires
    {
      return Some(token.clone());
    }

    let ttl = ACCESS_TOKEN_TTL.to_string();
    let refresh_ttl = REFRESH_TOKEN_TTL.to_string();
    let username = format!("+{}", self.source);
    let form: Vec<(&str, &str)> = match self.mode {
      AuthMode::Jwt => vec![("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &self.token)],
      AuthMode::Basic => {
        vec![("grant_type", "password"), ("username", &username), ("password", &self.token), ("access_token_ttl", &ttl), ("refresh_token_ttl", &refresh_ttl)]
      }
    };

    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.client_id, self.client_secret));
    let result = client
      .post(format!("{}/restapi/oauth/token", base))
      .header("User-Agent", APP_ID)
      .header("Accept", "application/json")
      .header("Authorization", format!("Basic {}", credentials))
      .form(&form)
      .send()
      .await;

    let resp = match result {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending RingCentral auth.login.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return None;
      }
    };
    if resp.status() != reqwest::StatusCode::OK {
      tracing::warn!("Failed to send RingCentral auth.login: error={}.", resp.status().as_u16());
      return None;
    }

    // A 200 OK without an access_token is not a usable login
    let content: Value = resp.json().await.unwrap_or(Value::Null);
    let token = content.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty())?.to_string();
    let lifetime = content.get("expires_in").and_then(Value::as_f64).and_then(|s| Duration::try_from_secs_f64(s).ok()).unwrap_or_default();
    let now = Instant::now();
    *self.access_token.lock().unwrap() = Some((token.clone(), now.checked_add(lifetime).unwrap_or(now)));
    Some(token)
  }
}

#[async_trait]
impl Notify for RingCentral {
  fn schemas(&self) -> &[&str] {
    &["ringc"]
  }
  fn service_name(&self) -> &str {
    "RingCentral"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn body_maxlen(&self) -> usize {
    160
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = service_base(&self.platform_url());
    let client = build_client(self.verify_certificate)?;

    let Some(access_token) = self.login(&client, &base).await else {
      tracing::warn!("RingCentral {} authentication failed.", self.mode.as_str());
      return Ok(false);
    };
    let authorization = format!("Bearer {}", access_token);

    // Attachments switch every message to the MMS endpoint
    let use_mms = !ctx.attachments.is_empty();
    let url = format!("{}/restapi/v1.0/account/~/extension/~/{}", base, if use_mms { "mms" } else { "sms" });

    // With no targets, the message goes back to our own number
    let targets = if self.targets.is_empty() { std::slice::from_ref(&self.source) } else { self.targets.as_slice() };

    let mut has_error = false;
    for target in targets {
      // A retry skips numbers that already accepted the message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let metadata = json!({
        "from": {"phoneNumber": format!("+{}", self.source)},
        "to": [{"phoneNumber": format!("+{}", target)}],
        "text": ctx.body,
      });

      let request = client.post(&url).header("User-Agent", APP_ID).header("Accept", "application/json").header("Authorization", &authorization);
      let request = if use_mms {
        // JSON metadata part followed by one part per attachment
        let json_part = Part::text(metadata.to_string()).mime_str("application/json").map_err(NotifyError::Http)?;
        let mut form = Form::new().part("json", json_part);
        for attachment in &ctx.attachments {
          let part = || Part::bytes(attachment.data.clone()).file_name(attachment.name.clone());
          form = form.part("attachment", part().mime_str(&attachment.mime_type).unwrap_or_else(|_| part()));
        }
        request.multipart(form)
      } else {
        request.header("Content-Type", "application/json").body(metadata.to_string())
      };

      // Only notification calls are throttled; logins are not
      crate::notify::throttle().await;
      match request.send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent RingCentral notification to {}.", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          tracing::warn!("Failed to send RingCentral notification: error={}.", resp.status().as_u16());
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending RingCentral notification.");
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
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
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  // base64("myclientid:myclientsecret")
  const BASIC_AUTH: &str = "Basic bXljbGllbnRpZDpteWNsaWVudHNlY3JldA==";
  const JWT: &str = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4iLCJpYXQiOjE1MTYyMzkwMjJ9.abcdefghijklmnop";

  fn parse(url: &str) -> Option<RingCentral> {
    RingCentral::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn token_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
      "access_token": "abc123",
      "token_type": "bearer",
      "expires_in": 3600,
      "scope": "Faxes SMS TeamMessaging A2PSMS",
      "owner_id": "123",
      "endpoint_id": "akfJbWJYQ7GEUev2CaR37k",
    }))
  }

  const SMS: &str = "/restapi/v1.0/account/~/extension/~/sms";

  fn sms_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"id": 123, "type": "SMS", "messageStatus": "Queued"}))
  }

  async fn mount_password_login(server: &MockServer, expected_calls: u64) {
    Mock::given(method("POST"))
      .and(path("/restapi/oauth/token"))
      .and(header("User-Agent", APP_ID))
      .and(header("Accept", "application/json"))
      .and(header("Authorization", BASIC_AUTH))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("grant_type=password&username=%2B15551234567&password=secretpass&access_token_ttl=3600&refresh_token_ttl=604800"))
      .respond_with(token_response())
      .expect(expected_calls)
      .mount(server)
      .await;
  }

  fn sms_body(to: &str, text: &str) -> Value {
    json!({"from": {"phoneNumber": "+15551234567"}, "to": [{"phoneNumber": to}], "text": text})
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "ringc://".to_string(),
      "ringc://:@/".to_string(),
      // No client secret
      "ringc://password@client_id/18005554321".to_string(),
      format!("ringc://18005554321:jwt{}@client_id", "a".repeat(60)),
      // Invalid client id / secret
      format!("ringc://18005554321:jwt{}@%21%21%21/secret", "b".repeat(60)),
      format!("ringc://18005554321:jwt{}@client_id/%21%21%21/", "c".repeat(60)),
      // Invalid auth mode / environment
      "ringc://18005554321:password@client_id/secret?mode=invalid".to_string(),
      "ringc://18005554321:password@client_id/secret?env=invalid".to_string(),
      "ringc://18005554321:password@client_id/secret?env=production".to_string(),
      // Invalid JWT (contains disallowed chars)
      "ringc://18005554321:jwt=@client_id/secret?mode=jwt".to_string(),
      // Invalid source phone
      "ringc://notaphone:password@client_id/secret".to_string(),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let jwt60 = format!("jwt{}", "c".repeat(60));

    let obj = parse(&format!("ringc://18005554321:{}@client_id/secret/1555123456?mode=jwt", jwt60)).unwrap();
    assert_eq!(obj.mode, AuthMode::Jwt);
    assert_eq!(obj.source, "18005554321");
    assert_eq!(obj.targets, vec!["1555123456"]);
    assert_eq!(obj.client_id, "client_id");
    assert_eq!(obj.client_secret, "secret");

    // JWT detected from the token length
    assert_eq!(parse(&format!("ringc://18005554321:{}@client_id/secret/1555123456", jwt60)).unwrap().mode, AuthMode::Jwt);
    assert_eq!(parse("ringc://15559990000:shortpw@client_id/secret").unwrap().mode, AuthMode::Basic);
    assert_eq!(parse(&format!("ringc://15559990000:{}@client_id/secret?mode=b", "x".repeat(70))).unwrap().mode, AuthMode::Basic);

    // Invalid targets are dropped (the source is used instead)
    let obj = parse(&format!("ringc://18005554321:{}@client_id/secret/245/?env=sandbox", jwt60)).unwrap();
    assert!(obj.targets.is_empty());
    assert_eq!(obj.environment, Environment::Sandbox);
    assert_eq!(parse("ringc://18005554321:pw@client_id/secret?env=Sand").unwrap().environment, Environment::Sandbox);
    assert_eq!(parse("ringc://18005554321:pw@client_id/secret?env=p").unwrap().environment, Environment::Production);

    // A BASIC password is not validated
    let obj = parse("ringc://18005554321:pass%20word!@client_id/secret").unwrap();
    assert_eq!(obj.token, "pass word!");

    // Query parameter forms
    let obj = parse(&format!("ringc://_?token={}&secret={}&from={}", "a".repeat(8), "b".repeat(16), "5".repeat(11))).unwrap();
    assert_eq!(obj.client_id, "_");
    assert_eq!(obj.token, "a".repeat(8));
    assert_eq!(obj.client_secret, "b".repeat(16));
    assert_eq!(obj.source, "5".repeat(11));
    let obj = parse(&format!("ringc://_?token={}&secret={}&source={}&to={}", "a".repeat(8), "b".repeat(16), "5".repeat(11), "7".repeat(13))).unwrap();
    assert_eq!(obj.source, "5".repeat(11));
    assert_eq!(obj.targets, vec!["7".repeat(13)]);
    let obj = parse(&format!("ringc://client_id?token={}&secret=secret&from=15559990000", "y".repeat(70))).unwrap();
    assert_eq!(obj.mode, AuthMode::Jwt);

    // Token first, source in the path
    let obj = parse("ringc://secretpass@myclientid/myclientsecret/15551234567/15559876543").unwrap();
    assert_eq!(obj.token, "secretpass");
    assert_eq!(obj.source, "15551234567");
    assert_eq!(obj.targets, vec!["15559876543"]);

    // ?source= wins over ?from=
    let obj = parse("ringc://secretpass@myclientid/myclientsecret?from=15551234567&source=15559990000&to=15559876543,15551112222").unwrap();
    assert_eq!(obj.source, "15559990000");
    assert_eq!(obj.targets, vec!["15559876543", "15551112222"]);
  }

  #[test]
  fn test_attributes() {
    let svc = from_url("ringc://18005554321:password@client_id/secret").unwrap();
    assert_eq!(svc.body_maxlen(), 160);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(svc.attachment_support());

    assert_eq!(parse("ringc://18005554321:pw@client_id/secret").unwrap().platform_url(), "https://platform.ringcentral.com");
    assert_eq!(parse("ringc://18005554321:pw@client_id/secret?env=sandbox").unwrap().platform_url(), "https://platform.devtest.ringcentral.com");
  }

  #[tokio::test]
  async fn test_send_sms_and_reuse_token() {
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    for to in ["+15559876543", "+15551112222"] {
      Mock::given(method("POST"))
        .and(path("/restapi/v1.0/account/~/extension/~/sms"))
        .and(header("User-Agent", APP_ID))
        .and(header("Accept", "application/json"))
        .and(header("Authorization", "Bearer abc123"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(sms_body(to, "t\r\nbody")))
        .respond_with(sms_response())
        .expect(2)
        .mount(&server)
        .await;
    }

    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559876543/15551112222").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t\r\nbody"))).await.unwrap());
    // The second send reuses the cached access token
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_loopback_to_source() {
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    Mock::given(method("POST"))
      .and(path("/restapi/v1.0/account/~/extension/~/sms"))
      .and(body_json(sms_body("+15551234567", "body")))
      .respond_with(sms_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_jwt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/restapi/oauth/token"))
      .and(header("Authorization", BASIC_AUTH))
      .and(body_string(format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}", JWT)))
      .respond_with(token_response())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/restapi/v1.0/account/~/extension/~/sms"))
      .and(body_json(sms_body("+15559876543", "body")))
      .respond_with(sms_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("ringc://15551234567:{}@myclientid/myclientsecret/15559876543", JWT)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_expired_token_logs_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/restapi/oauth/token"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "abc123", "expires_in": 0})))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/restapi/v1.0/account/~/extension/~/sms")).respond_with(sms_response()).expect(2).mount(&server).await;

    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("one"))).await.unwrap());
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("two"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_mms() {
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    Mock::given(method("POST"))
      .and(path("/restapi/v1.0/account/~/extension/~/mms"))
      .and(header("Authorization", "Bearer abc123"))
      .respond_with(sms_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559876543").unwrap();
    let mut ctx = ctx("title\r\nbody");
    ctx.attachments.push(Attachment { name: "pixel.png".into(), data: b"PNGDATA".to_vec(), mime_type: "image/png".into() });
    ctx.attachments.push(Attachment { name: "notes.txt".into(), data: b"plain text attachment".to_vec(), mime_type: "text/plain".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/restapi/v1.0/account/~/extension/~/mms").await;
    let mms = &requests[0];
    assert!(mms.headers.get("Content-Type").unwrap().to_str().unwrap().starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&mms.body).to_string();
    let metadata = sms_body("+15559876543", "title\r\nbody").to_string();
    let json_part = format!("Content-Disposition: form-data; name=\"json\"\r\nContent-Type: application/json\r\n\r\n{}\r\n", metadata);
    assert!(body.contains(&json_part), "missing json part in {}", body);
    let png = body.find("name=\"attachment\"; filename=\"pixel.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA").unwrap();
    let txt = body.find("name=\"attachment\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nplain text attachment").unwrap();
    assert!(body.find(&json_part).unwrap() < png && png < txt);
  }

  #[tokio::test]
  async fn test_auth_failures() {
    for response in [
      ResponseTemplate::new(500).set_body_string("{}"),
      // A 200 OK without an access_token is not a usable login
      ResponseTemplate::new(200).set_body_string("{}"),
      ResponseTemplate::new(200).set_body_string("not-json"),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/restapi/oauth/token")).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test"))).await.unwrap());
    }

    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("Test"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500, 999, 201] {
      let server = MockServer::start().await;
      mount_password_login(&server, 1).await;
      Mock::given(method("POST"))
        .and(path("/restapi/v1.0/account/~/extension/~/sms"))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559876543").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test"))).await.unwrap(), "status {}", status);
    }

    // A 200 with a non-JSON body is still a success
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    Mock::given(method("POST"))
      .and(path("/restapi/v1.0/account/~/extension/~/sms"))
      .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
      .mount(&server)
      .await;
    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559876543").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test"))).await.unwrap());

    // One failed target still notifies the rest
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    Mock::given(method("POST")).and(path(SMS)).and(body_json(sms_body("+15559998881", "Partial"))).respond_with(sms_response()).expect(1).mount(&server).await;
    Mock::given(method("POST"))
      .and(path(SMS))
      .and(body_json(sms_body("+15559998882", "Partial")))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559998881/15559998882").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("Partial"))).await.unwrap());

    // MMS rejected by the server
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    Mock::given(method("POST")).and(path("/restapi/v1.0/account/~/extension/~/mms")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let svc = from_url("ringc://15551234567:secretpass@myclientid/myclientsecret/15559876543").unwrap();
    let mut mms = ctx("MMS");
    mms.attachments.push(Attachment { name: "a.gif".into(), data: vec![1], mime_type: "image/gif".into() });
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&mms)).await.unwrap());
  }

  #[test]
  fn test_blank_option_value_loads() {
    // Upstream trims query values, so a whitespace-only value counts as unset.
    let url = "ringc://18005554321:password@client_id/secret?mode=%20";
    assert!(crate::notify::registry::from_url(url).is_some(), "Should parse: {}", url);
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    mount_password_login(&server, 1).await;
    // The second number fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path(SMS))
      .and(body_json(sms_body("+15559998882", "Retry")))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(SMS)).respond_with(sms_response()).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("ringc://15551234567:secretpass@myclientid/myclientsecret/15559998881/15559998882?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("Retry"))).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", SMS)
      .await
      .iter()
      .map(|r| r.body_json::<Value>().unwrap()["to"][0]["phoneNumber"].clone())
      .collect();
    assert_eq!(sent, vec![json!("+15559998881"), json!("+15559998882"), json!("+15559998882")]);
  }
}

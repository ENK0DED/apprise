use crate::conversion::convert_between;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::storage::with_service_store;
use crate::types::NotifyFormat;
use crate::utils::cwe312::cwe312_word;
use crate::utils::email::{is_email, parse_emails};
use crate::utils::parse::ParsedUrl;
use crate::utils::sanitize::sanitize_payload;
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// SendPulse rejects an empty subject.
const DEFAULT_EMPTY_SUBJECT: &str = "<no subject>";

/// Token lifetime assumed when the service reports none, in seconds.
const TOKEN_EXPIRY: i64 = 3600;

/// Seconds before expiry at which a token is renewed.
const TOKEN_EXPIRY_EDGE: i64 = 10;

/// Persistent store key of the OAuth access token.
const TOKEN_KEY: &str = "access_token";

pub struct SendPulse {
  client_id: String,
  client_secret: String,
  from_email: String,
  targets: Vec<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  /// Display names by address (upstream `names`); `None` stands for the
  /// asset's app ID, the sender's default name.
  names: HashMap<String, Option<String>>,
  template: Option<i64>,
  template_data: Map<String, Value>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's client ID and secret regex `^[A-Z0-9._-]+$` (any case).
fn is_credential(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Python's `int()` of a string: optional sign, digits with single
/// underscores between them, surrounding whitespace ignored.
fn py_int(value: &str) -> Option<i64> {
  let s = value.trim();
  let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
    return None;
  }
  let cleaned: String = s.chars().filter(|c| *c != '_').collect();
  if !cleaned.trim_start_matches(['+', '-']).chars().all(|c| c.is_ascii_digit()) {
    return None;
  }
  cleaned.parse().ok()
}

/// The token lifetime upstream `login()` derives from `expires_in`, or
/// `None` when the service returned an unusable one.
fn token_lifetime(expires_in: Option<&Value>) -> Option<i64> {
  let parsed = match expires_in {
    Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
    Some(Value::String(s)) => py_int(s),
    Some(Value::Bool(b)) => Some(*b as i64),
    _ => None,
  };
  let Some(seconds) = parsed else {
    tracing::warn!("SendPulse token expiry limit presumed to be: {}s", TOKEN_EXPIRY);
    return Some(TOKEN_EXPIRY - TOKEN_EXPIRY_EDGE);
  };
  let expires = seconds.saturating_sub(TOKEN_EXPIRY_EDGE);
  if expires <= TOKEN_EXPIRY_EDGE {
    tracing::error!("SendPulse token expiry limit returned was invalid");
    return None;
  }
  if expires > TOKEN_EXPIRY {
    tracing::warn!("SendPulse token expiry limit fixed to: {}s", TOKEN_EXPIRY);
    return Some(TOKEN_EXPIRY - TOKEN_EXPIRY_EDGE);
  }
  Some(expires)
}

/// The default HTTP lookup upstream uses for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Unsupported Parameters., ",
    401 => "Verification Failed., ",
    404 => "Page not found., ",
    405 => "Method not allowed., ",
    500 => "Internal server error., ",
    503 => "Servers are overloaded., ",
    _ => "",
  }
}

/// What a request to SendPulse came back with.
enum Reply {
  /// A JSON response with this status.
  Json(u16, Value),
  /// No usable response (connection error or a body that is not JSON).
  Failed,
}

impl SendPulse {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // sendpulse://{user}@{domain}/{client_id}/{client_secret}/{targets}
    // sendpulse://{client_id}/{client_secret}/?from={email}
    let mut host = url.host.clone().unwrap_or_default();
    let mut raw_targets: Vec<String> = Vec::new();

    // A sender address given elsewhere frees the host up to be a target
    let from_arg = url.get("from");
    if from_arg.is_some_and(|f| is_email(f).is_some()) {
      raw_targets.push(std::mem::take(&mut host));
    }
    if url.get("user").is_some() && url.user.as_deref().is_some_and(|u| is_email(u).is_some()) {
      raw_targets.push(std::mem::take(&mut host));
    }
    raw_targets.extend(url.path_parts.iter().cloned());

    let mut take = |key: &str| match url.get(key).filter(|v| !v.is_empty()) {
      Some(v) => Some(v.to_string()),
      None if !raw_targets.is_empty() => Some(raw_targets.remove(0)),
      None => None,
    };
    let client_id = take("id").unwrap_or_default();
    let client_secret = take("secret").unwrap_or_default();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.push(to.to_string());
    }

    // The sender: {user}@{host}, the ?user= address, then ?from=
    let mut from_name: Option<String> = None;
    let mut from_email = String::new();
    if let Some(user) = url.user.as_deref() {
      if !host.is_empty() {
        let local = user.split(|c: char| c == '@' || c.is_whitespace()).next().unwrap_or_default();
        from_email = format!("{}@{}", local, host);
      } else if let Some(e) = is_email(user) {
        // Upstream keeps the raw ?user= text (`Name<user@domain>`) as the
        // address here; apprise-go uses the parsed address and so do we
        from_name = e.display_name();
        from_email = e.full_email;
      }
    }
    if let Some(from) = from_arg {
      match is_email(from) {
        Some(e) => {
          from_name = e.display_name().or(from_name);
          from_email = e.full_email;
        }
        // Not an address: only the display name changes
        None => from_name = Some(from.to_string()),
      }
    }
    if is_email(&from_email).is_none() {
      tracing::warn!("Invalid ~From~ email specified: {}", from_email);
      return None;
    }

    if !is_credential(&client_id) {
      tracing::warn!("An invalid SendPulse Client ID was specified.");
      return None;
    }
    if !is_credential(&client_secret) {
      tracing::warn!("An invalid SendPulse Client Secret was specified.");
      return None;
    }

    let template = match url.get("template").filter(|t| !t.is_empty()) {
      Some(t) => match py_int(t) {
        Some(id) => Some(id),
        None => {
          tracing::warn!("The SendPulse Template ID specified ({}) is invalid.", t);
          return None;
        }
      },
      None => None,
    };
    let template_data: Map<String, Value> = url.qsd_add.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

    let mut names: HashMap<String, Option<String>> = HashMap::new();
    names.insert(from_email.clone(), from_name);

    let mut targets: Vec<String> = Vec::new();
    for recipient in raw_targets.iter().flat_map(|t| parse_emails(t)) {
      match is_email(&recipient) {
        Some(e) => {
          if let Some(name) = e.display_name() {
            names.insert(e.full_email.clone(), Some(name));
          }
          targets.push(e.full_email);
        }
        None => tracing::warn!("Dropped invalid To email ({}) specified.", recipient),
      }
    }

    let mut collect = |key: &str, kind: &str| {
      let mut list: Vec<String> = Vec::new();
      for recipient in url.get(key).filter(|v| !v.is_empty()).map(parse_emails).unwrap_or_default() {
        match is_email(&recipient) {
          Some(e) => {
            if let Some(name) = e.display_name() {
              names.insert(e.full_email.clone(), Some(name));
            }
            if !list.contains(&e.full_email) {
              list.push(e.full_email);
            }
          }
          None => tracing::warn!("Dropped invalid {} email ({}) specified.", kind, recipient),
        }
      }
      list
    };
    let cc = collect("cc", "Carbon Copy");
    let bcc = collect("bcc", "Blind Carbon Copy");

    // Without any valid target the sender notifies itself
    if targets.is_empty() {
      targets.push(from_email.clone());
    }

    Some(Self {
      client_id,
      client_secret,
      from_email,
      targets,
      cc,
      bcc,
      names,
      template,
      template_data,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SendPulse",
      service_url: Some("https://sendpulse.com"),
      setup_url: Some("https://appriseit.com/services/sendpulse/"),
      protocols: vec!["sendpulse"],
      description: "Send email via SendPulse.",
      attachment_support: true,
    }
  }

  /// A recipient entry, with the display name when one is known.
  fn recipient(&self, addr: &str, app_id: &str) -> Value {
    let mut entry = json!({ "email": addr });
    if let Some(name) = self.names.get(addr) {
      entry["name"] = json!(name.as_deref().unwrap_or(app_id));
    }
    entry
  }

  /// POST `payload` as JSON (upstream `_fetch()`), with the bearer token
  /// when there is one.
  async fn post(&self, client: &reqwest::Client, url: &str, payload: &Value, token: Option<&str>, target: Option<&str>) -> Reply {
    if tracing::enabled!(tracing::Level::DEBUG) {
      tracing::debug!("SendPulse POST URL: {} (cert_verify={})", url, self.verify_certificate);
      // Upstream logs the client secret here; keep it masked
      let mut logged = payload.clone();
      if let Some(secret) = logged.get_mut("client_secret") {
        *secret = json!(cwe312_word(&self.client_secret, true, false));
      }
      tracing::debug!("SendPulse Payload: {}", sanitize_payload(&logged));
    }

    let mut request = client.post(url).header("User-Agent", APP_ID).header("Content-Type", "application/json");
    if let Some(token) = token {
      request = request.header("Authorization", format!("Bearer {}", token));
    }

    throttle().await;
    let resp = match request.body(payload.to_string()).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending SendPulse notification to {}.", target.unwrap_or("None"));
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Reply::Failed;
      }
    };
    let status = resp.status().as_u16();
    let content = resp.text().await.unwrap_or_default();
    match serde_json::from_str(&content) {
      Ok(json) => Reply::Json(status, json),
      Err(_) => {
        tracing::warn!("Invalid response from SendPulse server.");
        tracing::debug!("Response Details:\r\n{}", content.chars().take(2000).collect::<String>());
        Reply::Failed
      }
    }
  }

  /// Fetch a fresh access token (upstream `login()`) and cache it.
  async fn login(&self, client: &reqwest::Client) -> Option<String> {
    with_service_store(|s| s.clear(&[TOKEN_KEY]));
    let payload = json!({
      "grant_type": "client_credentials",
      "client_id": self.client_id,
      "client_secret": self.client_secret,
    });
    let url = format!("{}/oauth/access_token", service_base("https://api.sendpulse.com"));
    let response = match self.post(client, &url, &payload, None, None).await {
      Reply::Json(200 | 202, response) => {
        tracing::debug!("SendPulse authentication successful");
        response
      }
      Reply::Json(status, response) => {
        tracing::warn!("SendPulse Authentication Request failed: {}error={}.", status_message(status), status);
        tracing::debug!("Response Details:\r\n{}", response.to_string().chars().take(2000).collect::<String>());
        return None;
      }
      Reply::Failed => return None,
    };

    let expires = token_lifetime(response.get("expires_in"))?;
    let token = response.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty())?.to_string();
    with_service_store(|s| s.set(TOKEN_KEY, json!(token), Some(Duration::from_secs(expires as u64)), true));
    Some(token)
  }
}

#[async_trait]
impl Notify for SendPulse {
  fn schemas(&self) -> &[&str] {
    &["sendpulse"]
  }
  fn service_name(&self) -> &str {
    "SendPulse"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Html, NotifyFormat::Text]
  }
  fn request_rate_per_sec(&self) -> f64 {
    // 300 requests per minute
    0.2
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["sendpulse".into(), self.client_id.clone(), self.client_secret.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    // Reuse the cached token; outside a dispatch there is no store
    let cached = with_service_store(|s| s.get(TOKEN_KEY)).flatten().and_then(|v| v.as_str().map(str::to_string));
    let mut token = match cached {
      Some(token) => Some(token),
      None => self.login(&client).await,
    };
    if token.is_none() {
      return Ok(false);
    }

    let app_id = ctx.asset.app_id.as_str();
    let mut email = json!({
      "from": { "name": self.names.get(&self.from_email).cloned().flatten().unwrap_or_else(|| app_id.to_string()), "email": self.from_email },
      "to": [],
      "subject": if ctx.title.is_empty() { DEFAULT_EMPTY_SUBJECT } else { ctx.title.as_str() },
    });
    // HTML adds a derived plain text part; text sends the body alone
    if ctx.body_format == NotifyFormat::Text {
      email["text"] = json!(ctx.body);
    } else {
      email["text"] = json!(convert_between(&NotifyFormat::Html, &NotifyFormat::Text, &ctx.body));
      email["html"] = json!(base64::engine::general_purpose::STANDARD.encode(ctx.body.as_bytes()));
    }
    if !ctx.attachments.is_empty() {
      let mut attachments = Map::new();
      for (i, att) in ctx.attachments.iter().enumerate() {
        let name = if att.name.is_empty() { format!("file{:03}.dat", i + 1) } else { att.name.clone() };
        attachments.insert(name, json!(base64::engine::general_purpose::STANDARD.encode(&att.data)));
      }
      email["attachments_binary"] = Value::Object(attachments);
    }
    if let Some(id) = self.template.filter(|id| *id != 0) {
      email["template"] = json!({ "id": id, "variables": self.template_data });
    }

    let url = format!("{}/smtp/emails", service_base("https://api.sendpulse.com"));
    let mut has_error = false;
    for target in &self.targets {
      // A retry skips the targets that already accepted the message
      if is_delivered(target, false) {
        continue;
      }

      // Every target gets its own cc/bcc lists, without itself in them
      let mut payload = email.clone();
      payload["to"] = json!([self.recipient(target, app_id)]);
      let cc: Vec<Value> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != target).map(|a| self.recipient(a, app_id)).collect();
      if !cc.is_empty() {
        payload["cc"] = json!(cc);
      }
      let bcc: Vec<Value> = self.bcc.iter().filter(|a| *a != target).map(|a| self.recipient(a, app_id)).collect();
      if !bcc.is_empty() {
        payload["bcc"] = json!(bcc);
      }
      let payload = json!({ "email": payload });

      // An expired token is renewed once
      let mut retry = true;
      let delivered = loop {
        match self.post(&client, &url, &payload, token.as_deref(), Some(target)).await {
          Reply::Json(401, _) if retry => {
            retry = false;
            token = self.login(&client).await;
            if token.is_some() {
              continue;
            }
            tracing::warn!("Failed to send SendPulse notification to {}: {}error=401.", target, status_message(401));
            break false;
          }
          Reply::Json(200 | 202, _) => break true,
          Reply::Json(status, response) => {
            tracing::warn!("Failed to send SendPulse notification to {}: {}error={}.", target, status_message(status), status);
            tracing::debug!("Response Details:\r\n{}", response.to_string().chars().take(2000).collect::<String>());
            break false;
          }
          Reply::Failed => break false,
        }
      };
      if delivered {
        tracing::info!("Sent SendPulse notification to {}.", target);
        mark_delivered(target, false);
      } else {
        has_error = true;
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const OAUTH: &str = "/oauth/access_token";
  const EMAILS: &str = "/smtp/emails";

  fn parse(url: &str) -> SendPulse {
    SendPulse::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  fn token(token: &str, expires_in: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "access_token": token, "expires_in": expires_in }))
  }

  fn accepted() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "result": true, "id": "y" }))
  }

  /// The requests the mock server received at `endpoint`, in order.
  async fn requests(server: &MockServer, endpoint: &str) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == endpoint).collect()
  }

  async fn emails(server: &MockServer) -> Vec<Value> {
    requests(server, EMAILS).await.iter().map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["email"].clone()).collect()
  }

  /// A server that hands out `tok` and accepts every email.
  async fn mock() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok", json!(3600))).mount(&server).await;
    Mock::given(method("POST")).and(path(EMAILS)).respond_with(accepted()).mount(&server).await;
    server
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "sendpulse://",
      "sendpulse://:@/",
      "sendpulse://abcd",
      "sendpulse://abcd@host.com",
      "sendpulse://user@example.com/client_id/cs/?template=invalid",
      "sendpulse://?id=ci&secret=cs&user=chris",
      "sendpulse://user@example.com/client+id/cs/",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "sendpulse://user@example.com/client_id/cs1/?template=123",
      "sendpulse://user@example.com/client_id/cs1/",
      "sendpulse://user@example.com/client_id/cs1/?format=text",
      "sendpulse://user@example.com/client_id/cs1/?format=html",
      "sendpulse://chris@example.com/client_id/cs1/?from=Chris",
      "sendpulse://?id=ci&secret=cs&user=chris@example.com",
      "sendpulse://?id=ci&secret=cs&user=Chris<chris@example.com>",
      "sendpulse://example.com/client_id/cs1/?user=chris",
      "sendpulse://client_id/cs1/?user=chris@example.ca",
      "sendpulse://client_id/cs1/?from=Chris<chris@example.com>",
      "sendpulse://?from=Chris<chris@example.com>&id=ci&secret=cs",
      "sendpulse://user@example.com/client_id/cs2/?bcc=l2g@nuxref.com",
      "sendpulse://user@example.com/client_id/cs2/?bcc=invalid",
      "sendpulse://user@example.com/client_id/cs3/?cc=l2g@nuxref.com",
      "sendpulse://user@example.com/client_id/cs4/?cc=Chris<l2g@nuxref.com>",
      "sendpulse://user@example.com/client_id/cs5/?cc=invalid",
      "sendpulse://user@example.com/client_id/cs6/?to=invalid",
      "sendpulse://user@example.com/client_id/cs7/chris@example.com",
      "sendpulse://user@example.com/client_id/cs8/?to=Chris<chris@example.com>",
      "sendpulse://user@example.com/client_id/cs9/chris@example.com/chris2@example.com/Test<test@test.com>",
      "sendpulse://user@example.com/client_id/cs16/?template=1234&+sub=value&+sub2=value2",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values below come from upstream's parsed objects
    let obj = parse("sendpulse://user@example.com/client_id/cs1/?template=123");
    assert_eq!((obj.client_id.as_str(), obj.client_secret.as_str()), ("client_id", "cs1"));
    assert_eq!(obj.from_email, "user@example.com");
    assert_eq!(obj.targets, vec!["user@example.com"]);
    assert_eq!(obj.names.get("user@example.com"), Some(&None));
    assert_eq!(obj.template, Some(123));

    let obj = parse("sendpulse://chris@example.com/client_id/cs1/?from=Chris");
    assert_eq!(obj.from_email, "chris@example.com");
    assert_eq!(obj.names.get("chris@example.com"), Some(&Some("Chris".to_string())));

    let obj = parse("sendpulse://?id=ci&secret=cs&user=chris@example.com");
    assert_eq!((obj.client_id.as_str(), obj.client_secret.as_str(), obj.from_email.as_str()), ("ci", "cs", "chris@example.com"));

    // Upstream keeps "Chris<chris@example.com>" as the address; we keep the
    // address alone (as apprise-go does) and the name
    let obj = parse("sendpulse://?id=ci&secret=cs&user=Chris<chris@example.com>");
    assert_eq!(obj.from_email, "chris@example.com");
    assert_eq!(obj.names.get("chris@example.com"), Some(&Some("Chris".to_string())));

    let obj = parse("sendpulse://example.com/client_id/cs1/?user=chris");
    assert_eq!((obj.client_id.as_str(), obj.from_email.as_str()), ("client_id", "chris@example.com"));

    let obj = parse("sendpulse://client_id/cs1/?user=chris@example.ca");
    assert_eq!((obj.client_id.as_str(), obj.client_secret.as_str(), obj.from_email.as_str()), ("client_id", "cs1", "chris@example.ca"));

    let obj = parse("sendpulse://client_id/cs1/?from=Chris<chris@example.com>");
    assert_eq!((obj.client_id.as_str(), obj.from_email.as_str()), ("client_id", "chris@example.com"));
    assert_eq!(obj.names.get("chris@example.com"), Some(&Some("Chris".to_string())));

    // Invalid recipients leave the sender as the only target
    assert_eq!(parse("sendpulse://user@example.com/client_id/cs6/?to=invalid").targets, vec!["user@example.com"]);
    assert!(parse("sendpulse://user@example.com/client_id/cs2/?bcc=invalid").bcc.is_empty());

    let obj = parse("sendpulse://user@example.com/client_id/cs9/chris@example.com/chris2@example.com/Test<test@test.com>");
    assert_eq!(obj.targets, vec!["chris@example.com", "chris2@example.com", "test@test.com"]);
    assert_eq!(obj.names.get("test@test.com"), Some(&Some("Test".to_string())));

    let obj = parse("sendpulse://user@example.com/client_id/cs4/?cc=Chris<l2g@nuxref.com>");
    assert_eq!(obj.cc, vec!["l2g@nuxref.com"]);
    assert_eq!(obj.names.get("l2g@nuxref.com"), Some(&Some("Chris".to_string())));

    let obj = parse("sendpulse://user@example.com/client_id/cs16/?template=1234&+sub=value&+sub2=value2");
    assert_eq!(obj.template, Some(1234));
    assert_eq!(Value::Object(obj.template_data), json!({ "sub": "value", "sub2": "value2" }));
  }

  #[test]
  fn test_helpers() {
    assert_eq!(py_int("123"), Some(123));
    assert_eq!(py_int(" -5 "), Some(-5));
    assert_eq!(py_int("1_000"), Some(1000));
    assert_eq!(py_int("1__0"), None);
    assert_eq!(py_int("12a"), None);
    assert_eq!(py_int(""), None);

    // None, garbage, too long and too short lifetimes, as upstream tests
    assert_eq!(token_lifetime(None), Some(3590));
    assert_eq!(token_lifetime(Some(&json!(null))), Some(3590));
    assert_eq!(token_lifetime(Some(&json!("garbage"))), Some(3590));
    assert_eq!(token_lifetime(Some(&json!(300000))), Some(3590));
    assert_eq!(token_lifetime(Some(&json!(3600))), Some(3590));
    assert_eq!(token_lifetime(Some(&json!("1800"))), Some(1790));
    assert_eq!(token_lifetime(Some(&json!(-1))), None);
    assert_eq!(token_lifetime(Some(&json!(20))), None);
  }

  #[test]
  fn test_service_details() {
    let details = SendPulse::static_details();
    assert_eq!(details.service_name, "SendPulse");
    assert_eq!(details.protocols, vec!["sendpulse"]);
    assert!(details.attachment_support);
    let svc = from_url("sendpulse://user@example.com/ci/cs/").unwrap();
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Text]);
    assert_eq!(svc.request_rate_per_sec(), 0.2);
  }

  #[tokio::test]
  async fn test_send_oauth_and_email() {
    let server = MockServer::start().await;
    // The apprise-go golden for this URL
    Mock::given(method("POST"))
      .and(path(OAUTH))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({ "grant_type": "client_credentials", "client_id": "clientid", "client_secret": "clientsecret" })))
      .respond_with(token("token", json!(3600)))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(EMAILS))
      .and(header("Authorization", "Bearer token"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "email": {
        "from": { "name": "Apprise", "email": "user@example.com" },
        "to": [{ "email": "target@example.com" }],
        "subject": "apprise parity",
        "text": "hello from python",
        "html": "aGVsbG8gZnJvbSBweXRob24=",
      }})))
      .respond_with(accepted())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("sendpulse://user@example.com/clientid/clientsecret/target@example.com").unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("apprise parity", "hello from python") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_markdown_input() {
    let server = mock().await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("sendpulse://user@example.com/clientid/clientsecret/target@example.com"));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("over", "**bold** and [link](https://example.com) plus `code`.") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    // The apprise-go golden for this URL
    assert_eq!(
      emails(&server).await,
      vec![json!({
        "from": { "name": "Apprise", "email": "user@example.com" },
        "to": [{ "email": "target@example.com" }],
        "subject": "over",
        "text": "bold and link plus\ncode\n.",
        "html": "PHA+PHN0cm9uZz5ib2xkPC9zdHJvbmc+IGFuZCA8YSBocmVmPSJodHRwczovL2V4YW1wbGUuY29tIj5saW5rPC9hPiBwbHVzIDxjb2RlPmNvZGU8L2NvZGU+LjwvcD4=",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_text_names_and_self_target() {
    let server = mock().await;
    // Matches pycapture.py: the text body alone, and the sender's name on
    // the self-addressed copy
    let svc = from_url("sendpulse://user@example.com/ci/cs/?from=John").unwrap();
    let c = NotifyContext { body_format: NotifyFormat::Text, ..ctx("", "hello") };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());
    assert_eq!(
      emails(&server).await,
      vec![json!({
        "from": { "name": "John", "email": "user@example.com" },
        "to": [{ "email": "user@example.com", "name": "John" }],
        "subject": "<no subject>",
        "text": "hello",
      })]
    );
  }

  #[tokio::test]
  async fn test_send_cc_bcc_template_attachments() {
    let server = mock().await;
    let svc = from_url(
      "sendpulse://user@example.com/cid/cs/a@example.com/Test<b@example.com>\
       ?cc=Chris<c@example.com>,b@example.com,d@example.com&bcc=Dee<d@example.com>&from=Sender&template=1234&+sub=value",
    )
    .unwrap();
    let mut c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("T", "<b>hi</b>") };
    c.attachments = vec![Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&c)).await.unwrap());

    // Matches pycapture.py (upstream's cc order comes from a set)
    let common = json!({
      "from": { "name": "Sender", "email": "user@example.com" },
      "subject": "T", "text": "hi", "html": "PGI+aGk8L2I+",
      "attachments_binary": { "report.pdf": "UERGREFUQQ==" },
      "template": { "id": 1234, "variables": { "sub": "value" } },
      "bcc": [{ "email": "d@example.com", "name": "Dee" }],
    });
    let with = |extra: Value| {
      let mut v = common.clone();
      v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
      v
    };
    assert_eq!(
      emails(&server).await,
      vec![
        with(json!({
          "to": [{ "email": "a@example.com" }],
          "cc": [{ "email": "c@example.com", "name": "Chris" }, { "email": "b@example.com", "name": "Test" }],
        })),
        with(json!({ "to": [{ "email": "b@example.com", "name": "Test" }], "cc": [{ "email": "c@example.com", "name": "Chris" }] })),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_cc_per_target() {
    // f39501d0: one target's cc list never carries over to the next
    let server = mock().await;
    let svc = from_url("sendpulse://user@example.com/ci/cs/a@example.com/b@example.com?cc=b@example.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "body"))).await.unwrap());
    let sent = emails(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["to"], json!([{ "email": "a@example.com" }]));
    assert_eq!(sent[0]["cc"], json!([{ "email": "b@example.com" }]));
    assert_eq!(sent[1]["to"], json!([{ "email": "b@example.com" }]));
    assert!(sent[1].get("cc").is_none());
  }

  #[tokio::test]
  async fn test_send_multiple_cc_bcc() {
    // f39501d0: every cc and bcc address is sent, not only the last one
    let server = mock().await;
    let svc =
      from_url("sendpulse://user@example.com/ci/cs/to@example.com?cc=Alice<a@example.com>,b@example.com&bcc=Carol<c@example.com>,d@example.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "body"))).await.unwrap());
    let sent = emails(&server).await;
    assert_eq!(sent[0]["cc"], json!([{ "email": "a@example.com", "name": "Alice" }, { "email": "b@example.com" }]));
    assert_eq!(sent[0]["bcc"], json!([{ "email": "c@example.com", "name": "Carol" }, { "email": "d@example.com" }]));
  }

  #[tokio::test]
  async fn test_token_cached_between_notifications() {
    let server = mock().await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("sendpulse://user@example.com/ci/cs/?from=John"));
    for _ in 0..2 {
      assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("t", "b"))).await.is_success());
    }
    // The second notification reuses the stored token
    assert_eq!(requests(&server, OAUTH).await.len(), 1);
    let sent = requests(&server, EMAILS).await;
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.headers.get("authorization").unwrap() == "Bearer tok"));
  }

  #[tokio::test]
  async fn test_reauthenticate_once_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok1", json!(3600))).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok2", json!(3600))).mount(&server).await;
    Mock::given(method("POST"))
      .and(path(EMAILS))
      .and(header("Authorization", "Bearer tok1"))
      .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "error": "expired" })))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(EMAILS)).and(header("Authorization", "Bearer tok2")).respond_with(accepted()).mount(&server).await;

    let svc = from_url("sendpulse://usr2@example.com/ci/cs/?from=Retry").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert_eq!(requests(&server, OAUTH).await.len(), 2);
    assert_eq!(requests(&server, EMAILS).await.len(), 2);
  }

  #[tokio::test]
  async fn test_reauthenticate_does_not_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok", json!(3600))).mount(&server).await;
    Mock::given(method("POST")).and(path(EMAILS)).respond_with(ResponseTemplate::new(401).set_body_json(json!({}))).mount(&server).await;
    let svc = from_url("sendpulse://usr2@example.com/ci/cs/?from=Retry").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert_eq!(requests(&server, OAUTH).await.len(), 2);
    assert_eq!(requests(&server, EMAILS).await.len(), 2);
  }

  #[tokio::test]
  async fn test_unauthorized_without_json_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok", json!(3600))).mount(&server).await;
    Mock::given(method("POST")).and(path(EMAILS)).respond_with(ResponseTemplate::new(401).set_body_string("not json")).mount(&server).await;
    let svc = from_url("sendpulse://user@example.com/cid/cs/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert_eq!(requests(&server, OAUTH).await.len(), 1);
  }

  #[tokio::test]
  async fn test_login_failures() {
    for response in [
      // An unusable lifetime, no token, a bad status and a non-JSON body
      token("tok", json!(5)),
      ResponseTemplate::new(200).set_body_json(json!({ "expires_in": 3600 })),
      ResponseTemplate::new(403).set_body_json(json!({ "error": "denied" })),
      ResponseTemplate::new(200).set_body_string("{"),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(OAUTH)).respond_with(response).mount(&server).await;
      let svc = from_url("sendpulse://user@example.com/cid/cs/").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
      assert!(requests(&server, EMAILS).await.is_empty());
    }
  }

  #[tokio::test]
  async fn test_send_failures() {
    for response in [ResponseTemplate::new(403).set_body_json(json!({})), ResponseTemplate::new(200).set_body_string("{")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(OAUTH)).respond_with(token("tok", json!(3600))).mount(&server).await;
      Mock::given(method("POST")).and(path(EMAILS)).respond_with(response).mount(&server).await;
      let svc = from_url("sendpulse://user@example.com/cid/cs/").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    }
    let svc = from_url("sendpulse://user@example.com/cid/cs/").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:9", svc.send(&ctx("t", "b"))).await.unwrap());
  }
}

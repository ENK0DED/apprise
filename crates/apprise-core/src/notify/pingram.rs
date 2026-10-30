use crate::asset::NotifyImageSize;
use crate::conversion::convert_between;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::email::{is_email, parse_emails};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::is_phone_no;
use crate::utils::plugin_args::parse_list;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

/// Message type used when none is given.
const DEFAULT_MESSAGE_TYPE: &str = "apprise";

/// Upstream `PINGRAM_REGIONS` with their API endpoints (`PINGRAM_API_LOOKUP`).
const REGIONS: &[(&str, &str)] = &[("us", "https://api.pingram.io"), ("ca", "https://api.ca.pingram.io"), ("eu", "https://api.eu.pingram.io")];

/// Upstream `PINGRAM_CHANNELS`.
const CHANNELS: &[&str] = &["email", "sms", "inapp", "web_push", "mobile_push", "slack", "call"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Template,
  Message,
}

/// One recipient: any of an id, an email address and a phone number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Target {
  id: Option<String>,
  email: Option<String>,
  number: Option<String>,
}

impl Target {
  fn is_empty(&self) -> bool {
    self.id.is_none() && self.email.is_none() && self.number.is_none()
  }

  /// What logs and delivery tracking call this recipient.
  fn describe(&self) -> &str {
    self.id.as_deref().or(self.number.as_deref()).or(self.email.as_deref()).unwrap_or_default()
  }

  fn to_json(&self) -> Value {
    let mut to = Map::new();
    for (key, value) in [("id", &self.id), ("email", &self.email), ("number", &self.number)] {
      if let Some(value) = value {
        to.insert(key.into(), json!(value));
      }
    }
    Value::Object(to)
  }
}

pub struct Pingram {
  apikey: String,
  message_type: String,
  mode: Mode,
  region: &'static str,
  /// Forced or detected channels, in [`CHANNELS`] order.
  channels: Vec<&'static str>,
  targets: Vec<Target>,
  /// The sender address; `None` when `?from=` held no address.
  from_email: Option<String>,
  /// The sender name; `None` stands for the asset's app ID.
  from_name: Option<String>,
  cc: Vec<String>,
  bcc: Vec<String>,
  tokens: Map<String, Value>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's API key regex `^pingram_(sk|pk)_[\w.-]+$` (any case).
fn is_apikey(key: &str) -> bool {
  let lower = key.to_lowercase();
  let rest = lower.strip_prefix("pingram_sk_").or_else(|| lower.strip_prefix("pingram_pk_"));
  rest.is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-')))
}

/// Upstream's message type regex `^[A-Z0-9_-]+$` (any case).
fn is_message_type(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Upstream `IS_VALID_ID_RE` (`^\s*(@|%40)?(?P<id>[\w_-]+)\s*$`).
fn recipient_id(entry: &str) -> Option<&str> {
  let s = entry.trim();
  let s = s.strip_prefix('@').or_else(|| s.strip_prefix("%40")).unwrap_or(s);
  (!s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))).then_some(s)
}

/// Upstream's target grouping: ids, email addresses and phone numbers are
/// gathered into one recipient until a second value of a kind starts the
/// next one. The first address or number also picks the default channel
/// when none was forced.
fn group_targets(entries: &[String], channels: &mut Vec<&'static str>) -> Vec<Target> {
  let mut targets = Vec::new();
  let mut current = Target::default();
  for entry in entries {
    if let Some(e) = is_email(entry) {
      if current.email.is_some() {
        targets.push(std::mem::take(&mut current));
      } else if channels.is_empty() {
        channels.push("email");
        tracing::info!("The Pingram default channel of email was set.");
      }
      current.email = Some(e.full_email);
      continue;
    }
    if let Some(phone) = is_phone_no(entry, 11) {
      // Upstream drops the `+` from every number after the first in a
      // row; apprise-go keeps it, and so do we
      let number = format!("{}{}", if entry.starts_with('+') { "+" } else { "" }, phone.full);
      if current.number.is_some() {
        targets.push(std::mem::take(&mut current));
      } else if channels.is_empty() {
        channels.push("sms");
        tracing::info!("The Pingram default channel of sms was set.");
      }
      current.number = Some(number);
      continue;
    }
    if let Some(id) = recipient_id(entry) {
      if current.id.is_some() {
        targets.push(std::mem::take(&mut current));
      }
      current.id = Some(id.to_string());
      continue;
    }
    tracing::warn!("Dropped invalid Pingram target ({}) specified", entry);
  }
  if !current.is_empty() {
    targets.push(current);
  }
  targets
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

impl Pingram {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pingram://{apikey}/{targets}
    // pingram://{type}@{apikey}/{targets}
    let mut raw_targets: Vec<String> = url.host.iter().cloned().collect();
    raw_targets.extend(url.path_parts.iter().cloned());
    let apikey = match url.get("apikey").filter(|k| !k.is_empty()) {
      Some(key) => key.to_string(),
      None if !raw_targets.is_empty() => raw_targets.remove(0),
      None => String::new(),
    };
    if !is_apikey(&apikey) {
      tracing::warn!("An invalid Pingram API Key was specified.");
      return None;
    }
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.push(to.to_string());
    }

    // The sender; a ?from= without an address only names it
    let mut from_name: Option<String> = None;
    let mut from_email: Option<String> = None;
    if let Some(from) = url.get("from").filter(|f| !f.is_empty()) {
      match is_email(from) {
        Some(e) => {
          from_name = e.display_name();
          from_email = Some(e.full_email);
        }
        None => from_name = Some(from.to_string()),
      }
    }

    let message_type = url.get("type").filter(|t| !t.is_empty()).map(str::to_string).or_else(|| url.user.clone());
    let mode = match url.get("mode").filter(|m| !m.is_empty()) {
      // Matched by prefix, case-sensitively, as upstream does
      Some(m) if "template".starts_with(m) => Mode::Template,
      Some(m) if "message".starts_with(m) => Mode::Message,
      Some(m) => {
        tracing::warn!("The Pingram mode specified ({}) is invalid.", m);
        return None;
      }
      None if message_type.is_some() => Mode::Template,
      None => Mode::Message,
    };
    let message_type = match message_type {
      Some(t) if is_message_type(&t) => t,
      Some(t) => {
        tracing::warn!("An invalid Pingram Message Type ({}) was specified.", t);
        return None;
      }
      None => DEFAULT_MESSAGE_TYPE.to_string(),
    };

    let region = match url.get("region").filter(|r| !r.is_empty()) {
      Some(r) => match REGIONS.iter().find(|(name, _)| *name == r.to_lowercase()) {
        Some((name, _)) => *name,
        None => {
          tracing::warn!("The Pingram region specified ({}) is invalid.", r);
          return None;
        }
      },
      None => "us",
    };

    let mut forced: Vec<String> = Vec::new();
    for channel in url.get("channels").filter(|c| !c.is_empty()).map(|c| parse_list([c], true, true)).unwrap_or_default() {
      let channel = channel.to_lowercase();
      if !CHANNELS.contains(&channel.as_str()) {
        tracing::warn!("The Pingram forced channel specified ({}) is invalid.", channel);
        return None;
      }
      forced.push(channel);
    }
    let mut channels: Vec<&'static str> = CHANNELS.iter().copied().filter(|c| forced.iter().any(|f| f == c)).collect();

    let entries = parse_list(&raw_targets, true, false);
    let targets = group_targets(&entries, &mut channels);
    channels.sort_by_key(|c| CHANNELS.iter().position(|x| x == c));

    let collect = |key: &str, kind: &str| {
      let mut list: Vec<String> = Vec::new();
      for recipient in url.get(key).filter(|v| !v.is_empty()).map(parse_emails).unwrap_or_default() {
        match is_email(&recipient) {
          Some(e) if !list.contains(&e.full_email) => list.push(e.full_email),
          Some(_) => {}
          None => tracing::warn!("Dropped invalid {} email ({}) specified.", kind, recipient),
        }
      }
      list
    };
    let cc = collect("cc", "Carbon Copy");
    let bcc = collect("bcc", "Blind Carbon Copy");

    let tokens: Map<String, Value> = url.qsd_colon.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

    Some(Self {
      apikey,
      message_type,
      mode,
      region,
      channels,
      targets,
      from_email,
      from_name,
      cc,
      bcc,
      tokens,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pingram",
      service_url: Some("https://www.pingram.io/"),
      setup_url: Some("https://appriseit.com/services/pingram/"),
      protocols: vec!["pingram"],
      description: "Send email, SMS, push, Slack and voice notifications via Pingram (formerly NotificationAPI).",
      attachment_support: false,
    }
  }

  /// The API endpoint of the configured region.
  fn api_base(&self) -> &'static str {
    REGIONS.iter().find(|(name, _)| *name == self.region).map(|(_, base)| *base).unwrap_or(REGIONS[0].1)
  }

  /// The payload shared by every recipient (upstream `gen_payload()`).
  fn base_payload(&self, ctx: &NotifyContext) -> Value {
    let asset = &ctx.asset;
    let image = ctx.image_url(NotifyImageSize::Xy72);
    let title = if ctx.title.is_empty() { asset.app_id.as_str() } else { ctx.title.as_str() };
    let mut payload = json!({ "type": self.message_type });

    match self.mode {
      Mode::Template => {
        let mut parameters = self.tokens.clone();
        for (key, value) in [
          ("appBody", json!(ctx.body)),
          ("appTitle", json!(ctx.title)),
          ("appType", json!(ctx.notify_type.as_str())),
          ("appId", json!(asset.app_id)),
          ("appDescription", json!(asset.app_desc)),
          ("appColor", json!(asset.color(&ctx.notify_type))),
          ("appImageUrl", json!(image)),
          ("appUrl", json!(asset.app_url)),
        ] {
          parameters.insert(key.into(), value);
        }
        payload["parameters"] = Value::Object(parameters);
      }
      Mode::Message => {
        // SMS, calls, push and Slack take text; email takes HTML
        let is_html = ctx.body_format == NotifyFormat::Html;
        let text = if is_html { convert_between(&NotifyFormat::Html, &NotifyFormat::Text, &ctx.body) } else { ctx.body.clone() };
        let titled = if ctx.title.is_empty() { text.clone() } else { format!("{}\n{}", ctx.title, text) };
        for channel in &self.channels {
          payload[*channel] = match *channel {
            "sms" | "call" => json!({ "message": titled }),
            "email" => {
              let html = if is_html { ctx.body.clone() } else { convert_between(&NotifyFormat::Text, &NotifyFormat::Html, &ctx.body) };
              let mut email = json!({ "subject": title, "html": html });
              if let Some(from) = &self.from_email {
                email["senderEmail"] = json!(from);
                email["senderName"] = json!(self.from_name.as_deref().unwrap_or(&asset.app_id));
              }
              email
            }
            "inapp" => json!({ "title": title, "image": image }),
            "web_push" => json!({ "title": title, "message": text, "icon": image }),
            "mobile_push" => json!({ "title": title, "message": text }),
            _ => json!({ "text": titled }),
          };
        }
      }
    }

    if let Some(from) = &self.from_email {
      payload["options"] = json!({ "email": { "fromAddress": from, "fromName": self.from_name.as_deref().unwrap_or(&asset.app_id) } });
    } else if !self.cc.is_empty() || !self.bcc.is_empty() {
      payload["options"] = json!({ "email": {} });
    }
    payload
  }
}

#[async_trait]
impl Notify for Pingram {
  fn schemas(&self) -> &[&str] {
    &["pingram"]
  }
  fn service_name(&self) -> &str {
    "Pingram"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Text, NotifyFormat::Html]
  }
  fn request_rate_per_sec(&self) -> f64 {
    // 300 requests per minute
    0.2
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["pingram".into(), self.apikey.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Pingram recipients to notify");
      return Ok(false);
    }

    let base = self.base_payload(ctx);
    let url = format!("{}/send", service_base(self.api_base()));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for target in &self.targets {
      let desc = target.describe();
      // A retry skips the recipients that already accepted the message
      if is_delivered(desc, false) {
        continue;
      }

      // Copies must not go to the recipient itself, nor cc a bcc address
      let mut payload = base.clone();
      payload["to"] = target.to_json();
      let cc: Vec<&String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && Some(*a) != target.email.as_ref()).collect();
      let bcc: Vec<&String> = self.bcc.iter().filter(|a| Some(*a) != target.email.as_ref()).collect();
      if !cc.is_empty() {
        payload["options"]["email"]["ccAddresses"] = json!(cc);
      }
      if !bcc.is_empty() {
        payload["options"]["email"]["bccAddresses"] = json!(bcc);
      }

      tracing::debug!("Pingram POST URL: {} (cert_verify={})", url, self.verify_certificate);
      tracing::debug!("Pingram Payload: {}", desc);

      throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", self.apikey))
        .body(payload.to_string())
        .send()
        .await;
      let resp = match result {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Pingram notification to {}.", desc);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };
      let status = resp.status().as_u16();
      let content = resp.text().await.unwrap_or_default();
      // Anything but a JSON reply is a failure, whatever the status
      if serde_json::from_str::<Value>(&content).is_err() {
        tracing::warn!("Invalid response from Pingram server.");
        tracing::debug!("Response Details:\r\n{}", content.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      if !matches!(status, 200 | 202) {
        tracing::warn!("Failed to send Pingram notification to {}: {}error={}", desc, status_message(status), status);
        tracing::debug!("Response Details:\r\n{}", content.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Pingram notification to {}.", desc);
      mark_delivered(desc, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const GOOD: &str = r#"{"trackingId": "abc123"}"#;

  fn parse(url: &str) -> Pingram {
    Pingram::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  fn target(id: Option<&str>, email: Option<&str>, number: Option<&str>) -> Target {
    Target { id: id.map(Into::into), email: email.map(Into::into), number: number.map(Into::into) }
  }

  async fn mock(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/send")).respond_with(ResponseTemplate::new(status).set_body_string(body)).mount(&server).await;
    server
  }

  async fn payloads(server: &MockServer) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().filter(|r| r.url.path() == "/send").map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  async fn send(url: &str, c: &NotifyContext) -> (bool, Vec<Value>) {
    let server = mock(200, GOOD).await;
    let ok = crate::notify::with_test_base(server.uri(), from_url(url).unwrap().send(c)).await.unwrap();
    (ok, payloads(&server).await)
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "pingram://",
      "pingram://:@/",
      "pingram://abcd",
      "pingram://pingram_sk_key/+15551235553/?mode=invalid",
      "pingram://pingram_sk_key/+15551235553/?mode=Template",
      "pingram://pingram_sk_key/+15551235553/?region=invalid",
      "pingram://pingram_sk_key/+15551235553/?type=*(",
      "pingram://pingram_sk_key/+15551235553/?channels=bad",
      "pingram://pingram_xk_key/+15551235553",
      // NotificationAPI's schemas are gone upstream
      "napi://pingram_sk_abc/+15551234567",
      "notificationapi://pingram_sk_abc/+15551234567",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "pingram://pingram_sk_abc123/g@rb@ge/+15551235553/",
      "pingram://pingram_sk_abc123/g@rb@ge/",
      "pingram://pingram_sk_abc123/user1@example.com/user2@example.com",
      "pingram://pingram_sk_abc123/+15551235553",
      "pingram://pingram_pk_abc123/user@example.ca",
      "pingram://pingram_sk_aaaa.bbbb.cccc/+15551235553",
      "pingram://pingram_pk_aaaa.bbbb.cccc-dd_ee/user@example.ca",
      "pingram://pingram_sk_abc123/+15551235553/+15551235554",
      "pingram://pingram_sk_abc123/id/+15551235553",
      "pingram://pingram_sk_abc123/id/id2/user1@example.com",
      "pingram://type@pingram_sk_abc123/id10/user2@example.com/id5/+15551235555/id8/+15551235534?reply=Chris<chris@example.com>",
      "pingram://type@pingram_sk_abc123/abc1/user1@example.com/id5/+15551235555/?from=Chris&reply=Christopher",
      "pingram://type@pingram_sk_abc123/user3@example.com/?from=joe@example.ca&reply=user@abc.com",
      "pingram://type@pingram_sk_abc123/user4@example.com/?from=joe@example.ca&bcc=user1@yahoo.ca&cc=user2@yahoo.ca",
      "pingram://?apikey=pingram_sk_abc123&to=id,user5@example.com&type=typec",
      "pingram://pingram_sk_abc123?to=id,user5@example.com&type=typeb",
      "pingram://?apikey=pingram_sk_abc123&type=test-type&region=eu",
      "pingram://pingram_sk_abc123/id/user6@example.ca?bcc=invalid",
      "pingram://pingram_sk_abc123/id/user8@example.ca?cc=l2g@nuxref.com",
      "pingram://pingram_sk_abc123/id/user8@example.ca?channels=email,sms,slack,mobile_push,web_push,inapp,call",
      "pingram://pingram_sk_abc123/id/user9@example.ca?cc=Chris<l2g@nuxref.com>",
      "pingram://pingram_sk_abc123/id/user11@example.ca?to=invalid",
      "pingram://pingram_sk_abc123/id1/user12@example.ca?to=id,Chris<chris2@example.com>",
      "pingram://pingram_sk_abc123/id2/user13@example.ca/id/kris@example.com/id/chris2@example.com/id/+15552341234?:token=value",
      "pingram://pingram_sk_abc123/id/user14@example.ca?bcc=Chris<chris14@example.com>",
      "pingram://pingram_sk_abc123/id/user@example.ca?:sub=value&:sub2=value2",
      "pingram://pingram_sk_abc/+15551234567?mode=t",
      "pingram://pingram_sk_abc/+15551234567?region=EU&channels=SMS",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values below come from upstream's parsed objects
    let obj = parse("pingram://pingram_sk_abc123/g@rb@ge/+15551235553/");
    assert_eq!((obj.apikey.as_str(), obj.message_type.as_str(), obj.mode, obj.region), ("pingram_sk_abc123", "apprise", Mode::Message, "us"));
    assert_eq!(obj.targets, vec![target(None, None, Some("+15551235553"))]);
    assert_eq!(obj.channels, vec!["sms"]);

    let obj = parse("pingram://pingram_sk_abc123/g@rb@ge/");
    assert!(obj.targets.is_empty());
    assert!(obj.channels.is_empty());

    let obj = parse("pingram://pingram_sk_abc123/user1@example.com/user2@example.com");
    assert_eq!(obj.targets, vec![target(None, Some("user1@example.com"), None), target(None, Some("user2@example.com"), None)]);
    assert_eq!(obj.channels, vec!["email"]);

    // Upstream drops the second number's `+`; we keep it (apprise-go)
    let obj = parse("pingram://pingram_sk_abc123/+15551235553/+15551235554");
    assert_eq!(obj.targets, vec![target(None, None, Some("+15551235553")), target(None, None, Some("+15551235554"))]);

    let obj = parse("pingram://pingram_sk_abc123/id/id2/user1@example.com");
    assert_eq!(obj.targets, vec![target(Some("id"), None, None), target(Some("id2"), Some("user1@example.com"), None)]);

    let obj = parse("pingram://type@pingram_sk_abc123/id10/user2@example.com/id5/+15551235555/id8/+15551235534?reply=Chris<chris@example.com>");
    assert_eq!((obj.message_type.as_str(), obj.mode), ("type", Mode::Template));
    assert_eq!(
      obj.targets,
      vec![
        target(Some("id10"), Some("user2@example.com"), None),
        target(Some("id5"), None, Some("+15551235555")),
        target(Some("id8"), None, Some("+15551235534")),
      ]
    );
    assert_eq!(obj.channels, vec!["email"]);

    // A ?from= without an address only names the sender
    let obj = parse("pingram://type@pingram_sk_abc123/abc1/user1@example.com/id5/+15551235555/?from=Chris&reply=Christopher");
    assert_eq!((obj.from_email.as_deref(), obj.from_name.as_deref()), (None, Some("Chris")));

    let obj = parse("pingram://type@pingram_sk_abc123/user4@example.com/?from=joe@example.ca&bcc=user1@yahoo.ca&cc=user2@yahoo.ca");
    assert_eq!((obj.from_email.as_deref(), obj.from_name.as_deref()), (Some("joe@example.ca"), None));
    assert_eq!((obj.cc.clone(), obj.bcc.clone()), (vec!["user2@yahoo.ca".to_string()], vec!["user1@yahoo.ca".to_string()]));

    let obj = parse("pingram://?apikey=pingram_sk_abc123&to=id,user5@example.com&type=typec");
    assert_eq!((obj.apikey.as_str(), obj.message_type.as_str(), obj.mode), ("pingram_sk_abc123", "typec", Mode::Template));
    assert_eq!(obj.targets, vec![target(Some("id"), Some("user5@example.com"), None)]);

    let obj = parse("pingram://pingram_sk_abc123?to=id,user5@example.com&type=typeb");
    assert_eq!(obj.targets, vec![target(Some("id"), Some("user5@example.com"), None)]);

    let obj = parse("pingram://?apikey=pingram_sk_abc123&type=test-type&region=eu");
    assert_eq!((obj.region, obj.mode, obj.targets.len()), ("eu", Mode::Template, 0));

    let obj = parse("pingram://pingram_sk_abc123/id/user8@example.ca?channels=email,sms,slack,mobile_push,web_push,inapp,call");
    assert_eq!(obj.channels, CHANNELS.to_vec());

    // "invalid" is a valid recipient id
    let obj = parse("pingram://pingram_sk_abc123/id/user11@example.ca?to=invalid");
    assert_eq!(obj.targets, vec![target(Some("id"), Some("user11@example.ca"), None), target(Some("invalid"), None, None)]);

    let obj = parse("pingram://pingram_sk_abc123/id1/user12@example.ca?to=id,Chris<chris2@example.com>");
    assert_eq!(obj.targets, vec![target(Some("id1"), Some("user12@example.ca"), None), target(Some("id"), Some("chris2@example.com"), None)]);

    let obj = parse("pingram://pingram_sk_abc123/id2/user13@example.ca/id/kris@example.com/id/chris2@example.com/id/+15552341234?:token=value");
    assert_eq!(obj.targets.len(), 4);
    assert_eq!(obj.targets[3], target(Some("id"), None, Some("+15552341234")));
    assert_eq!(Value::Object(obj.tokens), json!({ "token": "value" }));

    assert_eq!(parse("pingram://pingram_sk_abc123/id/user14@example.ca?bcc=Chris<chris14@example.com>").bcc, vec!["chris14@example.com"]);
    assert!(parse("pingram://pingram_sk_abc123/id/user6@example.ca?bcc=invalid").bcc.is_empty());

    let obj = parse("pingram://pingram_sk_abc/+15551234567?mode=t");
    assert_eq!(obj.mode, Mode::Template);
    let obj = parse("pingram://pingram_sk_abc/+15551234567?region=EU&channels=SMS");
    assert_eq!((obj.region, obj.channels.clone()), ("eu", vec!["sms"]));
    assert_eq!(obj.api_base(), "https://api.eu.pingram.io");
    assert_eq!(parse("pingram://pingram_sk_abc/+1?region=ca").api_base(), "https://api.ca.pingram.io");
    assert_eq!(parse("pingram://pingram_sk_abc/+15551234567").api_base(), "https://api.pingram.io");

    // The API key keeps its case
    assert_eq!(parse("pingram://pingram_sk_AbCdEf/+15551234567").apikey, "pingram_sk_AbCdEf");

    // The `@` prefix marks an id; a formatted phone number is normalised
    let obj = parse("pingram://pingram_sk_abc/@john/%40jane/+1-555-123-4567");
    assert_eq!(obj.targets, vec![target(Some("john"), None, None), target(Some("jane"), None, Some("+15551234567"))]);
  }

  #[test]
  fn test_service_details() {
    let details = Pingram::static_details();
    assert_eq!(details.service_name, "Pingram");
    assert_eq!(details.protocols, vec!["pingram"]);
    assert!(!details.attachment_support);
    let svc = from_url("pingram://pingram_sk_abc/+15551234567").unwrap();
    assert_eq!(svc.notify_formats(), vec![NotifyFormat::Text, NotifyFormat::Html]);
    assert_eq!(svc.request_rate_per_sec(), 0.2);
    assert_eq!(svc.url_identifier().unwrap(), vec!["pingram", "pingram_sk_abc"]);
  }

  #[tokio::test]
  async fn test_template_sms_payload() {
    let server = MockServer::start().await;
    // Upstream's test_plugin_pingram_template_sms_payloads
    Mock::given(method("POST"))
      .and(path("/send"))
      .and(header("Authorization", "Bearer pingram_sk_my_key"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({
        "type": "apprise-post",
        "to": { "id": "userid", "number": "+15551234567" },
        "parameters": {
          "appBody": "body",
          "appTitle": "title",
          "appType": "info",
          "appId": "Apprise",
          "appDescription": "Apprise Notifications",
          "appColor": "#3AA3E3",
          "appImageUrl": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png",
          "appUrl": "https://github.com/caronc/apprise",
        },
      })))
      .respond_with(ResponseTemplate::new(200).set_body_string(GOOD))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("pingram://apprise-post@pingram_sk_my_key/userid/+1-555-123-4567?mode=template").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title", "body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_template_email_payload() {
    let (ok, sent) = send(
      "pingram://apprise-post@pingram_sk_my_key_abc/userid/test@example.ca\
       ?from=Chris<chris@example.eu>&bcc=joe@hidden.com&cc=jason@hidden.com&:customToken=customValue&mode=template",
      &ctx("title", "body"),
    )
    .await;
    assert!(ok);
    assert_eq!(
      sent,
      vec![json!({
        "type": "apprise-post",
        "to": { "id": "userid", "email": "test@example.ca" },
        "options": { "email": {
          "fromAddress": "chris@example.eu",
          "fromName": "Chris",
          "ccAddresses": ["jason@hidden.com"],
          "bccAddresses": ["joe@hidden.com"],
        }},
        "parameters": {
          "customToken": "customValue",
          "appBody": "body",
          "appTitle": "title",
          "appType": "info",
          "appId": "Apprise",
          "appDescription": "Apprise Notifications",
          "appColor": "#3AA3E3",
          "appImageUrl": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png",
          "appUrl": "https://github.com/caronc/apprise",
        },
      })]
    );
  }

  #[tokio::test]
  async fn test_message_payloads() {
    // Upstream's test_plugin_pingram_message_payloads
    let (ok, sent) = send(
      "pingram://apprise-post@pingram_sk_my_key_abc/userid/test@example.ca/+15551239876?from=Chris<chris@example.eu>&bcc=joe@hidden.com&mode=message",
      &ctx("title", "body"),
    )
    .await;
    assert!(ok);
    assert_eq!(
      sent,
      vec![json!({
        "type": "apprise-post",
        "to": { "id": "userid", "email": "test@example.ca", "number": "+15551239876" },
        "email": { "subject": "title", "html": "body", "senderName": "Chris", "senderEmail": "chris@example.eu" },
        "options": { "email": { "fromAddress": "chris@example.eu", "fromName": "Chris", "bccAddresses": ["joe@hidden.com"] } },
      })]
    );

    // A number first makes SMS the default channel
    let (_, sent) =
      send("pingram://pingram_sk_my_key_abc/userid/+15551239876/test@example.ca?from=Chris<chris@example.eu>&bcc=joe@hidden.com", &ctx("title", "body")).await;
    assert_eq!(
      sent,
      vec![json!({
        "type": "apprise",
        "to": { "id": "userid", "number": "+15551239876", "email": "test@example.ca" },
        "sms": { "message": "title\nbody" },
        "options": { "email": { "fromAddress": "chris@example.eu", "fromName": "Chris", "bccAddresses": ["joe@hidden.com"] } },
      })]
    );

    let (_, sent) = send(
      "pingram://apprise-post@pingram_sk_my_key_abc/userid/+15551239876/test@example.ca\
       ?from=Chris<chris@example.eu>&bcc=joe@hidden.com&mode=message&channels=sms,slack,call",
      &ctx("title", "body"),
    )
    .await;
    assert_eq!(
      sent,
      vec![json!({
        "type": "apprise-post",
        "to": { "id": "userid", "email": "test@example.ca", "number": "+15551239876" },
        "slack": { "text": "title\nbody" },
        "sms": { "message": "title\nbody" },
        "call": { "message": "title\nbody" },
        "options": { "email": { "fromAddress": "chris@example.eu", "fromName": "Chris", "bccAddresses": ["joe@hidden.com"] } },
      })]
    );
  }

  #[tokio::test]
  async fn test_every_channel() {
    // Matches pycapture.py with --input-format text
    let c = NotifyContext { body_format: NotifyFormat::Text, ..ctx("title", "body") };
    let (ok, sent) = send("pingram://pingram_sk_abc123/id/user8@example.ca?channels=email,sms,slack,mobile_push,web_push,inapp,call&cc=a@b.com", &c).await;
    assert!(ok);
    let icon = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png";
    assert_eq!(
      sent,
      vec![json!({
        "type": "apprise",
        "email": { "subject": "title", "html": "body" },
        "slack": { "text": "title\nbody" },
        "call": { "message": "title\nbody" },
        "sms": { "message": "title\nbody" },
        "web_push": { "title": "title", "message": "body", "icon": icon },
        "mobile_push": { "title": "title", "message": "body" },
        "inapp": { "title": "title", "image": icon },
        "options": { "email": { "ccAddresses": ["a@b.com"] } },
        "to": { "id": "id", "email": "user8@example.ca" },
      })]
    );
  }

  #[tokio::test]
  async fn test_message_formats() {
    // Text: the email part is converted to HTML
    let c = NotifyContext { body_format: NotifyFormat::Text, ..ctx("apprise parity", "hello from python") };
    let (_, sent) = send("pingram://pingram_sk_abc123/john@example.com", &c).await;
    // The apprise-go golden
    assert_eq!(
      sent[0],
      json!({ "type": "apprise", "email": { "subject": "apprise parity", "html": "hello&nbsp;from&nbsp;python" }, "to": { "email": "john@example.com" } })
    );

    // HTML: SMS gets a text version, email the body as given; an empty
    // title becomes the app ID
    let c = NotifyContext { body_format: NotifyFormat::Html, ..ctx("", "<b>hi</b>") };
    let (_, sent) = send("pingram://pingram_sk_abc123/id/+15551234567?channels=email,sms", &c).await;
    assert_eq!(sent[0]["email"], json!({ "subject": "Apprise", "html": "<b>hi</b>" }));
    assert_eq!(sent[0]["sms"], json!({ "message": "hi" }));

    // Through the orchestrator, Markdown input is sent as text, markup
    // kept, as pycapture.py shows
    let server = mock(200, GOOD).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("pingram://pingram_sk_abc123/+15551234567"));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("T", "**bold** text") };
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    assert_eq!(payloads(&server).await[0]["sms"], json!({ "message": "T\n**bold** text" }));
  }

  #[tokio::test]
  async fn test_channels_and_types() {
    // The apprise-go golden for ?channels=slack,inapp
    let c = NotifyContext { notify_type: NotifyType::Success, ..ctx("apprise parity", "hello from python") };
    let (_, sent) = send("pingram://pingram_sk_abc123/@john?channels=slack,inapp", &c).await;
    assert_eq!(
      sent[0],
      json!({
        "inapp": { "image": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-success-72x72.png", "title": "apprise parity" },
        "slack": { "text": "apprise parity\nhello from python" },
        "to": { "id": "john" },
        "type": "apprise",
      })
    );

    // An id alone with no channel sends just the type
    let (_, sent) = send("pingram://pingram_sk_abc123/@john", &c).await;
    assert_eq!(sent[0], json!({ "type": "apprise", "to": { "id": "john" } }));
  }

  #[tokio::test]
  async fn test_cc_per_recipient() {
    // Each recipient gets its own lists, without itself in them. Upstream
    // shares one options dict between recipients, so b@example.com is also
    // sent the cc meant for a@example.com
    let (ok, sent) = send("pingram://pingram_sk_abc123/a@example.com/b@example.com?cc=b@example.com,c@example.com&bcc=c@example.com", &ctx("t", "b")).await;
    assert!(ok);
    assert_eq!(sent[0]["options"], json!({ "email": { "ccAddresses": ["b@example.com"], "bccAddresses": ["c@example.com"] } }));
    assert_eq!(sent[1]["options"], json!({ "email": { "bccAddresses": ["c@example.com"] } }));
  }

  #[tokio::test]
  async fn test_multiple_numbers() {
    let (ok, sent) = send("pingram://pingram_sk_abc123/+15551234567/+15551234568", &ctx("title", "body")).await;
    assert!(ok);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], json!({ "type": "apprise", "to": { "number": "+15551234567" }, "sms": { "message": "title\nbody" } }));
    assert_eq!(sent[1]["to"], json!({ "number": "+15551234568" }));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("pingram://pingram_sk_abc123/g@rb@ge/").unwrap();
    let server = mock(200, GOOD).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert!(payloads(&server).await.is_empty());

    // A bad status, an unknown one, and a 200 that is not JSON
    for (status, body) in [(500, GOOD), (999, GOOD), (200, "{"), (200, "")] {
      let server = mock(status, body).await;
      let svc = from_url("pingram://pingram_sk_abc123/id/user@example.ca").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), "status {} body {:?}", status, body);
    }

    let svc = from_url("pingram://pingram_sk_abc123/id/user@example.ca").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:9", svc.send(&ctx("t", "b"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_recipients() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/send"))
      .and(wiremock::matchers::body_partial_json(json!({ "to": { "number": "+15551234568" } })))
      .respond_with(ResponseTemplate::new(500).set_body_string(GOOD))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/send")).respond_with(ResponseTemplate::new(200).set_body_string(GOOD)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("pingram://pingram_sk_abc123/+15551234567/+15551234568?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("t", "b"))).await.is_success());
    let numbers: Vec<Value> = payloads(&server).await.iter().map(|p| p["to"]["number"].clone()).collect();
    assert_eq!(numbers, vec![json!("+15551234567"), json!("+15551234568"), json!("+15551234568")]);
  }
}

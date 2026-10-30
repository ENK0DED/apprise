use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;
use serde_json::json;

/// Upstream's default `AppriseAsset.image_url_mask`.
const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

/// LINE Messaging API push: `line://{token}/{user_id}/...`.
pub struct Line {
  token: String,
  targets: Vec<String>,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace, then
/// drop empty entries, de-duplicate and sort.
fn parse_list<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
  let mut out: Vec<String> = items
    .into_iter()
    .flat_map(|s| s.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()))
    .filter(|s| !s.is_empty())
    .map(String::from)
    .collect();
  out.sort();
  out.dedup();
  out
}

/// Upstream `AppriseAsset.image_url()` at the 128x128 size LINE uses.
fn image_url(asset: &AppriseAsset, notify_type: &NotifyType) -> Option<String> {
  let mask = asset.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
  if mask.is_empty() {
    return None;
  }
  Some(mask.replace("{THEME}", &asset.theme).replace("{TYPE}", notify_type.as_str()).replace("{XY}", "128x128").replace("{EXTENSION}", ".png"))
}

impl Line {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let mut path: Vec<String> = url.path_parts.clone();
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => t.to_string(),
      None => {
        let mut token = urlencoding::decode(url.host.as_deref().unwrap_or_default()).map(|s| s.into_owned()).unwrap_or_default();
        // Access tokens can contain slashes. When the host does not end the
        // token (with its base64 '=' padding), the path up to the first
        // entry that ends in '=' still belongs to it.
        if !token.ends_with('=')
          && let Some(index) = path.iter().position(|e| e.ends_with('='))
        {
          let rest = path.split_off(index + 1);
          token = format!("{}/{}", token, path.join("/"));
          path = rest;
        }
        token
      }
    };
    let token = token.trim().to_string();
    if token.is_empty() {
      tracing::warn!("An invalid LINE Access Token was specified.");
      return None;
    }

    let mut raw: Vec<String> = path;
    if let Some(to) = url.get("to") {
      raw.extend(to.split([' ', '\t', '\r', '\n', ',', '#', '\\', '/']).filter(|s| !s.is_empty()).map(String::from));
    }
    let targets = parse_list(raw.iter().map(String::as_str));
    let include_image = url.get("image").map(parse_bool).unwrap_or(true);

    Some(Self { token, targets, include_image, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Line",
      service_url: Some("https://line.me/"),
      setup_url: Some("https://appriseit.com/services/line/"),
      protocols: vec!["line"],
      description: "Send LINE messages via bot.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Line {
  fn schemas(&self) -> &[&str] {
    &["line"]
  }
  fn service_name(&self) -> &str {
    "Line"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    5000
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Line targets to notify.");
      return Ok(false);
    }
    let mut sender = json!({ "name": ctx.asset.app_id });
    if self.include_image
      && let Some(icon) = image_url(&ctx.asset, &ctx.notify_type)
    {
      sender["iconUrl"] = json!(icon);
    }

    let url = format!("{}/v2/bot/message/push", service_base("https://api.line.me"));
    let client = build_client(self.verify_certificate)?;
    let mut all_ok = true;
    for target in &self.targets {
      let payload = json!({ "to": target, "messages": [{ "type": "text", "text": ctx.body, "sender": sender }] });
      let resp = match client.post(&url).header("User-Agent", APP_ID).bearer_auth(&self.token).json(&payload).send().await {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Line notification to {}: {}", target, e);
          all_ok = false;
          continue;
        }
      };
      if resp.status() == reqwest::StatusCode::OK {
        tracing::info!("Sent Line notification to {}.", target);
      } else {
        tracing::warn!("Failed to send Line notification to {}: error={}", target, resp.status().as_u16());
        all_ok = false;
      }
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_line(url: &str) -> Option<Line> {
    ParsedUrl::parse(url).and_then(|p| Line::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "line://token",
      "line://token=/target",
      "line://token/target?image=no",
      "line://a/very/long/token=/target?image=no",
      "line://?token=token&to=target1",
      "line://token/target",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["line://", "line://%20/"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse_line("line://token").unwrap();
    assert_eq!(obj.token, "token");
    assert!(obj.targets.is_empty());
    assert!(obj.include_image);

    let obj = parse_line("line://token=/target?image=no").unwrap();
    assert_eq!(obj.token, "token=");
    assert_eq!(obj.targets, vec!["target"]);
    assert!(!obj.include_image);

    let obj = parse_line("line://?token=token&to=target1").unwrap();
    assert_eq!(obj.token, "token");
    assert_eq!(obj.targets, vec!["target1"]);
  }

  #[test]
  fn test_token_with_slashes() {
    // The path up to the first entry ending in '=' belongs to the token
    let obj = parse_line("line://a/very/long/token=/target").unwrap();
    assert_eq!(obj.token, "a/very/long/token=");
    assert_eq!(obj.targets, vec!["target"]);

    // Without an '=' the host alone is the token
    let obj = parse_line("line://token/t1/t2").unwrap();
    assert_eq!(obj.token, "token");
    assert_eq!(obj.targets, vec!["t1", "t2"]);
  }

  #[test]
  fn test_targets_sorted_and_unique() {
    let obj = parse_line("line://a/very/long/token=/tgtB/tgtA?to=x1%23x2,tgtA").unwrap();
    assert_eq!(obj.targets, vec!["tgtA", "tgtB", "x1", "x2"]);
  }

  #[test]
  fn test_service_details() {
    let details = Line::static_details();
    assert_eq!(details.service_name, "Line");
    assert_eq!(details.protocols, vec!["line"]);
    let svc = from_url("line://token/target").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 5000);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    let icon = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-128x128.png";
    for target in ["tgtA", "tgtB"] {
      Mock::given(method("POST"))
        .and(path("/v2/bot/message/push"))
        .and(header("Authorization", "Bearer a/very/long/token="))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({
          "to": target,
          "messages": [{ "type": "text", "text": "T\r\nB", "sender": { "name": "Apprise", "iconUrl": icon } }],
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("line://a/very/long/token=/tgtB/tgtA").unwrap();
    let ctx = NotifyContext { body: "T\r\nB".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({ "to": "t1", "messages": [{ "type": "text", "text": "B", "sender": { "name": "Apprise" } }] })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("line://tok/t1?image=no").unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No targets: nothing is sent
    let svc = from_url("line://token").unwrap();
    assert!(!svc.send(&NotifyContext::default()).await.unwrap());

    // One target failing still tries the others and reports failure
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(body_json_to("t1")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(body_json_to("t2")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("line://token/t1/t2").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap());
  }

  fn body_json_to(target: &'static str) -> impl wiremock::Match {
    move |req: &wiremock::Request| serde_json::from_slice::<serde_json::Value>(&req.body).map(|v| v["to"] == target).unwrap_or(false)
  }
}

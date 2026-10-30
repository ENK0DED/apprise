use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// Native webhook URL prefix (upstream `parse_native_url`).
const NATIVE_PREFIX: &str = "https://open.larksuite.com/open-apis/bot/v2/hook/";

/// Lark bot webhook: `lark://{token}`.
pub struct Lark {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Lark {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = if url.schema == "https" || url.schema == "http" {
      // https://open.larksuite.com/open-apis/bot/v2/hook/{token}, nothing after
      let prefix = url.raw.get(..NATIVE_PREFIX.len())?;
      if !prefix.eq_ignore_ascii_case(NATIVE_PREFIX) {
        return None;
      }
      let rest = &url.raw[NATIVE_PREFIX.len()..];
      if rest.is_empty() || !rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return None;
      }
      rest.to_string()
    } else {
      match url.get("token").filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => urlencoding::decode(url.host.as_deref().unwrap_or_default()).ok()?.into_owned(),
      }
    };
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
      tracing::warn!("The Lark Bot Token specified is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Lark (Feishu)",
      service_url: Some("https://open.larksuite.com/"),
      setup_url: Some("https://appriseit.com/services/lark/"),
      protocols: vec!["lark"],
      description: "Send via Lark (Feishu international) webhooks.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Lark {
  fn schemas(&self) -> &[&str] {
    &["lark"]
  }
  fn service_name(&self) -> &str {
    "Lark (Feishu)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    19985
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/open-apis/bot/v2/hook/{}", service_base("https://open.larksuite.com"), self.token);
    let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
    let payload = json!({ "msg_type": "text", "content": { "text": text } });
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("Lark notification sent successfully.");
      Ok(true)
    } else {
      tracing::warn!("Lark notification failed: {}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_lark(url: &str) -> Option<Lark> {
    ParsedUrl::parse(url).and_then(|p| Lark::from_url(&p))
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "lark://",
      "lark://:@/",
      "lark://abc_def",
      "lark://?token=a.b",
      // Native URLs must end with the token
      "https://open.larksuite.com/open-apis/bot/v2/hook/",
      "https://open.larksuite.com/open-apis/bot/v2/hook/abcd-1234/",
      "https://open.larksuite.com/open-apis/bot/v2/hook/abcd-1234?x=y",
      "https://open.larksuite.com/other/abcd-1234",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let a80 = format!("lark://{}", "a".repeat(80));
    let urls = vec![
      "lark://abcd-1234",
      "lark://?token=abcd-1234",
      "https://open.larksuite.com/open-apis/bot/v2/hook/abcd-1234",
      "HTTPS://OPEN.LARKSUITE.COM/open-apis/bot/v2/hook/abcd-1234",
      &a80,
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_token() {
    assert_eq!(parse_lark("lark://abcd-1234").unwrap().token, "abcd-1234");
    assert_eq!(parse_lark("lark://host?token=abcd-1234").unwrap().token, "abcd-1234");
    assert_eq!(parse_lark("https://open.larksuite.com/open-apis/bot/v2/hook/abcd-1234").unwrap().token, "abcd-1234");
  }

  #[test]
  fn test_service_details() {
    let details = Lark::static_details();
    assert_eq!(details.service_name, "Lark (Feishu)");
    assert_eq!(details.protocols, vec!["lark"]);
    assert_eq!(from_url("lark://abcd-1234").unwrap().body_maxlen(), 19985);
  }

  async fn send_expecting(ctx: &NotifyContext, text: &str) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/open-apis/bot/v2/hook/abcd-1234"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "msg_type": "text", "content": { "text": text } })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("lark://abcd-1234").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send() {
    send_expecting(&NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() }, "T\nB").await;
    send_expecting(&NotifyContext { body: "B".into(), ..Default::default() }, "B").await;
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url("lark://abcd-1234").unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err());
    }
  }
}

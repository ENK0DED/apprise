use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{qsd_bool, unquote};
use async_trait::async_trait;

pub struct PushMe {
  token: String,
  status: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `AppriseAsset.ascii()`.
fn ascii(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info => "[i]",
    NotifyType::Success => "[+]",
    NotifyType::Failure => "[!]",
    NotifyType::Warning => "[~]",
  }
}

impl PushMe {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ?token= (or its push_key= alias) wins over the host
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(str::to_string);
    let token = qsd("token").or_else(|| qsd("push_key")).or_else(|| url.host.as_deref().map(unquote)).unwrap_or_default();
    let Some(token) = token.split_whitespace().next().map(str::to_string) else {
      tracing::warn!("An invalid PushMe Token was specified.");
      return None;
    };
    Some(Self { token, status: qsd_bool(url, "status", true), verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "PushMe",
      service_url: Some("https://push.i-i.me/"),
      setup_url: Some("https://appriseit.com/services/pushme/"),
      protocols: vec!["pushme"],
      description: "Send notifications via PushMe.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for PushMe {
  fn schemas(&self) -> &[&str] {
    &["pushme"]
  }
  fn service_name(&self) -> &str {
    "PushMe"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // status= prefixes the title with the notification type
    let title = if self.status { format!("{} {}", ascii(&ctx.notify_type), ctx.title) } else { ctx.title.clone() };
    let params = [
      ("push_key", self.token.as_str()),
      ("title", title.as_str()),
      ("content", ctx.body.as_str()),
      ("type", if ctx.body_format == NotifyFormat::Markdown { "markdown" } else { "text" }),
    ];
    // Everything travels in the query string of an empty POST
    let url = format!("{}/", service_base("https://push.i-i.me"));
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).query(&params).send().await?;
    let status = resp.status();
    if status.as_u16() == 200 {
      tracing::info!("Sent PushMe notification.");
      return Ok(true);
    }
    let reason = status.canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send PushMe notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
    Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    let valid = [
      format!("pushme://{}", "a".repeat(6)),
      format!("pushme://?token={}&status=yes", "b".repeat(6)),
      format!("pushme://?token={}&status=no", "b".repeat(6)),
      format!("pushme://?token={}&status=True", "b".repeat(6)),
      format!("pushme://?push_key={}&status=no", "p".repeat(6)),
    ];
    for url in &valid {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    for url in ["pushme://", "pushme://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    let obj = PushMe::from_url(&ParsedUrl::parse("pushme://host?push_key=pk&status=no").unwrap()).unwrap();
    assert_eq!((obj.token.as_str(), obj.status), ("pk", false));
    assert!(PushMe::from_url(&ParsedUrl::parse("pushme://tok").unwrap()).unwrap().status);
    // An unrecognised value turns the status off, as upstream parses it
    assert!(!PushMe::from_url(&ParsedUrl::parse("pushme://tok?status=random").unwrap()).unwrap().status);
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py: a warning gets "[~] " in front of the title
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(query_param("push_key", "tok123"))
      .and(query_param("title", "[~] T"))
      .and(query_param("content", "B"))
      .and(query_param("type", "text"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), from_url("pushme://tok123").unwrap().send(&ctx)).await.unwrap());
    assert!(server.received_requests().await.unwrap()[0].body.is_empty());
  }

  #[tokio::test]
  async fn test_send_markdown_without_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(query_param("title", ""))
      .and(query_param("type", "markdown"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let ctx = NotifyContext { body: "B".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), from_url("pushme://tok123?status=no").unwrap().send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), from_url("pushme://tok").unwrap().send(&ctx)).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = PushMe::static_details();
    assert_eq!(details.service_name, "PushMe");
    assert_eq!(details.protocols, vec!["pushme"]);
  }
}

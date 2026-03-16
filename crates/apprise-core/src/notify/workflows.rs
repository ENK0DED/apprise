use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct Workflows {
  workflow_url: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Workflows {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    if host.is_empty() {
      return None;
    }
    // Need at least 2 path parts (workflow_id + signature) or query params with id/signature
    let has_query_workflow = url.get("id").is_some() || url.get("workflow").is_some();
    let has_query_sig = url.get("signature").is_some();
    if url.path_parts.len() < 2 && !has_query_workflow && !has_query_sig {
      return None;
    }
    // Validate path parts — reject special chars
    for pp in &url.path_parts {
      if pp.contains('^') || pp.contains('(') || pp.contains(')') {
        return None;
      }
    }
    let path = if url.path.is_empty() { String::new() } else { format!("/{}", url.path) };
    let workflow_url = format!("https://{}{}", host, path);
    Some(Self { workflow_url, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Microsoft Workflows",
      service_url: Some("https://make.powerautomate.com"),
      setup_url: None,
      protocols: vec!["workflow", "workflows"],
      description: "Send via Microsoft Power Automate Workflows.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Workflows {
  fn schemas(&self) -> &[&str] {
    &["workflow", "workflows"]
  }
  fn service_name(&self) -> &str {
    "Microsoft Workflows"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({ "title": ctx.title, "text": ctx.body, "type": ctx.notify_type.to_string() });
    let resp = client.post(&self.workflow_url).header("User-Agent", APP_ID).json(&payload).send().await?;
    Ok(resp.status().is_success())
  }
}

#[cfg(test)]
mod tests {
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "workflow://host:443/workflow1a/signature/?image=no",
      "workflows://host:443/workflow1b/signature/",
      "workflows://host:443/signature/?id=workflow1c",
      "workflows://host:443/signature/?workflow=workflow1d&wrap=yes",
      "workflows://host:443/signature/?workflow=workflow1d&wrap=no",
      "workflows://host:443/workflow1e/signature/?api-version=2024-01-01",
      "workflows://host:443/workflow1b/signature/?ver=2016-06-01",
      "workflows://host:443/?id=workflow1b&signature=signature",
      "workflows://host:443/workflow1e/signature/?powerautomate=yes",
      "workflows://host:443/workflow1e/signature/?pa=yes&ver=1995-01-01",
      "workflows://host:443/workflow1e/signature/?pa=yes",
      "https://server.azure.com:443/workflows/643e69f83c8944/triggers/manual/paths/invoke?api-version=2016-06-01&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=KODuebWbDGYFr0z0eu",
      "https://server.azure.com:443/powerautomate/automations/direct/workflows/643e69f83c8944/triggers/manual/paths/invoke?api-version=2022-03-01-preview&sp=%2Ftriggers%2Fmanual%2Frun&sv=1.0&sig=KODuebWbDGYFr0z0eu",
      "workflow://host:443/workflow4/signature/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["workflow://", "workflow://:@/", "workflow://host/workflow", "workflow://host:443/^(/signature"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  // ── Behavioral tests using wiremock ──────────────────────────────────

  use super::*;
  use crate::asset::AppriseAsset;
  use crate::notify::{Notify, NotifyContext};
  use crate::types::{NotifyFormat, NotifyType};
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext {
      body: body.to_string(),
      title: title.to_string(),
      notify_type: NotifyType::Info,
      body_format: NotifyFormat::Text,
      attachments: vec![],
      interpret_escapes: false,
      interpret_emojis: false,
      tags: vec![],
      asset: AppriseAsset::default(),
    }
  }

  fn make_workflows(server: &MockServer) -> Workflows {
    let addr = server.address();
    let base = format!("http://127.0.0.1:{}", addr.port());
    Workflows { workflow_url: format!("{}/workflows/wf1/triggers/manual/paths/invoke", base), verify_certificate: false, tags: vec![] }
  }

  #[tokio::test]
  async fn test_workflows_basic_send_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/workflows/wf1/triggers/manual/paths/invoke"))
      .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
      .expect(1)
      .mount(&server)
      .await;

    let wf = make_workflows(&server);
    let result = wf.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "Workflows POST should succeed");
  }

  #[tokio::test]
  async fn test_workflows_http_500_returns_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/workflows/wf1/triggers/manual/paths/invoke"))
      .respond_with(ResponseTemplate::new(500).set_body_string("error"))
      .expect(1)
      .mount(&server)
      .await;

    let wf = make_workflows(&server);
    let result = wf.send(&ctx("title", "body")).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "HTTP 500 should return false");
  }

  #[tokio::test]
  async fn test_workflows_connection_refused_returns_error() {
    let wf = Workflows { workflow_url: "http://127.0.0.1:19999/workflows/wf1/invoke".to_string(), verify_certificate: false, tags: vec![] };
    let result = wf.send(&ctx("title", "body")).await;
    assert!(result.is_err(), "Connection refused should return Err");
  }

  #[tokio::test]
  async fn test_workflows_payload_contains_title_and_body() {
    use wiremock::matchers::body_json;
    let server = MockServer::start().await;
    let expected = serde_json::json!({
        "title": "test title",
        "text": "test body",
        "type": "info",
    });
    Mock::given(method("POST"))
      .and(path("/workflows/wf1/triggers/manual/paths/invoke"))
      .and(body_json(&expected))
      .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
      .expect(1)
      .mount(&server)
      .await;

    let wf = make_workflows(&server);
    let result = wf.send(&ctx("test title", "test body")).await;
    assert!(result.is_ok());
    assert!(result.unwrap());
  }

  #[test]
  fn test_workflows_from_url_struct_fields() {
    let parsed = ParsedUrl::parse("workflow://host:443/workflow1a/signature/?image=no").unwrap();
    let wf = Workflows::from_url(&parsed).unwrap();
    assert!(wf.workflow_url.contains("host"));
    assert!(wf.workflow_url.contains("workflow1a"));
  }

  #[test]
  fn test_workflows_static_details() {
    let details = Workflows::static_details();
    assert_eq!(details.service_name, "Microsoft Workflows");
    assert_eq!(details.protocols, vec!["workflow", "workflows"]);
    assert!(!details.attachment_support);
  }
}

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
pub struct Notifico {
  project_id: String,
  msghook: String,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Notifico {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // notifico://project_id/msghook
    // or https://n.tkte.ch/h/project_id/msghook
    let (project_id, msghook) = if url.schema == "https" || url.schema == "http" {
      // https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj
      // path_parts: ["h", "2144", "uJmKaBW9WFk42miB146ci3Kj"]
      let h_idx = url.path_parts.iter().position(|p| p == "h")?;
      let pid = url.path_parts.get(h_idx + 1)?.clone();
      let mh = url.path_parts.get(h_idx + 2)?.clone();
      (pid, mh)
    } else {
      let pid = url.host.clone()?;
      let mh = url.path_parts.first()?.clone();
      (pid, mh)
    };
    // Project ID must be numeric
    if project_id.is_empty() || !project_id.chars().all(|c| c.is_ascii_digit()) {
      return None;
    }
    Some(Self { project_id, msghook, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notifico",
      service_url: Some("https://notico.re"),
      setup_url: None,
      protocols: vec!["notifico"],
      description: "Send IRC notifications via Notifico.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Notifico {
  fn schemas(&self) -> &[&str] {
    &["notifico"]
  }
  fn service_name(&self) -> &str {
    "Notifico"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
    let url = format!("https://notico.re/api/{}/{}", self.project_id, self.msghook);
    let client = build_client(self.verify_certificate)?;
    let resp = client.get(&url).header("User-Agent", APP_ID).query(&[("msg", msg.as_str())]).send().await?;
    if resp.status().is_success() {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "notifico://1234/ckhrjW8w672m6HG",
      "notifico://1234/ckhrjW8w672m6HG?prefix=no",
      "notifico://1234/ckhrjW8w672m6HG?color=yes",
      "notifico://1234/ckhrjW8w672m6HG?color=no",
      "https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["notifico://", "notifico://:@/", "notifico://1234", "notifico://abcd/ckhrjW8w672m6HG"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = ParsedUrl::parse("notifico://1234/ckhrjW8w672m6HG").expect("parse");
    let n = Notifico::from_url(&parsed).expect("from_url");
    assert_eq!(n.project_id, "1234");
    assert_eq!(n.msghook, "ckhrjW8w672m6HG");
  }

  #[test]
  fn test_native_url_parsing() {
    let parsed = ParsedUrl::parse("https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj").expect("parse");
    let n = Notifico::from_url(&parsed).expect("from_url");
    assert_eq!(n.project_id, "2144");
    assert_eq!(n.msghook, "uJmKaBW9WFk42miB146ci3Kj");
  }

  #[test]
  fn test_project_id_must_be_numeric() {
    // Non-numeric project id
    assert!(from_url("notifico://abcd/ckhrjW8w672m6HG").is_none());
    // Numeric project id
    assert!(from_url("notifico://1234/ckhrjW8w672m6HG").is_some());
  }

  #[test]
  fn test_missing_msghook() {
    // Only project id, no message hook
    assert!(from_url("notifico://1234").is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Notifico::static_details();
    assert_eq!(details.service_name, "Notifico");
    assert!(details.protocols.contains(&"notifico"));
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_api_endpoint_format() {
    // Verify the endpoint would be correct
    let parsed = ParsedUrl::parse("notifico://1234/ckhrjW8w672m6HG").expect("parse");
    let n = Notifico::from_url(&parsed).expect("from_url");
    let expected_url = format!("https://notico.re/api/{}/{}", n.project_id, n.msghook);
    assert_eq!(expected_url, "https://notico.re/api/1234/ckhrjW8w672m6HG");
  }
}

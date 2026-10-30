use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

/// Native push URL prefix (upstream `parse_native_url`).
const NATIVE_PREFIX: &str = "https://qmsg.zendee.cn/send/";

/// QQ Push (Qmsg): `qq://{token}`.
pub struct Qq {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Qq {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = if url.schema == "https" || url.schema == "http" {
      // https://qmsg.zendee.cn/send/{token}, nothing after
      let prefix = url.raw.get(..NATIVE_PREFIX.len())?;
      if !prefix.eq_ignore_ascii_case(NATIVE_PREFIX) {
        return None;
      }
      url.raw[NATIVE_PREFIX.len()..].to_string()
    } else {
      match url.get("token").filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => urlencoding::decode(url.host.as_deref().unwrap_or_default()).ok()?.into_owned(),
      }
    };
    if !(24..=64).contains(&token.len()) || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("The QQ Push token is invalid.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "QQ Push",
      service_url: Some("https://github.com/songquanpeng/message-pusher"),
      setup_url: Some("https://appriseit.com/services/qq/"),
      protocols: vec!["qq"],
      description: "Send notifications via QQ Push (Qmsg).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Qq {
  fn schemas(&self) -> &[&str] {
    &["qq"]
  }
  fn service_name(&self) -> &str {
    "QQ Push"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
    let url = format!("{}/send/{}", service_base("https://qmsg.zendee.cn"), self.token);
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(&url).header("User-Agent", APP_ID).form(&[("msg", msg.as_str())]).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
      tracing::info!("QQ Push notification sent successfully.");
      Ok(true)
    } else {
      tracing::warn!("QQ Push notification failed: {}", status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "abc123def456ghi789jkl012mno345pq";

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("qq://{}", TOKEN),
      format!("qq://?token={}", TOKEN),
      format!("https://qmsg.zendee.cn/send/{}", TOKEN),
      format!("HTTPS://QMSG.ZENDEE.CN/send/{}", TOKEN),
      format!("qq://{}", "f".repeat(64)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "qq://".to_string(),
      "qq://invalid!".to_string(),
      // Too short and too long
      format!("qq://{}", "a".repeat(23)),
      format!("qq://{}", "a".repeat(65)),
      // Native URLs must end with the token
      format!("https://qmsg.zendee.cn/send/{}/", TOKEN),
      format!("https://qmsg.zendee.cn/send/{}?x=y", TOKEN),
      format!("https://qmsg.zendee.cn/other/{}", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let q = Qq::from_url(&ParsedUrl::parse(&format!("qq://{}", TOKEN)).unwrap()).unwrap();
    assert_eq!(q.token, TOKEN);
    let q = Qq::from_url(&ParsedUrl::parse(&format!("qq://{}?token={}", "b".repeat(30), TOKEN)).unwrap()).unwrap();
    assert_eq!(q.token, TOKEN);
    let q = Qq::from_url(&ParsedUrl::parse(&format!("https://qmsg.zendee.cn/send/{}", TOKEN)).unwrap()).unwrap();
    assert_eq!(q.token, TOKEN);
  }

  #[test]
  fn test_static_details() {
    let details = Qq::static_details();
    assert_eq!(details.service_name, "QQ Push");
    assert!(details.protocols.contains(&"qq"));
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/send/{}", TOKEN)))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("msg=T%0AB"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("qq://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("qq://{}", TOKEN)).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err());
    }
  }
}

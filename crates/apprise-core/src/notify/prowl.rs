use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{split_path, unquote};
use async_trait::async_trait;

/// Upstream `PROWL_PRIORITY_MAP`, checked in order against the start of the
/// lower-cased `?priority=` value.
const PROWL_PRIORITY_MAP: &[(&str, i32)] = &[("l", -2), ("m", -1), ("n", 0), ("h", 1), ("e", 2), ("-2", -2), ("-1", -1), ("0", 0), ("1", 1), ("2", 2)];

pub struct Prowl {
  apikey: String,
  providerkey: Option<String>,
  priority: i32,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Prowl API and provider keys are 40 alphanumeric characters.
fn is_prowl_key(key: &str) -> bool {
  key.len() == 40 && key.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Prowl {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = unquote(url.host.as_deref()?).trim().to_string();
    if !is_prowl_key(&apikey) {
      tracing::warn!("An invalid Prowl API Key was specified.");
      return None;
    }

    // Only the first path entry is the provider key
    let providerkey = split_path(url).into_iter().next().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    if let Some(key) = &providerkey
      && !is_prowl_key(key)
    {
      tracing::warn!("An invalid Prowl Provider Key was specified.");
      return None;
    }

    // Unknown priorities fall back to normal (0), as upstream does
    let priority = url
      .get("priority")
      .filter(|p| !p.is_empty())
      .and_then(|p| {
        let p = p.to_lowercase();
        PROWL_PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v)
      })
      .unwrap_or(0);

    Some(Self { apikey, providerkey, priority, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Prowl",
      service_url: Some("https://www.prowlapp.com/"),
      setup_url: Some("https://appriseit.com/services/prowl/"),
      protocols: vec!["prowl"],
      description: "Send iOS push notifications via Prowl.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Prowl {
  fn schemas(&self) -> &[&str] {
    &["prowl"]
  }
  fn service_name(&self) -> &str {
    "Prowl"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    10000
  }
  fn title_maxlen(&self) -> usize {
    1024
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let priority = self.priority.to_string();
    let mut params = vec![
      ("apikey", self.apikey.as_str()),
      ("application", ctx.asset.app_id.as_str()),
      ("event", ctx.title.as_str()),
      ("description", ctx.body.as_str()),
      ("priority", priority.as_str()),
    ];
    if let Some(providerkey) = &self.providerkey {
      params.push(("providerkey", providerkey.as_str()));
    }

    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post(format!("{}/publicapi/add", service_base("https://api.prowlapp.com")))
      .header("User-Agent", crate::notify::APP_ID)
      .form(&params)
      .send()
      .await?;

    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent Prowl notification.");
      return Ok(true);
    }

    let reason = match status {
      406 => "IP address has exceeded API limit",
      409 => "Request not aproved.",
      _ => resp.status().canonical_reason().unwrap_or(""),
    };
    tracing::warn!("Failed to send Prowl notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
    Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn prowl(url: &str) -> Prowl {
    Prowl::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let a40 = "a".repeat(40);
    let b40 = "b".repeat(40);
    let w40 = "w".repeat(40);
    let urls = vec![
      format!("prowl://{}/{}", a40, b40),
      format!("prowl://{}", a40),
      format!("prowl://{}?priority=high", a40),
      format!("prowl://{}?priority=invalid", a40),
      format!("prowl://{}?priority=", a40),
      format!("prowl://{}///", w40),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let short_key = format!("prowl://{}", "a".repeat(20));
    let bad_provider = format!("prowl://{}/{}", "a".repeat(40), "b".repeat(20));
    let bad_chars = format!("prowl://{}-", "a".repeat(39));
    let urls: Vec<&str> = vec!["prowl://", "prowl://:@/", &short_key, &bad_provider, &bad_chars];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_provider_key() {
    let a40 = "a".repeat(40);
    let b40 = "b".repeat(40);
    let obj = prowl(&format!("prowl://{}/{}", a40, b40));
    assert_eq!(obj.apikey, a40);
    assert_eq!(obj.providerkey.as_deref(), Some(b40.as_str()));
    assert_eq!(prowl(&format!("prowl://{}///", a40)).providerkey, None);
    // Only the first path entry is used as the provider key
    assert_eq!(prowl(&format!("prowl://{}/{}/short", a40, b40)).providerkey.as_deref(), Some(b40.as_str()));
  }

  #[test]
  fn test_priorities() {
    let a40 = "a".repeat(40);
    let cases = [
      ("low", -2),
      ("moderate", -1),
      ("normal", 0),
      ("high", 1),
      ("emergency", 2),
      ("E", 2),
      ("-2", -2),
      ("-1", -1),
      ("0", 0),
      ("1", 1),
      ("2", 2),
      ("10", 1),
      ("invalid", 0),
      ("", 0),
    ];
    for (value, expected) in cases {
      assert_eq!(prowl(&format!("prowl://{}?priority={}", a40, value)).priority, expected, "priority={}", value);
    }
    assert_eq!(prowl(&format!("prowl://{}", a40)).priority, 0);
  }

  #[test]
  fn test_service_details() {
    let details = Prowl::static_details();
    assert_eq!(details.service_name, "Prowl");
    assert!(details.protocols.contains(&"prowl"));
    assert_eq!(details.service_url, Some("https://www.prowlapp.com/"));
    let svc = from_url(&format!("prowl://{}", "a".repeat(40))).unwrap();
    assert_eq!(svc.body_maxlen(), 10000);
    assert_eq!(svc.title_maxlen(), 1024);
  }

  #[tokio::test]
  async fn test_send_with_provider_key() {
    let a40 = "a".repeat(40);
    let b40 = "b".repeat(40);
    let server = MockServer::builder().start().await;
    // Matches pycapture.py for prowl://a.../b...?priority=-1
    Mock::given(method("POST"))
      .and(path("/publicapi/add"))
      .and(header("content-type", "application/x-www-form-urlencoded"))
      .and(body_string(format!("apikey={}&application=Apprise&event=T&description=B&priority=-1&providerkey={}", a40, b40)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("prowl://{}/{}?priority=-1", a40, b40)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_default() {
    let a40 = "a".repeat(40);
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/publicapi/add"))
      .and(body_string(format!("apikey={}&application=Apprise&event=apprise+parity&description=hello+from+python&priority=0", a40)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("prowl://{}", a40)).unwrap();
    let ctx = NotifyContext { title: "apprise parity".into(), body: "hello from python".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [406, 500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("prowl://{}", "a".repeat(40))).unwrap();
      let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err(), "status {}", status);
    }
  }
}

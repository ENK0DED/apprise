use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::{url_credentials, url_host};
use async_trait::async_trait;

pub struct FortySixElks {
  user: String,
  password: String,
  source: Option<String>,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `parse_native_url()`:
/// `^https?://(?P<credentials>[^@]+)@api\.46elks\.com/a1/sms/?(?P<params>\?.+)$`
fn is_native_url(url: &ParsedUrl) -> bool {
  let after_scheme = url.raw.split_once("://").map(|(_, rest)| rest).unwrap_or_default();
  let Some((credentials, rest)) = after_scheme.split_once('@') else { return false };
  let rest = rest.to_lowercase();
  let Some(params) = rest.strip_prefix("api.46elks.com/a1/sms").map(|p| p.strip_prefix('/').unwrap_or(p)) else { return false };
  !credentials.is_empty() && params.starts_with('?') && params.len() > 1
}

impl FortySixElks {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let native = url.schema == "http" || url.schema == "https";
    if native && !is_native_url(url) {
      return None;
    }

    let (user, password) = url_credentials(url);
    let Some(password) = password.filter(|p| !p.is_empty()) else {
      tracing::warn!("No 46elks password was specified.");
      return None;
    };
    let Some(user) = user.filter(|u| !u.is_empty()) else {
      tracing::warn!("No 46elks user was specified.");
      return None;
    };

    // A native URL is rewritten upstream as 46elks://{credentials}@/{params}
    let host = if native { String::new() } else { url_host(url) };
    let source = url.get("from").filter(|f| !f.is_empty()).map(str::to_string).unwrap_or(host);
    let source = Some(source.trim().to_string()).filter(|s| !s.is_empty());

    let mut raw_targets = if native { Vec::new() } else { url.path_parts.clone() };
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }
    // No targets: notify the source when it is a phone number
    if raw_targets.is_empty()
      && let Some(source) = &source
      && is_phone_no(source, 10).is_some()
    {
      raw_targets.push(source.clone());
    }

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        // Carry forward '+' if defined, otherwise do not
        Some(p) => targets.push(if target.trim_start().starts_with('+') { format!("+{}", p.full) } else { p.full }),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { user, password, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "46elks",
      service_url: Some("https://46elks.com"),
      setup_url: Some("https://appriseit.com/services/46elks/"),
      protocols: vec!["46elks", "elks"],
      description: "Send SMS via 46elks.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for FortySixElks {
  fn schemas(&self) -> &[&str] {
    &["46elks", "elks"]
  }
  fn service_name(&self) -> &str {
    "46elks"
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
    160
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no 46elks recipients to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/a1/sms", service_base("https://api.46elks.com"));
    let mut has_error = false;

    for target in &self.targets {
      let mut payload = vec![("to", target.as_str())];
      if let Some(source) = &self.source {
        payload.push(("from", source.as_str()));
      }
      payload.push(("message", ctx.body.as_str()));

      let resp = match client.post(&notify_url).header("User-Agent", APP_ID).basic_auth(&self.user, Some(&self.password)).form(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending 46elks notification to {}.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 {
        tracing::warn!("Failed to send 46elks notification to {}: error={}.", target, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent 46elks notification to {}.", target);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> FortySixElks {
    FortySixElks::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "46elks://",
      "46elks://user@/",
      "46elks://:pass@/",
      // Native URLs need credentials, the /a1/sms path and a query string
      "https://api.46elks.com/a1/sms?to=15551234511&user=a&pass=b",
      "https://user1:pass@api.46elks.com/a1/sms",
      "https://user1:pass@api.46elks.com/a1/other?to=15551234511",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // Loads but has no one to notify
      "46elks://user:pass@/",
      "46elks://user:pass@+15551234556",
      "46elks://user:pass@+15551234567/+46701234534?from=Acme",
      "elks://user:pass@+15551234123/",
      "46elks://user:pass@+15551234512",
      // Loads but has no valid targets
      "46elks://user:pass@Acme/234512",
      "https://user1:pass@api.46elks.com/a1/sms?to=15551234511&from=Acme",
      "https://user1:pass@API.46elks.com/A1/SMS/?to=15551234511",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("46elks://user1:pass123@+15551234591");
    assert_eq!(obj.user, "user1");
    assert_eq!(obj.password, "pass123");
    assert_eq!(obj.source.as_deref(), Some("+15551234591"));
    // With no targets the source number is notified
    assert_eq!(obj.targets, vec!["+15551234591"]);

    // ?from= replaces the host, which is then dropped
    let obj = parse("46elks://user:pass@+15551234567/+46701234534?from=Acme");
    assert_eq!(obj.source.as_deref(), Some("Acme"));
    assert_eq!(obj.targets, vec!["+46701234534"]);

    let obj = parse("46elks://user:pass@Acme/234512");
    assert_eq!(obj.source.as_deref(), Some("Acme"));
    assert!(obj.targets.is_empty());

    let obj = parse("46elks://user:pass@/");
    assert!(obj.source.is_none());
    assert!(obj.targets.is_empty());

    let obj = parse("https://user1:pass@api.46elks.com/a1/sms?to=15551234511&from=Acme");
    assert_eq!(obj.user, "user1");
    assert_eq!(obj.source.as_deref(), Some("Acme"));
    assert_eq!(obj.targets, vec!["15551234511"]);

    let obj = parse("https://user1:pass@api.46elks.com/a1/sms/?to=15551234511");
    assert!(obj.source.is_none());
    assert_eq!(obj.targets, vec!["15551234511"]);
  }

  #[test]
  fn test_native_url_routes_here() {
    let svc = from_url("https://user1:pass@api.46elks.com/a1/sms?to=15551234511&from=Acme").unwrap();
    assert_eq!(svc.service_name(), "46elks");
  }

  #[test]
  fn test_service_details() {
    let details = FortySixElks::static_details();
    assert_eq!(details.service_name, "46elks");
    assert_eq!(details.protocols, vec!["46elks", "elks"]);
    let obj = parse("46elks://user:pass@+15551234512");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_to_source() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/a1/sms"))
      // user1:pass123
      .and(header("Authorization", "Basic dXNlcjE6cGFzczEyMw=="))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("to=%2B15551234591&from=%2B15551234591&message=title%0D%0Abody"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("46elks://user1:pass123@+15551234591").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_source() {
    let server = MockServer::start().await;
    for to in ["15551234567", "%2B46701234534"] {
      Mock::given(method("POST"))
        .and(path("/a1/sms"))
        .and(body_string(format!("to={}&message=body", to)))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("46elks://user:pass@/15551234567/+46701234534").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("46elks://user:pass@Acme/234512").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_only_200_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).expect(2).mount(&server).await;
    let svc = from_url("46elks://user:pass@/15551234567/15551234568").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

pub struct FreeMobile {
  user: String,
  password: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl FreeMobile {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // freemobile://{user}@{password}; a real password (or ?pass=) wins
    let user = url.user.clone();
    let password = url.password.clone().unwrap_or_else(|| url.host.clone().unwrap_or_default());
    let Some(user) = user.filter(|u| !u.is_empty()).filter(|_| !password.is_empty()) else {
      tracing::warn!("A FreeMobile user and password combination was not provided.");
      return None;
    };
    Some(Self { user, password, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Free-Mobile",
      service_url: Some("https://mobile.free.fr/"),
      setup_url: Some("https://appriseit.com/services/freemobile/"),
      protocols: vec!["freemobile"],
      description: "Send SMS via Free Mobile (France).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for FreeMobile {
  fn schemas(&self) -> &[&str] {
    &["freemobile"]
  }
  fn service_name(&self) -> &str {
    "Free-Mobile"
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
    let payload = json!({ "user": self.user, "pass": self.password, "msg": ctx.body });
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = match client
      .post(format!("{}/sendmsg", service_base("https://smsapi.free-mobile.fr")))
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .body(payload.to_string())
      .send()
      .await
    {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Free Mobile notification.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };

    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send Free Mobile notification: error={}.", status);
      tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
      return Ok(false);
    }
    tracing::info!("Sent Free Mobile notification.");
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> FreeMobile {
    FreeMobile::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["freemobile://", "freemobile://:@/", "freemobile://user@", "freemobile://password"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in ["freemobile://user@password", "freemobile://?user=user&pass=password", "freemobile://password?user=user"] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("freemobile://user@secret");
    assert_eq!((obj.user.as_str(), obj.password.as_str()), ("user", "secret"));
    // A password in the URL wins over the host
    let obj = parse("freemobile://user:pass@secret");
    assert_eq!((obj.user.as_str(), obj.password.as_str()), ("user", "pass"));
    let obj = parse("freemobile://secret?user=u");
    assert_eq!((obj.user.as_str(), obj.password.as_str()), ("u", "secret"));
    let obj = parse("freemobile://?user=u&pass=p");
    assert_eq!((obj.user.as_str(), obj.password.as_str()), ("u", "p"));
  }

  #[test]
  fn test_service_details() {
    let details = FreeMobile::static_details();
    assert_eq!(details.service_name, "Free-Mobile");
    assert_eq!(details.protocols, vec!["freemobile"]);
    let obj = parse("freemobile://user@password");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/sendmsg"))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({ "user": "user", "pass": "password", "msg": "title\r\nbody" })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("freemobile://user@password").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 201] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/sendmsg")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("freemobile://user@password").unwrap();
      let ctx = NotifyContext { body: "body".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "{}", status);
    }
  }
}

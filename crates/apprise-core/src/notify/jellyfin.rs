//! Jellyfin is an Emby fork that kept the session messaging endpoints, so
//! upstream implements it as a subclass of the Emby plugin. This does the same.

use crate::error::NotifyError;
use crate::notify::emby::{Emby, Flavor};
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

pub struct Jellyfin(Emby);

impl Jellyfin {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Emby::from_url_as(url, Flavor::Jellyfin).map(Self)
  }

  pub fn static_details() -> ServiceDetails {
    Flavor::Jellyfin.details()
  }
}

#[async_trait]
impl Notify for Jellyfin {
  fn schemas(&self) -> &[&str] {
    self.0.schemas()
  }
  fn service_name(&self) -> &str {
    self.0.service_name()
  }
  fn details(&self) -> ServiceDetails {
    self.0.details()
  }
  fn tags(&self) -> Vec<String> {
    self.0.tags()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    self.0.send(ctx).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in ["jellyfin://", "jellyfins://", "jellyfin://localhost", "jellyfin://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in ["jellyfin://l2g:l2gpass@localhost", "jellyfins://l2g:l2gpass@localhost", "jellyfin://l2g@localhost:8096?modal=yes"] {
      let svc = from_url(url).unwrap_or_else(|| panic!("Should parse: {}", url));
      assert_eq!(svc.service_name(), "Jellyfin");
    }
    let details = Jellyfin::static_details();
    assert_eq!((details.service_url, details.setup_url), (Some("https://jellyfin.org/"), Some("https://appriseit.com/services/jellyfin/")));
  }

  #[tokio::test]
  async fn test_send_uses_emby_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/Users/AuthenticateByName"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"AccessToken": "tok", "Id": "uid"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/Sessions"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"Id": "s1"}])))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/Sessions/s1/Message")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/Sessions/Logout")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;

    let j = Jellyfin::from_url(&ParsedUrl::parse(&format!("jellyfin://l2g:pass@localhost:{}", server.address().port())).unwrap()).unwrap();
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    assert!(j.send(&ctx).await.unwrap());
  }
}

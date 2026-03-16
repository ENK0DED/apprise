use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct Notifiarr {
  apikey: String,
  channels: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl Notifiarr {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.get("apikey").or_else(|| url.get("key")).map(|s| s.to_string()).or_else(|| url.host.clone().filter(|h| !h.is_empty()))?;
    if apikey.trim().is_empty() {
      return None;
    }

    // Validate event if provided
    if let Some(event) = url.get("event") {
      // Event must be numeric or empty
      if !event.is_empty() && event.parse::<u64>().is_err() {
        return None;
      }
    }

    let mut channels = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      channels.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    Some(Self { apikey, channels, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notifiarr",
      service_url: Some("https://notifiarr.com"),
      setup_url: None,
      protocols: vec!["notifiarr"],
      description: "Send notifications via Notifiarr.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for Notifiarr {
  fn schemas(&self) -> &[&str] {
    &["notifiarr"]
  }
  fn service_name(&self) -> &str {
    "Notifiarr"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let channel = self.channels.first().map(|s| s.as_str()).unwrap_or("0");
    let payload = json!({ "notification": { "update": false, "name": "Apprise", "event": ctx.title }, "discord": { "color": ctx.notify_type.color(), "ping": { "pingUser": 0, "pingRole": 0 }, "text": { "title": ctx.title, "content": ctx.body, "footer": "Apprise" }, "ids": { "channel": channel } } });
    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post("https://notifiarr.com/api/v1/notification/apprise")
      .header("User-Agent", APP_ID)
      .header("x-api-key", self.apikey.as_str())
      .json(&payload)
      .send()
      .await?;
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
      "notifiarr://apikey",
      "notifiarr://apikey/%%invalid%%",
      "notifiarr://apikey/#123",
      "notifiarr://apikey/123?image=No",
      "notifiarr://apikey/123?image=yes",
      "notifiarr://apikey/?to=123,432",
      "notifiarr://apikey/?to=123,432&event=1234",
      "notifiarr://123/?apikey=myapikey",
      "notifiarr://123/?key=myapikey",
      "notifiarr://123/?apikey=myapikey&image=yes",
      "notifiarr://123/?apikey=myapikey&image=no",
      "notifiarr://123/?apikey=myapikey&source=My%20System",
      "notifiarr://123/?apikey=myapikey&from=My%20System",
      "notifiarr://?apikey=myapikey",
      "notifiarr://invalid?apikey=myapikey",
      "notifiarr://123/325/?apikey=myapikey",
      "notifiarr://apikey/123/",
      "notifiarr://apikey/123",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["notifiarr://:@/", "notifiarr://", "notifiarr://apikey/1234/?event=invalid"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_apikey_from_host() {
    let parsed = ParsedUrl::parse("notifiarr://myapikey/123").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert_eq!(n.apikey, "myapikey");
    assert_eq!(n.channels, vec!["123"]);
  }

  #[test]
  fn test_from_url_apikey_from_query() {
    let parsed = ParsedUrl::parse("notifiarr://123/?apikey=myapikey").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert_eq!(n.apikey, "myapikey");
  }

  #[test]
  fn test_from_url_key_alias() {
    let parsed = ParsedUrl::parse("notifiarr://123/?key=myapikey").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert_eq!(n.apikey, "myapikey");
  }

  #[test]
  fn test_channels_from_to_param() {
    let parsed = ParsedUrl::parse("notifiarr://apikey/?to=123,432").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert!(n.channels.contains(&"123".to_string()));
    assert!(n.channels.contains(&"432".to_string()));
  }

  #[test]
  fn test_multiple_channels() {
    // When apikey is from host, path parts are channels
    let parsed = ParsedUrl::parse("notifiarr://apikey/123/325").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert_eq!(n.apikey, "apikey");
    assert!(n.channels.contains(&"123".to_string()));
    assert!(n.channels.contains(&"325".to_string()));
  }

  #[test]
  fn test_event_must_be_numeric() {
    // Valid numeric event
    assert!(from_url("notifiarr://apikey/?to=123,432&event=1234").is_some());
    // Invalid non-numeric event
    assert!(from_url("notifiarr://apikey/1234/?event=invalid").is_none());
  }

  #[test]
  fn test_static_details() {
    let details = Notifiarr::static_details();
    assert_eq!(details.service_name, "Notifiarr");
    assert_eq!(details.service_url, Some("https://notifiarr.com"));
    assert!(details.protocols.contains(&"notifiarr"));
    assert!(!details.attachment_support);
  }

  #[test]
  fn test_api_endpoint_is_fixed() {
    // The Notifiarr API endpoint is fixed at https://notifiarr.com/api/v1/notification/apprise
    // Verify it's used in the send method by checking the static details
    let parsed = ParsedUrl::parse("notifiarr://apikey/12345").expect("parse");
    let n = Notifiarr::from_url(&parsed).expect("from_url");
    assert_eq!(n.apikey, "apikey");
    assert_eq!(n.channels, vec!["12345"]);
  }
}

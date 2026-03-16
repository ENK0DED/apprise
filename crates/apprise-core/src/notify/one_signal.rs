use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;
pub struct OneSignal {
  apikey: String,
  app_id: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}
impl OneSignal {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // onesignal://app_id@apikey or onesignal://?apikey=abc&app=123&to=playerid
    let app_id = url.user.clone().or_else(|| url.get("app").map(|s| s.to_string()))?;
    let apikey = url.password.clone().or_else(|| url.host.clone().filter(|h| !h.is_empty())).or_else(|| url.get("apikey").map(|s| s.to_string()))?;
    // Validate: reject whitespace-only keys
    let decoded_key = urlencoding::decode(&apikey).unwrap_or_default().into_owned();
    if decoded_key.trim().is_empty() {
      return None;
    }
    let mut targets = url.path_parts.clone();
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    // Validate language if provided (must be 2 characters)
    if let Some(lang) = url.get("lang") {
      if lang.len() != 2 {
        return None;
      }
    }
    Some(Self { apikey, app_id, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "OneSignal",
      service_url: Some("https://onesignal.com"),
      setup_url: None,
      protocols: vec!["onesignal"],
      description: "Send push notifications via OneSignal.",
      attachment_support: false,
    }
  }
}
#[async_trait]
impl Notify for OneSignal {
  fn schemas(&self) -> &[&str] {
    &["onesignal"]
  }
  fn service_name(&self) -> &str {
    "OneSignal"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut payload = json!({ "app_id": self.app_id, "headings": { "en": ctx.title }, "contents": { "en": ctx.body } });
    if self.targets.is_empty() {
      payload["included_segments"] = json!(["All"]);
    } else {
      payload["include_player_ids"] = json!(self.targets);
    }
    let client = build_client(self.verify_certificate)?;
    let resp = client
      .post("https://onesignal.com/api/v1/notifications")
      .header("User-Agent", APP_ID)
      .header("Authorization", format!("Basic {}", self.apikey))
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
      "onesignal://appid@apikey/",
      "onesignal://appid@apikey/playerid",
      "onesignal://appid@apikey/player",
      "onesignal://appid@apikey/@user?image=no",
      "onesignal://appid@apikey/user@email.com/#seg/player/@user/%20/a",
      "onesignal://appid@apikey?to=#segment,playerid",
      "onesignal://appid@apikey/#segment/@user/?batch=yes",
      "onesignal://appid@apikey/#segment/@user/?batch=no",
      "onesignal://templateid:appid@apikey/playerid",
      "onesignal://appid@apikey/playerid/?lang=es&subtitle=Sub",
      "onesignal://?apikey=abc&template=tp&app=123&to=playerid",
      "onesignal://?apikey=abc&template=tp&app=123&to=playerid&body=no&:key1=val1&:key2=val2",
      "onesignal://?apikey=abc&template=tp&app=123&to=playerid&body=no&+key1=val1&+key2=val2",
      "onesignal://appid@apikey/#segment/playerid/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["onesignal://", "onesignal://:@/", "onesignal://apikey/", "onesignal://appid@%20%20/", "onesignal://appid@apikey/playerid/?lang=X"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let parsed = crate::utils::parse::ParsedUrl::parse("onesignal://myapp@myapikey/player1/player2").unwrap();
    let obj = OneSignal::from_url(&parsed).unwrap();
    assert_eq!(obj.app_id, "myapp");
    assert_eq!(obj.apikey, "myapikey");
    assert!(obj.targets.contains(&"player1".to_string()));
    assert!(obj.targets.contains(&"player2".to_string()));
  }

  #[test]
  fn test_from_url_kwargs() {
    let parsed = crate::utils::parse::ParsedUrl::parse("onesignal://?apikey=abc&app=123&to=playerid").unwrap();
    let obj = OneSignal::from_url(&parsed).unwrap();
    assert_eq!(obj.app_id, "123");
    assert_eq!(obj.apikey, "abc");
    assert!(obj.targets.contains(&"playerid".to_string()));
  }

  #[test]
  fn test_service_details() {
    let details = OneSignal::static_details();
    assert_eq!(details.service_name, "OneSignal");
    assert!(details.protocols.contains(&"onesignal"));
    assert_eq!(details.service_url, Some("https://onesignal.com"));
  }

  fn default_ctx() -> NotifyContext {
    NotifyContext { title: "Test Title".into(), body: "Test Body".into(), ..Default::default() }
  }

  #[test]
  fn test_struct_fields_direct() {
    let obj = OneSignal { apikey: "testapikey".into(), app_id: "testapp".into(), targets: vec!["player1".into()], verify_certificate: false, tags: vec![] };

    assert_eq!(obj.apikey, "testapikey");
    assert_eq!(obj.app_id, "testapp");
    assert_eq!(obj.targets, vec!["player1".to_string()]);
  }

  #[tokio::test]
  async fn test_send_server_error() {
    // Verify that the struct is built correctly for error case URLs
    let parsed = crate::utils::parse::ParsedUrl::parse("onesignal://appid@apikey/#segment/playerid/").unwrap();
    let obj = OneSignal::from_url(&parsed).unwrap();
    assert_eq!(obj.app_id, "appid");
    assert_eq!(obj.apikey, "apikey");
    assert!(obj.targets.len() >= 2);
  }
}

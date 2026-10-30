use std::collections::HashMap;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::base64::decode_b64_dict;
use crate::utils::email::{is_email, parse_list};
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

/// Requests per batch when `?batch=yes`.
const DEFAULT_BATCH_SIZE: usize = 2000;

/// Target categories, in the order they are sent (`ONESIGNAL_CATEGORIES`).
const PLAYER: &str = "include_player_ids";
const EMAIL: &str = "include_email_tokens";
const USER: &str = "include_external_user_ids";
const SEGMENT: &str = "included_segments";
const CATEGORIES: [&str; 4] = [PLAYER, EMAIL, USER, SEGMENT];

pub struct OneSignal {
  apikey: String,
  app: String,
  template_id: Option<String>,
  subtitle: Option<String>,
  language: String,
  include_image: bool,
  use_contents: bool,
  batch_size: usize,
  /// Targets per category; users and segments keep their `@` / `#`.
  targets: HashMap<&'static str, Vec<String>>,
  custom_data: Map<String, Value>,
  postback_data: Map<String, Value>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl OneSignal {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream unquotes these query values once more
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(unquote);
    let qsd_bool = |k: &str, default: bool| url.get(k).map(parse_bool).unwrap_or(default);
    // validate_regex(): the first non-whitespace run
    let token = |v: &str| v.split_whitespace().next().map(str::to_string);

    // {template}:{app}@{apikey} or {app}@{apikey}
    let (app, template) = match url.password.as_deref().filter(|p| !p.is_empty()) {
      Some(password) => (password.to_string(), url.user.clone()),
      None => (url.user.clone().unwrap_or_default(), None),
    };
    let app = qsd("app").unwrap_or(app);
    let template_id = qsd("template").or(template).filter(|t| !t.is_empty());
    let apikey = qsd("apikey").unwrap_or_else(|| url.host.clone().unwrap_or_default());

    let Some(apikey) = token(&apikey) else {
      tracing::warn!("An invalid OneSignal API key was specified.");
      return None;
    };
    let Some(app) = token(&app) else {
      tracing::warn!("An invalid OneSignal Application ID was specified.");
      return None;
    };

    // language= wins over its lang= alias; only the first 2 letters count
    let language = match qsd("language").or_else(|| qsd("lang")) {
      Some(language) => {
        let code: String = language.trim().to_lowercase().chars().take(2).collect();
        if code.chars().count() != 2 {
          tracing::warn!("An invalid OneSignal Language ({}) was specified.", language);
          return None;
        }
        code
      }
      None => "en".to_string(),
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      entries.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    let mut targets: HashMap<&'static str, Vec<String>> = CATEGORIES.iter().map(|c| (*c, Vec::new())).collect();
    for target in parse_list(&entries) {
      let target = target.trim();
      if target.chars().count() < 2 {
        tracing::debug!("Ignoring OneSignal Entry: {}", target);
        continue;
      }
      let (category, value) = if target.starts_with('@') {
        (USER, target.to_string())
      } else if target.starts_with('#') {
        (SEGMENT, target.to_string())
      } else if let Some(email) = is_email(target).map(|e| e.full_email) {
        (EMAIL, email)
      } else {
        (PLAYER, target.to_string())
      };
      targets.get_mut(category).unwrap().push(value);
    }

    let decode = qsd_bool("decode", false);
    let custom: HashMap<String, Value> = url.qsd_colon.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let custom = if decode { decode_b64_dict(&custom) } else { custom };
    let postback_data: Map<String, Value> = url.qsd_add.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();

    Some(Self {
      apikey,
      app,
      template_id,
      subtitle: qsd("subtitle"),
      language,
      include_image: qsd_bool("image", true),
      use_contents: qsd_bool("contents", true),
      batch_size: if qsd_bool("batch", false) { DEFAULT_BATCH_SIZE } else { 1 },
      targets,
      custom_data: custom.into_iter().collect(),
      postback_data,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "OneSignal",
      service_url: Some("https://onesignal.com"),
      setup_url: Some("https://appriseit.com/services/onesignal/"),
      protocols: vec!["onesignal"],
      description: "Send push notifications via OneSignal.",
      attachment_support: false,
    }
  }

  /// The payload every batch shares.
  fn payload(&self, ctx: &NotifyContext) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("app_id".into(), json!(self.app));
    payload.insert("contents".into(), json!({ self.language.clone(): ctx.body }));
    payload.insert("content_available".into(), json!(true));
    if let Some(template) = &self.template_id {
      payload.insert("template_id".into(), json!(template));
      if !self.use_contents {
        payload.remove("contents");
      }
    }
    if !self.custom_data.is_empty() {
      payload.insert("custom_data".into(), Value::Object(self.custom_data.clone()));
    }
    if !self.postback_data.is_empty() {
      payload.insert("data".into(), Value::Object(self.postback_data.clone()));
    }
    if !ctx.title.is_empty() {
      payload.insert("headings".into(), json!({ self.language.clone(): ctx.title }));
    }
    if let Some(subtitle) = &self.subtitle {
      payload.insert("subtitle".into(), json!({ self.language.clone(): subtitle }));
    }
    if self.include_image {
      if let Some(large) = ctx.image_url(crate::asset::NotifyImageSize::Xy72) {
        payload.insert("large_icon".into(), json!(large));
      }
      if let Some(small) = ctx.image_url(crate::asset::NotifyImageSize::Xy32) {
        payload.insert("small_icon".into(), json!(small));
      }
    }
    payload
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
    let url = format!("{}/notifications", service_base("https://api.onesignal.com"));
    let client = build_client(self.verify_certificate)?;
    let payload = self.payload(ctx);
    let mut has_error = false;
    let mut sent_count = 0;

    for category in CATEGORIES {
      for (index, batch) in self.targets[category].chunks(self.batch_size).enumerate() {
        // A retry skips batches that already went out, still counting them
        let key = format!("{}:{}", category, index);
        if crate::delivery::is_delivered(&key, false) {
          sent_count += batch.len();
          continue;
        }
        // Users and segments are sent without their prefix
        let batch: Vec<&str> = batch.iter().map(|t| if matches!(category, USER | SEGMENT) { &t[1..] } else { t.as_str() }).collect();
        let mut data = payload.clone();
        data.insert(category.into(), json!(batch));
        sent_count += batch.len();

        tracing::debug!("OneSignal POST URL: {} (cert_verify={})", url, self.verify_certificate);
        crate::notify::throttle().await;
        let result = client
          .post(&url)
          .header("User-Agent", APP_ID)
          .header("Content-Type", "application/json; charset=utf-8")
          .header("Authorization", format!("Basic {}", self.apikey))
          .body(serde_json::to_vec(&Value::Object(data))?)
          .send()
          .await;
        match result {
          Ok(resp) if matches!(resp.status().as_u16(), 200 | 204) => {
            tracing::info!("Sent OneSignal notification.");
            crate::delivery::mark_delivered(&key, false);
          }
          Ok(resp) => {
            let reason = resp.status().canonical_reason().unwrap_or("");
            tracing::warn!("Failed to send OneSignal notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, resp.status().as_u16());
            has_error = true;
          }
          Err(e) => {
            tracing::warn!("A Connection error occurred sending OneSignal notification.");
            tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
            has_error = true;
          }
        }
      }
    }

    if sent_count == 0 {
      tracing::warn!("There are no OneSignal targets to notify");
      return Ok(false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn onesignal(url: &str) -> OneSignal {
    OneSignal::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

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
      "onesignal://appid@apikey/playerid/?language=es&subtitle=Sub",
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
    let urls = vec![
      "onesignal://",
      "onesignal://:@/",
      "onesignal://apikey/",
      "onesignal://appid@%20%20/",
      "onesignal://appid@apikey/playerid/?lang=X",
      "onesignal://appid@apikey/playerid/?language=X",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parsing() {
    let obj = onesignal("onesignal://appid@apikey/user@email.com/%23seg/player/@user/%20/a/p2");
    assert_eq!((obj.app.as_str(), obj.apikey.as_str(), obj.template_id.as_deref()), ("appid", "apikey", None));
    assert_eq!(obj.targets[PLAYER], vec!["p2", "player"]);
    assert_eq!(obj.targets[EMAIL], vec!["user@email.com"]);
    assert_eq!(obj.targets[USER], vec!["@user"]);
    assert_eq!(obj.targets[SEGMENT], vec!["#seg"]);
    assert_eq!(obj.language, "en");
    assert_eq!(obj.batch_size, 1);

    let obj = onesignal("onesignal://tpl:appid@apikey?language=es-ES&batch=yes");
    assert_eq!((obj.app.as_str(), obj.template_id.as_deref()), ("appid", Some("tpl")));
    assert_eq!((obj.language.as_str(), obj.batch_size), ("es", DEFAULT_BATCH_SIZE));
    // language= wins over lang=
    assert_eq!(onesignal("onesignal://a@k/p?language=fr&lang=de").language, "fr");
    assert_eq!(onesignal("onesignal://a@k/p?lang=de").language, "de");

    let obj = onesignal("onesignal://?apikey=abc&template=tp&app=123&to=playerid");
    assert_eq!((obj.app.as_str(), obj.apikey.as_str(), obj.template_id.as_deref()), ("123", "abc", Some("tp")));
  }

  #[tokio::test]
  async fn test_send_matches_upstream() {
    // pycapture.py for this URL: one request per target, in category order
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/notifications"))
      .and(header("authorization", "Basic apikey"))
      .and(header("content-type", "application/json; charset=utf-8"))
      .respond_with(ResponseTemplate::new(200))
      .expect(5)
      .mount(&server)
      .await;

    let svc = from_url(
      "onesignal://tpl:appid@apikey/user@email.com/%23seg/player/@user/%20/a/p2?lang=es-ES&subtitle=Sub&contents=no&decode=yes&:key1=b64:eyJhIjoxfQ==&:key2=v2&+pb=1&batch=no",
    )
    .unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Success, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let base = json!({
      "app_id": "appid", "content_available": true, "template_id": "tpl",
      "custom_data": {"key1": {"a": 1}, "key2": "v2"}, "data": {"pb": "1"},
      "headings": {"es": "T"}, "subtitle": {"es": "Sub"},
      "large_icon": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-success-72x72.png",
      "small_icon": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-success-32x32.png",
    });
    let expected = [(PLAYER, "p2"), (PLAYER, "player"), (EMAIL, "user@email.com"), (USER, "user"), (SEGMENT, "seg")];
    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/notifications").await;
    for (request, (category, target)) in requests.iter().zip(expected) {
      let mut want = base.clone();
      want[category] = json!([target]);
      assert_eq!(serde_json::from_slice::<Value>(&request.body).unwrap(), want);
    }
  }

  #[tokio::test]
  async fn test_send_batches_and_contents() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/notifications")).respond_with(ResponseTemplate::new(204)).expect(2).mount(&server).await;
    let svc = from_url("onesignal://appid@apikey/pid1/pid2/@u1/@u2?batch=yes&image=no").unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/notifications").await;
    assert_eq!(
      serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
      json!({"app_id": "appid", "contents": {"en": "B"}, "content_available": true, "include_player_ids": ["pid1", "pid2"]})
    );
    assert_eq!(serde_json::from_slice::<Value>(&requests[1].body).unwrap()[USER], json!(["u1", "u2"]));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    // No targets
    assert!(!from_url("onesignal://appid@apikey/").unwrap().send(&ctx).await.unwrap());
    for status in [500, 999, 201] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/notifications")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url("onesignal://appid@apikey/p1/p2").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
  }

  #[test]
  fn test_service_details() {
    let details = OneSignal::static_details();
    assert_eq!(details.service_name, "OneSignal");
    assert!(details.protocols.contains(&"onesignal"));
    assert_eq!(details.service_url, Some("https://onesignal.com"));
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second batch fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/notifications"))
      .and(body_partial_json(json!({"include_external_user_ids": ["u1"]})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/notifications")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("onesignal://appid@apikey/pid1/@u1?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "B".into(), ..Default::default() })).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", "/notifications")
      .await
      .iter()
      .map(|r| {
        let b: Value = serde_json::from_slice(&r.body).unwrap();
        b.get(PLAYER).or_else(|| b.get(USER)).cloned().unwrap()
      })
      .collect();
    assert_eq!(sent, vec![json!(["pid1"]), json!(["u1"]), json!(["u1"])]);
  }
}

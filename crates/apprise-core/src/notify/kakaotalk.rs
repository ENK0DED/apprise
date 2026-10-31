use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::conversion::pystr;
use crate::error::NotifyError;
use crate::notify::incident::status_text;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::plugin_args::parse_list;

/// Upstream `KAKAOTALK_HTTP_ERROR_MAP`.
const KAKAOTALK_HTTP_ERROR_MAP: &[(u16, &str)] = &[
  (400, "Bad Request - Check the Client ID, tokens and recipients."),
  (401, "Unauthorized - The access token is invalid or expired."),
  (403, "Forbidden - Message permission or consent is missing."),
  (429, "Too many requests were made."),
];

/// The target that stands for the token owner.
const KAKAOTALK_ME: &str = "me";

/// Access token lifetime when one is not returned (12 hours).
const TOKEN_EXPIRY: i64 = 43200;

/// Renew the access token this many seconds before it expires.
const TOKEN_EXPIRY_EDGE: i64 = 60;

/// The text template needs a link; this is what Kakao's own examples use.
const DEFAULT_LINK: &str = "https://developers.kakao.com";

/// KakaoTalk messages through the Kakao Developers Message REST API, sent to
/// the account that signed in ("me") or to its friends by UUID.
///
/// `kakaotalk://[{client_secret}@]{client_id}/{refresh_token}[/{uuid}...]`.
/// The refresh token is traded for an access token at send time. The access
/// token, and any newer refresh token Kakao hands back, are kept in the
/// persistent store as upstream does; without a store every send logs in
/// with the refresh token from the URL.
pub struct KakaoTalk {
  client_id: String,
  client_secret: Option<String>,
  refresh_token: String,
  /// Friend UUIDs and `me`; empty sends to the token owner
  targets: Vec<String>,
  link: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `validate_regex(value, "^[...]+$", "i")`: every character passes
/// `allowed` (Python's `$` also accepts one trailing newline). Returns the
/// stripped value.
fn validate(value: &str, allowed: fn(char) -> bool) -> Option<String> {
  let core = value.strip_suffix('\n').unwrap_or(value);
  (!core.is_empty() && core.chars().all(allowed)).then(|| pystr::strip(value).to_string())
}

/// `^[a-z0-9]+$`: the Client ID and Client Secret.
fn is_key_char(c: char) -> bool {
  c.is_ascii_alphanumeric()
}

/// `^[a-z0-9_-]+$`: the refresh token and friend UUIDs.
fn is_token_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Python's `int(value)` of a JSON value, `None` where it raises.
fn py_int(value: Option<&Value>) -> Option<i64> {
  match value? {
    Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
    Value::String(s) => pystr::strip(s).parse().ok(),
    Value::Bool(b) => Some(i64::from(*b)),
    _ => None,
  }
}

/// A store expiry from upstream's `expires=` seconds: `None` (and `0`, which
/// Python reads as `False`) never expire. A negative value has already
/// expired.
fn store_expiry(seconds: Option<i64>) -> Result<Option<Duration>, ()> {
  match seconds {
    None | Some(0) => Ok(None),
    Some(s) if s < 0 => Err(()),
    Some(s) => Ok(Some(Duration::from_secs(s as u64))),
  }
}

/// A non-empty string field of a response.
fn text_field<'a>(content: &'a Value, key: &str) -> Option<&'a str> {
  content.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

impl KakaoTalk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The host is our Client ID
    let host = url.host.clone().unwrap_or_default();
    let Some(client_id) = validate(&host, is_key_char) else {
      tracing::warn!("An invalid KakaoTalk Client ID was specified.");
      return None;
    };

    // The first path entry is our refresh token; the rest are targets
    let mut entries = url.path_parts.clone();
    let refresh_token = if entries.is_empty() { String::new() } else { entries.remove(0) };
    let Some(refresh_token) = validate(&refresh_token, is_token_char) else {
      tracing::warn!("An invalid KakaoTalk Refresh Token was specified.");
      return None;
    };

    // The optional Client Secret sits in front of the @, or in ?secret=
    let client_secret = url.get("secret").filter(|s| !s.is_empty()).map(unquote).or_else(|| url.user.clone()).filter(|s| !s.is_empty());
    let client_secret = match client_secret {
      Some(secret) => match validate(&secret, is_key_char) {
        Some(secret) => Some(secret),
        None => {
          tracing::warn!("An invalid KakaoTalk Client Secret was specified.");
          return None;
        }
      },
      None => None,
    };

    // Where a tap on the message takes the reader
    let link = url.get("link").filter(|l| !l.is_empty()).map(|l| pystr::strip(&unquote(l)).to_string()).filter(|l| !l.is_empty());

    // Support ?to= for targets; upstream's URLBase.parse_list() also
    // unquotes each entry
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_list([to], true, true).iter().map(|t| unquote(t)));
    }

    let mut targets = Vec::new();
    for target in parse_list(&entries, true, true) {
      // "me" is the token owner; anything else is a friend UUID
      if target.to_lowercase() == KAKAOTALK_ME {
        targets.push(KAKAOTALK_ME.to_string());
      } else if let Some(uuid) = validate(&target, is_token_char) {
        targets.push(uuid);
      } else {
        tracing::warn!("Dropped invalid KakaoTalk UUID ({}) specified.", target);
      }
    }

    Some(Self { client_id, client_secret, refresh_token, targets, link, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "KakaoTalk",
      service_url: Some("https://www.kakaocorp.com/"),
      setup_url: Some("https://appriseit.com/services/kakaotalk/"),
      protocols: vec!["kakaotalk", "kakao"],
      description: "Send KakaoTalk messages to yourself or your friends.",
      attachment_support: false,
    }
  }

  /// Trade our refresh token for an access token (upstream `login()`).
  async fn login(&self, client: &reqwest::Client) -> Option<String> {
    // Reset our cached access token
    crate::storage::with_service_store(|s| s.remove("access_token"));

    // Kakao may have handed us a newer refresh token on a past run
    let stored = crate::storage::with_service_store(|s| s.get("refresh_token")).flatten();
    let refresh_token = stored.as_ref().and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(&self.refresh_token);

    let mut payload = vec![("grant_type", "refresh_token"), ("client_id", self.client_id.as_str()), ("refresh_token", refresh_token)];
    if let Some(secret) = &self.client_secret {
      payload.push(("client_secret", secret.as_str()));
    }

    // Token calls are not throttled
    let url = format!("{}/oauth/token", service_base("https://kauth.kakao.com"));
    let (ok, response) = self.fetch(client.post(url).form(&payload), "token request", false).await;

    let Some(access_token) = text_field(&response, "access_token").filter(|_| ok) else {
      tracing::warn!("KakaoTalk could not obtain an access token.");
      return None;
    };

    // Renew a little early so a send never uses a stale token
    let expires = py_int(response.get("expires_in")).unwrap_or(TOKEN_EXPIRY) - TOKEN_EXPIRY_EDGE;
    let ttl = Duration::from_secs(expires.max(TOKEN_EXPIRY_EDGE) as u64);
    crate::storage::with_service_store(|s| s.set("access_token", json!(access_token), Some(ttl), true));

    // Kakao returns a new refresh token once the old one has less than a
    // month left; the old one stops working, so keep the new one.
    if let Some(rotated) = text_field(&response, "refresh_token") {
      // No lifetime given keeps it until it is replaced
      match store_expiry(py_int(response.get("refresh_token_expires_in"))) {
        Ok(ttl) => crate::storage::with_service_store(|s| s.set("refresh_token", json!(rotated), ttl, true)),
        Err(()) => crate::storage::with_service_store(|s| s.remove("refresh_token")),
      };
    }

    Some(access_token.to_string())
  }

  /// POST helper shared by the token and message calls (upstream
  /// `_fetch()`): whether Kakao answered 200, and its JSON object reply
  /// (empty when it sent none).
  async fn fetch(&self, request: reqwest::RequestBuilder, name: &str, throttle: bool) -> (bool, Value) {
    if throttle {
      crate::notify::throttle().await;
    }
    let resp = match request.header("User-Agent", APP_ID).header("Accept", "application/json").send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending KakaoTalk {}.", name);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return (false, json!({}));
      }
    };

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    // A reply that is not a JSON object leaves us with the status code
    let content = serde_json::from_str::<Value>(&body).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));

    if status != 200 {
      // An expired or revoked token is fetched again next time
      if status == 401 {
        crate::storage::with_service_store(|s| s.remove("access_token"));
      }

      // Prefer Kakao's own explanation; the token server and the message
      // server name it differently
      let reason =
        text_field(&content, "msg").or_else(|| text_field(&content, "error_description")).unwrap_or_else(|| status_text(status, KAKAOTALK_HTTP_ERROR_MAP));
      tracing::warn!("Failed to send KakaoTalk {}: {}{}error={}.", name, reason, if reason.is_empty() { "" } else { ", " }, status);
      return (false, content);
    }
    (true, content)
  }
}

#[async_trait]
impl Notify for KakaoTalk {
  fn schemas(&self) -> &[&str] {
    &["kakaotalk", "kakao"]
  }
  fn service_name(&self) -> &str {
    "KakaoTalk"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The text template holds at most 200 characters
  fn body_maxlen(&self) -> usize {
    200
  }
  /// The text template has no title, so it goes into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  /// The app and the account that signed in identify the connection
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["kakaotalk".into(), self.client_id.clone(), self.client_secret.clone().unwrap_or_else(|| "None".into()), self.refresh_token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    // Use our cached access token, or fetch a new one
    let cached = crate::storage::with_service_store(|s| s.get("access_token")).flatten();
    let access_token = match cached.as_ref().and_then(Value::as_str).filter(|t| !t.is_empty()) {
      Some(token) => token.to_string(),
      None => match self.login(&client).await {
        Some(token) => token,
        None => return Ok(false),
      },
    };

    // A link is required by Kakao
    let link = self.link.as_deref().unwrap_or(DEFAULT_LINK);
    let template = json!({
      "object_type": "text",
      "text": ctx.body,
      "link": { "web_url": link, "mobile_web_url": link },
    })
    .to_string();

    let base = service_base("https://kapi.kakao.com");
    let me = [KAKAOTALK_ME.to_string()];
    let targets = if self.targets.is_empty() { &me[..] } else { &self.targets[..] };
    let mut has_error = false;
    for target in targets {
      // A retry skips a target that already accepted this message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }

      let mut payload = vec![("template_object", template.clone())];
      let url = if target == KAKAOTALK_ME {
        // Message ourselves
        format!("{}/v2/api/talk/memo/default/send", base)
      } else {
        // Message one friend
        payload.push(("receiver_uuids", json!([target]).to_string()));
        format!("{}/v1/api/talk/friends/message/default/send", base)
      };

      let request = client.post(url).bearer_auth(&access_token).form(&payload);
      let (ok, _) = self.fetch(request, &format!("message to {}", target), true).await;
      if !ok {
        has_error = true;
        continue;
      }

      tracing::info!("Sent KakaoTalk notification to {}.", target);
      crate::delivery::mark_delivered(target, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  // A realistic Client ID (REST API Key), Client Secret and Refresh Token
  const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";
  const SECRET: &str = "Sx9QeT1aYp7Lr3Kd";
  const REFRESH: &str = "tGb4hhaFbYxNRexWXy6TQuKGx-pxt4QxAAAAAgo9cxgAAAGB_1a2b3c4";

  // Friend UUIDs as Kakao hands them out
  const UUID1: &str = "-Jqzh7SBuYq4kKCRo5WvhrCJsJS9gbm7kQ";
  const UUID2: &str = "aBcDeFgHiJkLmNoPqRsTuVwXyZ_0123456";

  const TOKEN: &str = "/oauth/token";
  const MEMO: &str = "/v2/api/talk/memo/default/send";
  const FRIENDS: &str = "/v1/api/talk/friends/message/default/send";

  fn kakao(url: &str) -> KakaoTalk {
    KakaoTalk::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn token_reply(extra: Value) -> ResponseTemplate {
    let mut content = json!({"access_token": "abcd1234", "expires_in": 43199});
    content.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    ResponseTemplate::new(200).set_body_json(content)
  }

  async fn mount(server: &MockServer, route: &str, reply: ResponseTemplate) {
    Mock::given(method("POST")).and(path(route)).respond_with(reply).mount(server).await;
  }

  /// The form fields of a request.
  fn form(r: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&r.body).into_owned().collect()
  }

  fn field(r: &Request, key: &str) -> Option<String> {
    form(r).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
  }

  fn template(r: &Request) -> Value {
    serde_json::from_str(&field(r, "template_object").unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    // Upstream apprise_url_tests
    for url in [
      "kakaotalk://".to_string(),
      // No refresh token specified
      format!("kakaotalk://{CLIENT_ID}"),
      // An invalid Client ID
      format!("kakaotalk://invalid!id/{REFRESH}"),
      // An invalid Client Secret
      format!("kakaotalk://{CLIENT_ID}/{REFRESH}?secret=bad!secret"),
      format!("kakaotalk://bad!secret@{CLIENT_ID}/{REFRESH}"),
      // An invalid refresh token
      format!("kakaotalk://{CLIENT_ID}/bad%20token"),
      format!("kakaotalk://{CLIENT_ID}/bad.token"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {url}");
    }
    for url in [
      format!("kakaotalk://{CLIENT_ID}/{REFRESH}"),
      format!("kakao://{SECRET}@{CLIENT_ID}/{REFRESH}/{UUID1}/me"),
      format!("kakaotalk://{CLIENT_ID}/{REFRESH}?to={UUID1},{UUID2}&link=https://example.com/status"),
      format!("kakaotalk://{CLIENT_ID}/{REFRESH}/{UUID1}/bad.uuid"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {url}");
    }
  }

  #[test]
  fn test_url_parsing() {
    // Upstream test_plugin_kakaotalk_url_parsing
    let obj = kakao(&format!("kakao://{CLIENT_ID}/{REFRESH}/{UUID1}/ME/bad.uuid?secret={SECRET}&link=https://example.com/a"));
    assert_eq!(obj.client_id, CLIENT_ID);
    assert_eq!(obj.client_secret.as_deref(), Some(SECRET));
    assert_eq!(obj.refresh_token, REFRESH);
    assert_eq!(obj.link.as_deref(), Some("https://example.com/a"));
    // An invalid UUID is dropped; "ME" is the token owner
    assert_eq!(obj.targets, vec![UUID1, "me"]);

    // ?secret= wins over the user; ?to= entries are unquoted once more,
    // and every target is sorted
    let obj = kakao(&format!("kakao://other@{CLIENT_ID}/{REFRESH}/{UUID2}?secret={SECRET}&to=a%2541,me"));
    assert_eq!(obj.client_secret.as_deref(), Some(SECRET));
    assert_eq!(obj.targets, vec!["aA", UUID2, "me"]);

    // A blank link falls back to Kakao's example link
    assert_eq!(kakao(&format!("kakao://{CLIENT_ID}/{REFRESH}?link=%20")).link, None);
    assert_eq!(kakao(&format!("kakao://{CLIENT_ID}/{REFRESH}")).client_secret, None);
  }

  #[test]
  fn test_service_details() {
    let details = KakaoTalk::static_details();
    assert_eq!(details.service_name, "KakaoTalk");
    assert_eq!(details.protocols, vec!["kakaotalk", "kakao"]);
    assert!(!details.attachment_support);
    let svc = from_url(&format!("kakao://{CLIENT_ID}/{REFRESH}")).unwrap();
    assert_eq!(svc.body_maxlen(), 200);
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.notify_format(), crate::types::NotifyFormat::Text);
    assert_eq!(svc.request_rate_per_sec(), crate::notify::DEFAULT_REQUEST_INTERVAL);
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; the targets do not count
    crate::notify::custom_json::assert_upstream_uids(&[
      (&format!("kakaotalk://{CLIENT_ID}/{REFRESH}"), "58cdffe7"),
      (&format!("kakao://{SECRET}@{CLIENT_ID}/{REFRESH}/abc/me"), "72841ead"),
      (&format!("kakao://{SECRET}@{CLIENT_ID}/{REFRESH}"), "72841ead"),
    ]);
    // The Client Secret makes it a different connection
    assert_ne!(kakao(&format!("kakao://{CLIENT_ID}/{REFRESH}")).url_identifier(), kakao(&format!("kakao://{SECRET}@{CLIENT_ID}/{REFRESH}")).url_identifier());
  }

  #[test]
  fn test_lifetimes() {
    assert_eq!(py_int(Some(&json!(43199))), Some(43199));
    assert_eq!(py_int(Some(&json!(10.9))), Some(10));
    assert_eq!(py_int(Some(&json!(" 5184000 "))), Some(5184000));
    assert_eq!(py_int(Some(&json!("invalid"))), None);
    assert_eq!(py_int(Some(&Value::Null)), None);
    assert_eq!(py_int(None), None);
    assert_eq!(store_expiry(None), Ok(None));
    assert_eq!(store_expiry(Some(0)), Ok(None));
    assert_eq!(store_expiry(Some(30)), Ok(Some(Duration::from_secs(30))));
    assert_eq!(store_expiry(Some(-1)), Err(()));
  }

  #[tokio::test]
  async fn test_send_me() {
    // Upstream test_plugin_kakaotalk_send_me; the requests match pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(TOKEN))
      .and(header("user-agent", APP_ID))
      .and(header("accept", "application/json"))
      .and(header("content-type", "application/x-www-form-urlencoded"))
      .respond_with(token_reply(json!({})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(MEMO))
      .and(header("authorization", "Bearer abcd1234"))
      .and(header("user-agent", APP_ID))
      .and(header("accept", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": 0})))
      .expect(2)
      .mount(&server)
      .await;

    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("kakaotalk://{SECRET}@{CLIENT_ID}/{REFRESH}?link=https://example.com/status")));
    let first = NotifyContext { title: "Title & <b>".into(), body: "Body _.!*#<".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&first)).await.is_success());
    // The cached access token is reused on our next send
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("again"))).await.is_success());

    let token = &requests_to(&server, "POST", TOKEN).await[0];
    assert_eq!(
      form(token),
      [("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", REFRESH), ("client_secret", SECRET)]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .to_vec()
    );

    let sent = requests_to(&server, "POST", MEMO).await;
    assert_eq!(field(&sent[0], "receiver_uuids"), None);
    assert_eq!(
      template(&sent[0]),
      json!({
        "object_type": "text",
        // The title is merged into the body untouched
        "text": "Title & <b>\r\nBody _.!*#<",
        "link": {"web_url": "https://example.com/status", "mobile_web_url": "https://example.com/status"},
      })
    );
    assert_eq!(template(&sent[1])["text"], "again");
  }

  #[tokio::test]
  async fn test_send_friends() {
    // Upstream test_plugin_kakaotalk_send_friends: each friend gets their own
    // message, and a failure is reported
    let server = MockServer::start().await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    Mock::given(method("POST"))
      .and(path(FRIENDS))
      .and(wiremock::matchers::body_string_contains("receiver_uuids=%5B%22aBcDeF"))
      .respond_with(ResponseTemplate::new(400).set_body_json(json!({"msg": "daily message limit exceeded", "code": -532})))
      .with_priority(1)
      .mount(&server)
      .await;
    mount(&server, FRIENDS, ResponseTemplate::new(200).set_body_json(json!({"successful_receiver_uuids": [UUID1]}))).await;

    let svc = from_url(&format!("kakaotalk://{CLIENT_ID}/{REFRESH}/{UUID1}/{UUID2}")).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("hello"))).await.unwrap());

    // No Client Secret is sent without one
    assert_eq!(field(&requests_to(&server, "POST", TOKEN).await[0], "client_secret"), None);
    let sent = requests_to(&server, "POST", FRIENDS).await;
    assert_eq!(sent.len(), 2);
    for (request, uuid) in sent.iter().zip([UUID1, UUID2]) {
      assert_eq!(serde_json::from_str::<Value>(&field(request, "receiver_uuids").unwrap()).unwrap(), json!([uuid]));
      // Without ?link= Kakao's own example link is used
      assert_eq!(template(request)["link"]["web_url"], DEFAULT_LINK);
      assert_eq!(template(request)["link"]["mobile_web_url"], DEFAULT_LINK);
      assert_eq!(template(request)["text"], "hello");
    }
  }

  #[tokio::test]
  async fn test_send_friend_and_me() {
    // Sorted as upstream does: the UUID before "me"
    let server = MockServer::start().await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    mount(&server, FRIENDS, ResponseTemplate::new(200).set_body_json(json!({}))).await;
    mount(&server, MEMO, ResponseTemplate::new(200).set_body_json(json!({}))).await;
    let svc = from_url(&format!("kakao://{CLIENT_ID}/{REFRESH}/me/{UUID1}")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("hello"))).await.unwrap());
    let order: Vec<String> = server.received_requests().await.unwrap().iter().map(|r| r.url.path().to_string()).filter(|p| p != "/").collect();
    assert_eq!(order, vec![TOKEN, FRIENDS, MEMO]);
  }

  #[tokio::test]
  async fn test_rotated_refresh_token() {
    // Upstream test_plugin_kakaotalk_token_handling: a new refresh token is
    // kept and used for the next login
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(TOKEN))
      .respond_with(token_reply(json!({"expires_in": "invalid", "refresh_token": "newRefresh_1", "refresh_token_expires_in": 5184000})))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(TOKEN))
      .respond_with(token_reply(json!({"refresh_token": "newRefresh_2", "refresh_token_expires_in": null})))
      .up_to_n_times(1)
      .with_priority(2)
      .mount(&server)
      .await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    // A 401 forgets our access token so the next send logs in again
    mount(&server, MEMO, ResponseTemplate::new(401).set_body_json(json!({"msg": "this access token does not exist", "code": -401}))).await;

    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("kakaotalk://{CLIENT_ID}/{REFRESH}")));
    for _ in 0..3 {
      assert!(!crate::notify::with_test_base(server.uri(), a.notify(&ctx("hello"))).await.is_success());
    }
    let used: Vec<String> = requests_to(&server, "POST", TOKEN).await.iter().map(|r| field(r, "refresh_token").unwrap()).collect();
    assert_eq!(used, vec![REFRESH, "newRefresh_1", "newRefresh_2"]);
    assert_eq!(requests_to(&server, "POST", MEMO).await.len(), 3);
  }

  #[tokio::test]
  async fn test_send_failures() {
    let url = format!("kakaotalk://{CLIENT_ID}/{REFRESH}");

    // No access token was handed back, or the login failed: no message
    for reply in [
      ResponseTemplate::new(200).set_body_string("{}"),
      ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant", "error_description": "bad token"})),
      ResponseTemplate::new(200).set_body_json(json!({"access_token": ""})),
    ] {
      let server = MockServer::start().await;
      mount(&server, TOKEN, reply).await;
      mount(&server, MEMO, ResponseTemplate::new(200)).await;
      assert!(!crate::notify::with_test_base(server.uri(), from_url(&url).unwrap().send(&ctx("hello"))).await.unwrap());
      assert!(requests_to(&server, "POST", MEMO).await.is_empty());
    }

    // A response that is not JSON, or not a dict, is survived
    for reply in [ResponseTemplate::new(500).set_body_string("not json"), ResponseTemplate::new(500).set_body_string("[]"), ResponseTemplate::new(999)] {
      let server = MockServer::start().await;
      mount(&server, TOKEN, token_reply(json!({}))).await;
      mount(&server, MEMO, reply).await;
      assert!(!crate::notify::with_test_base(server.uri(), from_url(&url).unwrap().send(&ctx("hello"))).await.unwrap());
      assert_eq!(requests_to(&server, "POST", MEMO).await.len(), 1);
    }

    // A connection error on either call
    let server = MockServer::start().await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    let base = server.uri();
    drop(server);
    assert!(!crate::notify::with_test_base(base, from_url(&url).unwrap().send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    // Upstream's delivery-tracking fleet entry: the second friend fails once
    let server = MockServer::start().await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    Mock::given(method("POST"))
      .and(path(FRIENDS))
      .and(wiremock::matchers::body_string_contains("friend9999"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    mount(&server, FRIENDS, ResponseTemplate::new(200).set_body_json(json!({}))).await;

    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("kakaotalk://abcdef0123456789/{REFRESH}/friend0001/friend9999/?retry=1&wait=0")));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("hello"))).await.is_success());
    let sent: Vec<String> = requests_to(&server, "POST", FRIENDS).await.iter().map(|r| field(r, "receiver_uuids").unwrap()).collect();
    assert_eq!(sent, vec![r#"["friend0001"]"#, r#"["friend9999"]"#, r#"["friend9999"]"#]);
  }

  #[tokio::test]
  async fn test_messages_are_throttled() {
    // Only the message calls wait between requests; the token call does not,
    // so one message is sent without waiting at all
    let server = MockServer::start().await;
    mount(&server, TOKEN, token_reply(json!({}))).await;
    mount(&server, FRIENDS, ResponseTemplate::new(200).set_body_json(json!({}))).await;
    mount(&server, MEMO, ResponseTemplate::new(200).set_body_json(json!({}))).await;
    for (targets, waits) in [("", false), (&*format!("/{UUID1}/me"), true)] {
      let svc = from_url(&format!("kakaotalk://{CLIENT_ID}/{REFRESH}{targets}")).unwrap();
      let message = ctx("hello");
      let started = std::time::Instant::now();
      let send = crate::notify::with_throttle(1.0, Default::default(), crate::notify::with_test_base(server.uri(), svc.send(&message)));
      assert!(crate::notify::with_throttling_in_tests(send).await.unwrap());
      assert_eq!(started.elapsed() >= Duration::from_secs(1), waits, "{targets}: {:?}", started.elapsed());
    }
  }
}

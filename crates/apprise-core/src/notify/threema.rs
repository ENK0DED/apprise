use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::email::is_email;
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::is_phone_no;
use crate::utils::plugin_args::parse_list;
use async_trait::async_trait;

pub struct Threema {
  gateway_id: String,
  secret: String,
  /// `(parameter, value)`: `to` for Threema IDs, `email` or `phone`
  targets: Vec<(&'static str, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `urllib.parse.urlencode()` as requests applies it to `params=`.
fn query_encode(pairs: &[(&str, &str)]) -> String {
  pairs
    .iter()
    .map(|(k, v)| format!("{}={}", urlencoding::encode(k).replace("%20", "+"), urlencoding::encode(v).replace("%20", "+")))
    .collect::<Vec<_>>()
    .join("&")
}

impl Threema {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // threema://gateway_id@secret/targets
    let qs = |key: &str| url.get(key).filter(|v| !v.is_empty());
    // Upstream unquotes these query values once more
    let gateway_id = qs("from").or_else(|| qs("gwid")).map(unquote).or_else(|| url.user.clone()).unwrap_or_default();
    if gateway_id.is_empty() {
      tracing::warn!("Threema Gateway ID must be specified");
      return None;
    }
    if gateway_id.chars().count() != 8 {
      tracing::warn!("Threema Gateway ID must be 8 characters in length");
      return None;
    }
    let secret = qs("secret").map(unquote).or_else(|| url.host.clone()).unwrap_or_default();
    // Upstream validate_regex(): not blank and no leading whitespace
    if secret.chars().next().is_none_or(char::is_whitespace) {
      tracing::warn!("An invalid Threema API Secret was specified");
      return None;
    }
    let secret = secret.trim().to_string();

    let mut entries = url.path_parts.clone();
    if let Some(to) = qs("to") {
      // Upstream's URLBase.parse_list() keeps whitespace here and unquotes
      // each entry
      entries.extend(parse_list([to], false, true).iter().map(|t| unquote(t)));
    }
    let mut targets = Vec::new();
    // Upstream splits the targets without breaking on whitespace
    for target in parse_list(&entries, false, true) {
      if target.chars().count() == 8 {
        targets.push(("to", target));
      } else if let Some(email) = is_email(&target).map(|e| e.full_email) {
        targets.push(("email", email));
      } else if let Some(phone) = is_phone_no(&target, 10) {
        targets.push(("phone", phone.full));
      } else {
        tracing::warn!("Dropped invalid user/email/phone ({}) specified", target);
      }
    }

    Some(Self { gateway_id, secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Threema Gateway",
      service_url: Some("https://gateway.threema.ch/"),
      setup_url: Some("https://appriseit.com/services/threema/"),
      protocols: vec!["threema"],
      description: "Send messages via Threema Gateway.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Threema {
  fn schemas(&self) -> &[&str] {
    &["threema"]
  }
  fn service_name(&self) -> &str {
    "Threema Gateway"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    3500
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Threema Gateway targets to notify");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let base = format!("{}/send_simple", service_base("https://msgapi.threema.ch"));
    let mut has_error = false;

    for (key, target) in &self.targets {
      // A retry skips targets that already accepted the message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      // Everything travels in the query string; the body is empty
      let query = query_encode(&[("secret", &self.secret), ("from", &self.gateway_id), ("text", &ctx.body), (key, target)]);
      tracing::debug!("Threema Gateway GET URL: {} (cert_verify={})", base, self.verify_certificate);
      crate::notify::throttle().await;
      let result = client
        .post(format!("{}?{}", base, query))
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
        .header("Accept", "*/*")
        .send()
        .await;
      match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent Threema Gateway notification to {}", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!(
            "Failed to send Threema Gateway notification to {}: {}error={}",
            target,
            status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Threema Gateway:{} notification", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn threema(url: &str) -> Threema {
    Threema::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "threema://",
      "threema://@:",
      // Gateway IDs are 8 characters long
      "threema://user@secret",
      "threema://*THEGWID",
      "threema:///?from=*THEGWID&to=16134442222",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // Too short a number is dropped (notify fails)
      "threema://*THEGWID@secret/2222/",
      "threema://*THEGWID@secret/16134442222/",
      "threema://*THEGWID@secret/16134442222/16134443333/",
      "threema:///?secret=secret&from=*THEGWID&to=16134442222,16134443333",
      "threema:///?secret=secret&gwid=*THEGWID&to=16134442222,16134443333",
      "threema://*THEGWID@secret",
      "threema://*THEGWID@secret/16134443333",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let t = threema("threema://*THEGWID@sec%2Fret/ECHOECHO/16134442222/user@example.com/abc/?to=%2B1%20(613)%20444-3333,b%2Bx@example.org");
    assert_eq!(t.gateway_id, "*THEGWID");
    assert_eq!(t.secret, "sec/ret");
    // parse_list() sorts the targets
    assert_eq!(
      t.targets,
      vec![
        ("phone", "16134443333".to_string()),
        ("phone", "16134442222".to_string()),
        ("to", "ECHOECHO".to_string()),
        ("email", "b+x@example.org".to_string()),
        ("email", "user@example.com".to_string()),
      ]
    );

    let t = threema("threema://?gwid=*THEGWID&secret=mysecret&to=+1%20(555)%20123-9876");
    assert_eq!(t.gateway_id, "*THEGWID");
    assert_eq!(t.secret, "mysecret");
    assert_eq!(t.targets, vec![("phone", "15551239876".to_string())]);
    assert!(threema("threema://*THEGWID@secret/2222/").targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let d = Threema::static_details();
    assert_eq!(d.service_name, "Threema Gateway");
    assert!(d.protocols.contains(&"threema"));
  }

  #[tokio::test]
  async fn test_send_query_params() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for (key, value) in [("phone", "16134443333"), ("to", "ECHOECHO"), ("email", "user@example.com")] {
      Mock::given(method("POST"))
        .and(path("/send_simple"))
        .and(query_param("secret", "sec/ret"))
        .and(query_param("from", "*THEGWID"))
        .and(query_param("text", "title\r\nHé ~*"))
        .and(query_param(key, value))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("threema://*THEGWID@sec%2Fret/ECHOECHO/16134443333/user@example.com").unwrap();
    let ctx = NotifyContext { body: "title\r\nHé ~*".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let req = &crate::notify::custom_json::requests_to(&server, "POST", "/send_simple").await[0];
    assert!(req.body.is_empty());
    assert_eq!(req.url.query().unwrap(), "secret=sec%2Fret&from=%2ATHEGWID&text=title%0D%0AH%C3%A9+~%2A&phone=16134443333");
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(!threema("threema://*THEGWID@secret").send(&ctx).await.unwrap());
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/send_simple")).respond_with(ResponseTemplate::new(401)).expect(1).mount(&server).await;
    let svc = from_url("threema://*THEGWID@secret/16134443333").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The phone number fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/send_simple"))
      .and(query_param("phone", "16134443333"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/send_simple")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("threema://*THEGWID@secret/ECHOECHO/16134443333?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "b".into(), ..Default::default() })).await.is_success());
    let sent: Vec<String> = crate::notify::custom_json::requests_to(&server, "POST", "/send_simple")
      .await
      .iter()
      .map(|r| r.url.query_pairs().find(|(k, _)| k == "to" || k == "phone").unwrap().1.into_owned())
      .collect();
    // Targets are sorted, so the phone number goes first
    assert_eq!(sent, vec!["16134443333", "ECHOECHO", "16134443333"]);
  }
}

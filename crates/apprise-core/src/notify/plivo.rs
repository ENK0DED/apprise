use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::json;

/// Recipients per request in batch mode.
const DEFAULT_BATCH_SIZE: usize = 20;

pub struct Plivo {
  auth_id: String,
  auth_token: String,
  source: String,
  targets: Vec<String>,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `^[a-z0-9]{min,max}$`, any case.
fn alnum_len(value: &str, min: usize, max: usize) -> bool {
  (min..=max).contains(&value.len()) && value.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Plivo {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // plivo://auth_id@token/source/targets or plivo://?id=&token=&from=&to=
    // Upstream unquotes these query values once more
    let qs = |key: &str| url.get(key).filter(|v| !v.is_empty());
    let auth_id = qs("id").map(unquote).or_else(|| url.user.clone()).unwrap_or_default();
    let mut entries = url.path_parts.clone();
    let auth_token = match qs("token") {
      Some(token) => {
        // The host is then the first target
        if let Some(host) = url.host.clone() {
          entries.insert(0, host);
        }
        unquote(token)
      }
      None => url.host.clone().unwrap_or_default(),
    };
    let source = match qs("from") {
      Some(from) => Some(unquote(from)),
      None if !entries.is_empty() => Some(entries.remove(0)),
      None => None,
    };
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let batch = url.get("batch").map(parse_bool).unwrap_or(false);

    if !alnum_len(&auth_id, 20, 30) {
      tracing::warn!("An invalid Plivo Auth ID ({}) was specified.", auth_id);
      return None;
    }
    if !alnum_len(&auth_token, 30, 50) {
      tracing::warn!("An invalid Plivo Auth Token was specified.");
      return None;
    }
    let Some(source) = source.as_deref().and_then(|s| is_phone_no(s, 10)) else {
      tracing::warn!("The Plivo source specified ({}) is invalid.", source.unwrap_or_default());
      return None;
    };
    let source = format!("+{}", source.full);

    let mut targets = Vec::new();
    if entries.is_empty() {
      // Notify ourselves
      targets.push(source.clone());
    } else {
      for target in parse_phone_no(&entries) {
        match is_phone_no(&target, 10) {
          Some(phone) => targets.push(format!("+{}", phone.full)),
          None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
        }
      }
    }

    Some(Self { auth_id, auth_token, source, targets, batch, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Plivo",
      service_url: Some("https://plivo.com"),
      setup_url: Some("https://appriseit.com/services/plivo/"),
      protocols: vec!["plivo"],
      description: "Send SMS via Plivo.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Plivo {
  fn schemas(&self) -> &[&str] {
    &["plivo"]
  }
  fn service_name(&self) -> &str {
    "Plivo"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    140
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Plivo targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    // Upstream posts to the unformatted "{auth_id}" template, which the Plivo
    // API rejects; the account's own Auth ID belongs in the path.
    let url = format!("{}/v1/Account/{}/Message/", service_base("https://api.plivo.com"), self.auth_id);
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, chunk) in self.targets.chunks(batch_size).enumerate() {
      // A retry skips batches that already accepted the message
      let key = index.to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }
      let payload = json!({ "src": self.source, "dst": chunk.join("<"), "text": ctx.body });
      let to = if batch_size == 1 { format!(" to {}", chunk[0]) } else { "(s)".to_string() };
      crate::notify::throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .basic_auth(&self.auth_id, Some(&self.auth_token))
        .body(serde_json::to_string(&payload)?)
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200..=202) => {
          tracing::info!("Sent {} Plivo notification{}.", chunk.len(), to);
          crate::delivery::mark_delivered(&key, false);
        }
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!(
            "Failed to send {} Plivo notification{}: {}error={}.",
            chunk.len(),
            to,
            status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending {} Plivo notification(s).", chunk.len());
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
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn plivo(url: &str) -> Plivo {
    Plivo::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_valid_urls() {
    let a25 = "a".repeat(25);
    let b40 = "b".repeat(40);
    let a40 = "a".repeat(40);
    let urls = vec![
      format!("plivo://{}@{}/15551231234", a25, b40),
      // Invalid targets are dropped (notify then fails)
      format!("plivo://{}@{}/15551232000/abcd", a25, a40),
      format!("plivo://{}@{}/15551232000/123", a25, a40),
      format!("plivo://{}@{}/?from=15551233000&to=15551232000&batch=yes", a25, a40),
      format!("plivo://?id={}&token={}&from=15551233000&to=15551232000", a25, a40),
      format!("plivo://15551232123?id={}&token={}&from=15551233000&to=15551232000", a25, a40),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let a10 = "a".repeat(10);
    let a25 = "a".repeat(25);
    let urls = vec![
      "plivo://".to_string(),
      // invalid auth id (too short)
      format!("plivo://{}@{}/15551232000", a10, "a".repeat(25)),
      // invalid token (too short)
      format!("plivo://{}@{}/15551232000", a25, a10),
      // invalid source phone number (too short)
      format!("plivo://{}@{}/123", a25, "a".repeat(40)),
      // invalid source phone number (non-numeric)
      format!("plivo://{}@{}/abc", a25, "a".repeat(40)),
      // no source at all
      format!("plivo://{}@{}", a25, "a".repeat(40)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let a25 = "a".repeat(25);
    let b40 = "b".repeat(40);
    let obj = plivo(&format!("plivo://{}@{}/15551231234", a25, b40));
    assert_eq!(obj.auth_id, a25);
    assert_eq!(obj.auth_token, b40);
    assert_eq!(obj.source, "+15551231234");
    // No targets: the source is notified
    assert_eq!(obj.targets, vec!["+15551231234"]);
    assert!(!obj.batch);

    let obj = plivo(&format!("plivo://?id={}&token={}&from=15551233000&to=15551232000", a25, b40));
    assert_eq!(obj.auth_id, a25);
    assert_eq!(obj.auth_token, b40);
    assert_eq!(obj.source, "+15551233000");
    assert_eq!(obj.targets, vec!["+15551232000"]);

    // With ?token=, the host becomes the first target
    let obj = plivo(&format!("plivo://15551232123?id={}&token={}&from=15551233000&to=15551232000&batch=yes", a25, b40));
    assert_eq!(obj.targets, vec!["+15551232123", "+15551232000"]);
    assert!(obj.batch);

    // Targets given but all invalid: nothing to notify
    assert!(plivo(&format!("plivo://{}@{}/15551232000/abcd", a25, b40)).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = Plivo::static_details();
    assert_eq!(details.service_name, "Plivo");
    assert!(details.protocols.contains(&"plivo"));
    assert_eq!(details.service_url, Some("https://plivo.com"));
  }

  #[tokio::test]
  async fn test_send_one_request_per_target() {
    let a25 = "a".repeat(25);
    let b40 = "b".repeat(40);
    let server = MockServer::start().await;
    for dst in ["+15551231111", "+15551232222"] {
      Mock::given(method("POST"))
        .and(path(format!("/v1/Account/{}/Message/", a25)))
        .and(header("Content-Type", "application/json"))
        .and(header("Authorization", "Basic YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYTpiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJi"))
        .and(body_json(json!({"src": "+15551230000", "dst": dst, "text": "title\r\nbody"})))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("plivo://{}@{}/15551230000/15551231111/15551232222", a25, b40)).unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v1/Account/{}/Message/", "a".repeat(25))))
      .and(body_json(json!({"src": "+15551230000", "dst": "+15551231111<+15551232222", "text": "body"})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("plivo://{}@{}/15551230000/15551231111/15551232222?batch=yes", "a".repeat(25), "b".repeat(40))).unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    // All targets invalid
    let svc = from_url(&format!("plivo://{}@{}/15551232000/abcd", "a".repeat(25), "a".repeat(40))).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // One recipient fails; the other is still sent
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/v1/Account/{}/Message/", "a".repeat(25))))
      .and(body_json(json!({"src": "+15551230000", "dst": "+15551232222", "text": "body"})))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(format!("/v1/Account/{}/Message/", "a".repeat(25))))
      .and(body_json(json!({"src": "+15551230000", "dst": "+15551231111", "text": "body"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("plivo://{}@{}/15551230000/15551231111/15551232222", "a".repeat(25), "b".repeat(40))).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Connection errors fail without raising
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let a25 = "a".repeat(25);
    let server = MockServer::start().await;
    let messages = format!("/v1/Account/{}/Message/", a25);
    // The second number fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path(messages.as_str()))
      .and(body_partial_json(json!({"dst": "+15551232222"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(messages.as_str())).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("plivo://{}@{}/15551230000/15551231111/15551232222?retry=1&wait=0", a25, "b".repeat(40))));
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let sent: Vec<serde_json::Value> = crate::notify::custom_json::requests_to(&server, "POST", &messages)
      .await
      .iter()
      .map(|r| r.body_json::<serde_json::Value>().unwrap()["dst"].clone())
      .collect();
    assert_eq!(sent, vec![json!("+15551231111"), json!("+15551232222"), json!("+15551232222")]);
  }
}

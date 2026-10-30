use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use async_trait::async_trait;

/// API calls (`STRMLABS_CALLS`); alerts are the default.
const STRMLABS_CALLS: &[&str] = &["ALERTS", "DONATIONS"];

/// Alert types (`STRMLABS_ALERTS`), matched case-sensitively.
const STRMLABS_ALERTS: &[&str] = &["follow", "subscription", "donation", "host"];

pub struct Streamlabs {
  access_token: String,
  call: &'static str,
  alert_type: &'static str,
  image_href: String,
  sound_href: String,
  duration: String,
  special_text_color: String,
  amount: String,
  currency: String,
  name: String,
  identifier: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Streamlabs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream unquotes the query values once more
    let qsd = |k: &str| url.get(k).filter(|v| !v.is_empty()).map(unquote);

    // Access tokens are 40 alphanumeric characters
    let access_token = url.host.clone().unwrap_or_default().trim().to_string();
    if access_token.len() != 40 || !access_token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid Streamslabs access token was specified.");
      return None;
    }

    let call = match qsd("call") {
      None => STRMLABS_CALLS[0],
      Some(call) => {
        let upper = call.trim().to_uppercase();
        let Some(found) = STRMLABS_CALLS.iter().copied().find(|c| *c == upper) else {
          tracing::warn!("The streamlabs call specified ({}) is invalid.", call);
          return None;
        };
        found
      }
    };

    let alert_type = match qsd("alert_type") {
      None => "donation",
      Some(alert) => {
        let Some(found) = STRMLABS_ALERTS.iter().copied().find(|a| *a == alert) else {
          tracing::warn!("The streamlabs alert type specified ({}) is invalid.", alert);
          return None;
        };
        found
      }
    };

    let currency = qsd("currency").map(|c| c.trim().to_uppercase()).unwrap_or_else(|| "USD".into());
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
      tracing::warn!("An invalid Streamslabs currency was specified.");
      return None;
    }

    // Donors are 2 to 25 characters, starting with a non-space
    let name = qsd("name").map(|n| n.trim().to_string()).unwrap_or_else(|| "Anon".into());
    if !(2..=25).contains(&name.chars().count()) || name.starts_with(char::is_whitespace) {
      tracing::warn!("An invalid Streamslabs donor was specified.");
      return None;
    }

    Some(Self {
      access_token,
      call,
      alert_type,
      image_href: qsd("image_href").unwrap_or_default(),
      sound_href: qsd("sound_href").map(|s| s.trim().to_string()).unwrap_or_default(),
      duration: qsd("duration").map(|d| d.trim().to_uppercase()).unwrap_or_else(|| "1000".into()),
      special_text_color: qsd("special_text_color").map(|c| c.trim().to_uppercase()).unwrap_or_default(),
      amount: qsd("amount").unwrap_or_else(|| "0".into()),
      currency,
      name,
      identifier: qsd("identifier").map(|i| i.trim().to_string()).unwrap_or_else(|| "Apprise".into()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Streamlabs",
      service_url: Some("https://streamlabs.com/"),
      setup_url: Some("https://appriseit.com/services/streamlabs/"),
      protocols: vec!["strmlabs"],
      description: "Send alerts and donations via Streamlabs.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Streamlabs {
  fn schemas(&self) -> &[&str] {
    &["strmlabs"]
  }
  fn service_name(&self) -> &str {
    "Streamlabs"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    255
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let has_title = !ctx.title.is_empty();
    let form: Vec<(&str, String)> = if self.call == "ALERTS" {
      vec![
        ("access_token", self.access_token.clone()),
        ("type", self.alert_type.to_string()),
        ("image_href", self.image_href.clone()),
        ("sound_href", self.sound_href.clone()),
        ("message", if has_title { ctx.title.clone() } else { ctx.body.clone() }),
        ("user_message", if has_title { ctx.body.clone() } else { String::new() }),
        ("duration", self.duration.clone()),
        ("special_text_color", self.special_text_color.clone()),
      ]
    } else {
      let message = if has_title { format!("{}\r\n{}", ctx.title, ctx.body) } else { ctx.body.clone() };
      vec![
        ("name", self.name.clone()),
        ("identifier", self.identifier.clone()),
        ("amount", self.amount.clone()),
        ("currency", self.currency.clone()),
        ("access_token", self.access_token.clone()),
        ("message", message.chars().take(254).collect()),
      ]
    };

    let kind = if self.call == "ALERTS" { "alert" } else { "donation" };
    let url = format!("{}/api/v1.0/{}", service_base("https://streamlabs.com"), self.call.to_lowercase());
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = match client.post(&url).header("User-Agent", APP_ID).form(&form).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Streamlabs {}.", kind);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };
    if resp.status().as_u16() != 200 {
      let reason = resp.status().canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Streamlabs {}: {}{}error={}.", kind, reason, if reason.is_empty() { "" } else { ", " }, resp.status().as_u16());
      return Ok(false);
    }
    tracing::info!("Sent Streamlabs {}.", kind);
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "IcIcArukDQtuC1is1X1UdKZjTg118Lag2vScOmso";

  fn streamlabs(query: &str) -> Streamlabs {
    Streamlabs::from_url(&ParsedUrl::parse(&format!("strmlabs://{}/{}", TOKEN, query)).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("strmlabs://{}", TOKEN),
      format!("strmlabs://{}/?name=tt&identifier=pyt&amount=20&currency=USD&call=donations", TOKEN),
      format!("strmlabs://{}/?image_href=https://example.org/rms.jpg&sound_href=https://example.org/rms.mp3", TOKEN),
      format!("strmlabs://{}/?duration=1000&image_href=&sound_href=&alert_type=donation&special_text_color=crimson", TOKEN),
      format!("strmlabs://{}/?call=alerts", TOKEN),
      format!("strmlabs://{}/?call=donations", TOKEN),
      // identifier= is not validated upstream
      format!("strmlabs://{}/?identifier=x", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "strmlabs://".to_string(),
      "strmlabs://a_bd_/".to_string(),
      format!("strmlabs://{}/?currency=ABCD", TOKEN),
      format!("strmlabs://{}/?name=tt&identifier=pyt&amount=20&currency=USD&call=rms", TOKEN),
      format!("strmlabs://{}/?name=tt&identifier=pyt&amount=20&currency=USD&alert_type=rms", TOKEN),
      format!("strmlabs://{}/?name=t", TOKEN),
      // Alert types are case-sensitive
      format!("strmlabs://{}/?alert_type=Donation", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_options() {
    let s = streamlabs("");
    assert_eq!((s.call, s.alert_type, s.currency.as_str(), s.name.as_str(), s.identifier.as_str()), ("ALERTS", "donation", "USD", "Anon", "Apprise"));
    assert_eq!((s.amount.as_str(), s.duration.as_str()), ("0", "1000"));
    // Only call, currency, duration and the text colour are upper-cased (upstream dba5227b)
    let s = streamlabs("?name=MixedCase&identifier=PyT&sound_href=https://x/Sound.mp3&currency=usd&call=Donations&special_text_color=crimson");
    assert_eq!((s.name.as_str(), s.identifier.as_str(), s.sound_href.as_str()), ("MixedCase", "PyT", "https://x/Sound.mp3"));
    assert_eq!((s.currency.as_str(), s.call, s.special_text_color.as_str()), ("USD", "DONATIONS", "CRIMSON"));
  }

  #[tokio::test]
  async fn test_send_alert() {
    // Matches pycapture.py for this URL
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1.0/alerts"))
      .and(body_string(format!(
        "access_token={}&type=donation&image_href=&sound_href=https%3A%2F%2Fx%2Fs.mp3&message=T&user_message=B&duration=1000&special_text_color=CRIMSON",
        TOKEN
      )))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc =
      from_url(&format!("strmlabs://{}/?duration=1000&image_href=&sound_href=%20https://x/s.mp3&alert_type=donation&special_text_color=crimson", TOKEN))
        .unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_alert_without_title() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1.0/alerts"))
      .and(body_string(format!("access_token={}&type=donation&image_href=&sound_href=&message=B&user_message=&duration=1000&special_text_color=", TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("strmlabs://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_donation() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1.0/donations"))
      .and(body_string(format!("name=MixedCase&identifier=PyT&amount=20&currency=USD&access_token={}&message=T%0D%0AB", TOKEN)))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("strmlabs://{}/?name=MixedCase&identifier=PyT&amount=20&currency=usd&call=donations", TOKEN)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for (call, status) in [("alerts", 500), ("donations", 500), ("alerts", 999), ("donations", 201)] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path(format!("/api/v1.0/{}", call))).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("strmlabs://{}/?call={}", TOKEN, call)).unwrap();
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Streamlabs::static_details();
    assert_eq!(details.service_name, "Streamlabs");
    assert_eq!(details.protocols, vec!["strmlabs"]);
    assert_eq!(from_url(&format!("strmlabs://{}", TOKEN)).unwrap().body_maxlen(), 255);
  }
}

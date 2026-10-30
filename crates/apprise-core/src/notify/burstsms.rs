use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::{url_credentials, url_host};
use async_trait::async_trait;

/// Country codes (`?country=`).
const COUNTRY_CODES: [&str; 4] = ["au", "nz", "gb", "us"];

/// Most numbers one request may carry in batch mode.
const DEFAULT_BATCH_SIZE: usize = 500;

pub struct BurstSms {
  apikey: String,
  secret: String,
  source: String,
  country: String,
  /// Parsed and validated as upstream does; upstream never sends it.
  #[allow(dead_code)]
  validity: i64,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_alnum(s: &str) -> bool {
  // ^[a-z0-9]+$ (case-insensitive)
  !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
}

impl BurstSms {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = url_credentials(url);
    let apikey = url.get("key").filter(|k| !k.is_empty()).map(str::to_string).or(user).unwrap_or_default();
    let secret = url.get("secret").filter(|s| !s.is_empty()).map(str::to_string).or(password).unwrap_or_default();
    let source =
      url.get("source").filter(|s| !s.is_empty()).or_else(|| url.get("from").filter(|s| !s.is_empty())).map(str::to_string).unwrap_or_else(|| url_host(url));

    if !is_alnum(&apikey) {
      tracing::warn!("An invalid Burst SMS API Key was specified.");
      return None;
    }
    if !is_alnum(&secret) {
      tracing::warn!("An invalid Burst SMS API Secret was specified.");
      return None;
    }

    // Upstream checks the value as given, before lower-casing it
    let country = match url.get("country").filter(|c| !c.is_empty()) {
      Some(c) if COUNTRY_CODES.contains(&c) => c.to_string(),
      Some(c) => {
        tracing::warn!("An invalid Burst SMS country ({}) was specified.", c);
        return None;
      }
      None => "us".to_string(),
    };

    let validity = match url.get("validity").filter(|v| !v.is_empty()) {
      Some(v) => match v.trim().parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
          tracing::warn!("The Burst SMS Validity specified ({}) is invalid.", v);
          return None;
        }
      },
      None => 0,
    };

    // validate_regex(): must start with a non-whitespace character
    if source.is_empty() || source.starts_with(char::is_whitespace) {
      tracing::warn!("The Account Sender ID specified ({}) is invalid.", source);
      return None;
    }
    let source = source.trim().to_string();

    let mut raw_targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(p) => targets.push(p.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self {
      apikey,
      secret,
      source,
      country,
      validity,
      batch: url.get("batch").filter(|b| !b.is_empty()).map(parse_bool).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Burst SMS",
      service_url: Some("https://burstsms.com/"),
      setup_url: Some("https://appriseit.com/services/burstsms/"),
      protocols: vec!["burstsms"],
      description: "Send SMS via Burst SMS.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for BurstSms {
  fn schemas(&self) -> &[&str] {
    &["burstsms"]
  }
  fn service_name(&self) -> &str {
    "Burst SMS"
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
      tracing::warn!("There are no valid Burst SMS targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/send-sms.json", service_base("https://api.transmitsms.com"));
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for batch in self.targets.chunks(batch_size) {
      let to = batch.join(",");
      let payload = [("countrycode", self.country.as_str()), ("message", ctx.body.as_str()), ("from", self.source.as_str()), ("to", to.as_str())];
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .basic_auth(&self.apikey, Some(&self.secret))
        .form(&payload)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Burst SMS notification to {} target(s).", batch.len());
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 {
        tracing::warn!("Failed to send Burst SMS notification to {} target(s): error={}.", batch.len(), status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Burst SMS notification to {} target(s).", batch.len());
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

  fn parse(url: &str) -> BurstSms {
    BurstSms::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let (a, b, s, t) = ("a".repeat(8), "b".repeat(16), "5".repeat(11), "6".repeat(11));
    let urls = vec![
      "burstsms://".to_string(),
      "burstsms://:@/".to_string(),
      // no secret
      format!("burstsms://{}@12345678", "a".repeat(8)),
      // blank sender id
      format!("burstsms://{}:{}@%20", "d".repeat(8), "e".repeat(16)),
      format!("burstsms://_?key={}&secret={}&source={}&to={}&country=invalid", a, b, s, t),
      // upstream compares the country before lower-casing it
      format!("burstsms://_?key={}&secret={}&source={}&to={}&country=US", a, b, s, t),
      format!("burstsms://_?key={}&secret={}&source={}&to={}&validity=invalid", a, b, s, t),
      // invalid key / secret characters
      format!("burstsms://ab-cd:{}@{}/{}", b, s, t),
      format!("burstsms://{}:ab_cd@{}/{}", a, s, t),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (a, b, s, t) = ("a".repeat(8), "b".repeat(16), "5".repeat(11), "6".repeat(11));
    let urls = vec![
      // These load but have no valid targets
      format!("burstsms://{}:{}@{}/123/{}/abcd/", "f".repeat(8), "g".repeat(16), "3".repeat(11), "9".repeat(15)),
      format!("burstsms://{}:{}@{}", "h".repeat(8), "i".repeat(16), "5".repeat(11)),
      format!("burstsms://_?key={}&secret={}&from={}&to={}", a, b, s, t),
      format!("burstsms://_?key={}&secret={}&from={}&to={}&batch=y", a, b, s, t),
      format!("burstsms://_?key={}&secret={}&source={}&to={}&country=us", a, b, s, t),
      format!("burstsms://_?key={}&secret={}&source={}&to={}&validity=10", a, b, s, t),
      format!("burstsms://{}:{}@{}/{}", a, b, "6".repeat(11), "7".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("burstsms://apikey123:secret456@Acme/+1 555 123 1234/15557654321/123?country=gb&validity=10&batch=yes");
    assert_eq!(obj.apikey, "apikey123");
    assert_eq!(obj.secret, "secret456");
    assert_eq!(obj.source, "Acme");
    assert_eq!(obj.country, "gb");
    assert_eq!(obj.validity, 10);
    assert!(obj.batch);
    assert_eq!(obj.targets, vec!["15551231234", "15557654321"]);

    // Query arguments override the URL parts; ?source= wins over ?from=
    let obj = parse("burstsms://k:s@host?key=apikey&secret=secret&from=FromId&source=SourceId&to=15551231234");
    assert_eq!(obj.apikey, "apikey");
    assert_eq!(obj.secret, "secret");
    assert_eq!(obj.source, "SourceId");
    assert_eq!(obj.country, "us");
    assert!(!obj.batch);
    assert_eq!(obj.targets, vec!["15551231234"]);

    // The host is the sender id; every path entry here is invalid
    assert!(parse(&format!("burstsms://{}:{}@{}/123/{}/abcd/", "f".repeat(8), "g".repeat(16), "3".repeat(11), "9".repeat(15))).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = BurstSms::static_details();
    assert_eq!(details.service_name, "Burst SMS");
    assert_eq!(details.protocols, vec!["burstsms"]);
    let obj = parse("burstsms://k:s@Acme/15551231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["15551231234", "15557654321"] {
      Mock::given(method("POST"))
        .and(path("/send-sms.json"))
        .and(header("Authorization", "Basic YXBpa2V5MTIzOnNlY3JldDQ1Ng=="))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/x-www-form-urlencoded"))
        .and(body_string(format!("countrycode=us&message=title%0D%0Abody&from=Acme&to={}", to)))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("burstsms://apikey123:secret456@Acme/15551231234/15557654321").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/send-sms.json"))
      .and(body_string("countrycode=nz&message=hello&from=Acme&to=15551231234%2C15557654321"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("burstsms://apikey123:secret456@Acme/15551231234/15557654321?batch=yes&validity=30&country=nz").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url(&format!("burstsms://{}:{}@{}", "h".repeat(8), "i".repeat(16), "5".repeat(11))).unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_error_response() {
    let server = MockServer::start().await;
    let reply = r#"{"error": {"code": "FIELD_INVALID", "description": "Sender ID must be one of the numbers that are currently leased."}}"#;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_string(reply)).expect(2).mount(&server).await;
    let svc = from_url("burstsms://apikey123:secret456@Acme/15551231234/15557654321").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_only_200_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url("burstsms://apikey123:secret456@Acme/15551231234").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}

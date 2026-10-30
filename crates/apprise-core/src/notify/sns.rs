use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::aws::{AwsResponse, SigV4Request, flatten_response, sign};
use crate::utils::parse::ParsedUrl;
use crate::utils::targets::{is_phone_no, parse_list, split_path};
use async_trait::async_trait;

const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";
const API_VERSION: &str = "2010-03-31";

/// XML response tags kept from every SNS reply.
const AWS_KEEP_MAP: &[(&str, &str)] = &[
  ("RequestId", "request_id"),
  ("TopicArn", "topic_arn"),
  ("MessageId", "message_id"),
  ("Type", "error_type"),
  ("Code", "error_code"),
  ("Message", "error_message"),
];

/// How a notification is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnsMode {
  /// Plain SMS: no title field, 160 character body.
  Sms,
  /// Topic publishing: title is sent as the `Subject`, 256 KB body.
  Topic,
}

impl SnsMode {
  const ALL: [(&'static str, SnsMode); 2] = [("sms", SnsMode::Sms), ("topic", SnsMode::Topic)];

  /// Upstream accepts any prefix of a mode name (`t`, `to`, ...).
  fn from_prefix(value: &str) -> Option<Self> {
    let value = value.to_lowercase();
    Self::ALL.iter().find(|(name, _)| name.starts_with(&value)).map(|(_, mode)| *mode)
  }
}

pub struct Sns {
  access_key: String,
  secret_key: String,
  region: String,
  session_token: Option<String>,
  phones: Vec<String>,
  topics: Vec<String>,
  mode: SnsMode,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `IS_REGION`: `^\s*([a-z]{2})-([a-z-]+?)-([0-9]+)\s*$` (any case).
/// Returns the region lowercased.
fn match_region(entry: &str) -> Option<String> {
  let entry = entry.trim();
  let (head, no) = entry.rsplit_once('-')?;
  if no.is_empty() || !no.chars().all(|c| c.is_ascii_digit()) {
    return None;
  }
  let (country, area) = head.split_once('-')?;
  let valid_country = country.len() == 2 && country.chars().all(|c| c.is_ascii_alphabetic());
  let valid_area = !area.is_empty() && area.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
  (valid_country && valid_area).then(|| format!("{}-{}-{}", country.to_lowercase(), area.to_lowercase(), no))
}

/// Upstream `IS_TOPIC`: `^#?([A-Za-z0-9_-]+)\s*$`.
fn match_topic(entry: &str) -> Option<String> {
  let name = entry.strip_prefix('#').unwrap_or(entry).trim_end();
  (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| name.to_string())
}

/// Upstream `validate_regex()` with its default `[^\s]+` pattern: the value
/// must not be empty or start with whitespace, and is returned trimmed.
fn validate_token(value: Option<&str>) -> Option<String> {
  let value = value?;
  (value.chars().next().is_some_and(|c| !c.is_whitespace())).then(|| value.trim().to_string())
}

/// Python's `str.isprintable()`, closely enough for a subject line.
fn is_printable(c: char) -> bool {
  !(c.is_control()
    || (c.is_whitespace() && c != ' ')
    || matches!(c, '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}'))
}

/// Topic subjects must be printable ASCII-ish text on a single line.
fn clean_subject(title: &str, max: usize) -> String {
  let printable: String = title.chars().map(|c| if is_printable(c) { c } else { ' ' }).collect();
  printable.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// Form-encode `pairs` the way upstream `urlencode()` does (`%20` for spaces,
/// nothing left unescaped but `A-Za-z0-9-_.~`).
fn form_encode(pairs: &[(&str, &str)]) -> String {
  pairs.iter().map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v))).collect::<Vec<_>>().join("&")
}

impl Sns {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qs = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    // sns://[token@]access_key/secret/key/parts/region/targets
    // The secret key may hold '/' so everything before the region is it.
    let entries = split_path(&url.path_parts);
    let mut secret_from_path = None;
    let mut region_from_path = None;
    let mut index = 0;
    for (i, entry) in entries.iter().enumerate() {
      if let Some(region) = match_region(entry) {
        secret_from_path = Some(entries[..i].join("/"));
        region_from_path = Some(region);
        index = i + 1;
        break;
      }
    }
    let mut targets: Vec<String> = entries[index..].to_vec();
    if let Some(to) = qs("to") {
      targets.extend(parse_list([to], true));
    }

    let secret = qs("secret").map(str::to_string).or(secret_from_path);
    let access = qs("key").or_else(|| qs("access")).map(str::to_string).or_else(|| url.host.clone());
    let region = qs("region").map(str::to_string).or(region_from_path);
    let session_token = qs("token").map(str::to_string).or_else(|| url.user.clone().filter(|u| !u.is_empty()));

    let Some(access_key) = validate_token(access.as_deref()) else {
      tracing::warn!("An invalid AWS Access Key ID was specified.");
      return None;
    };
    let Some(secret_key) = validate_token(secret.as_deref()) else {
      tracing::warn!("An invalid AWS Secret Access Key was specified.");
      return None;
    };
    // Upstream validates `^[a-z]{2}-[a-z-]+?-[0-9]+$` but keeps the case
    let Some(region) = region.filter(|r| match_region(r).is_some() && r.trim() == r) else {
      tracing::warn!("An invalid AWS Region was specified.");
      return None;
    };

    let mut phones = Vec::new();
    let mut topics = Vec::new();
    for target in parse_list(&targets, true) {
      if let Some(phone) = is_phone_no(&target, 10) {
        phones.push(format!("+{}", phone.full));
      } else if let Some(topic) = match_topic(&target) {
        topics.push(topic);
      } else {
        tracing::warn!("Dropped invalid phone/topic ({}) specified.", target);
      }
    }

    let mode = match url.get("mode").map(str::trim).filter(|m| !m.is_empty()) {
      Some(m) => match SnsMode::from_prefix(m) {
        Some(mode) => mode,
        None => {
          tracing::warn!("The AWS SNS mode specified ({}) is invalid.", m);
          return None;
        }
      },
      None if !topics.is_empty() && phones.is_empty() => SnsMode::Topic,
      None => SnsMode::Sms,
    };

    Some(Self { access_key, secret_key, region, session_token, phones, topics, mode, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "AWS Simple Notification Service (SNS)",
      service_url: Some("https://aws.amazon.com/sns/"),
      setup_url: Some("https://appriseit.com/services/sns/"),
      protocols: vec!["sns"],
      description: "Send SMS messages or topic notifications via AWS SNS.",
      attachment_support: false,
    }
  }

  /// POST one signed action; returns whether AWS answered 200 and the
  /// flattened XML reply.
  async fn post(&self, client: &reqwest::Client, payload: &[(&str, &str)], to: &str) -> (bool, AwsResponse) {
    let body = form_encode(payload);
    let host = format!("sns.{}.amazonaws.com", self.region);
    let signed = sign(
      &SigV4Request {
        method: "POST",
        host: &host,
        uri: "/",
        query: "",
        body: body.as_bytes(),
        content_type: CONTENT_TYPE,
        access_key: &self.access_key,
        secret_key: &self.secret_key,
        session_token: self.session_token.as_deref(),
        region: &self.region,
        service: "sns",
      },
      chrono::Utc::now(),
    );
    let url = format!("{}/", service_base(&format!("https://{}", host)));
    tracing::debug!("AWS POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("AWS Payload: {}", body);

    let mut req = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", CONTENT_TYPE)
      .header("X-Amz-Date", &signed.amz_date)
      .header("Authorization", &signed.authorization);
    if let Some(token) = &self.session_token {
      req = req.header("X-Amz-Security-Token", token);
    }
    match req.body(body).send().await {
      Ok(resp) => {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed = flatten_response(&text, AWS_KEEP_MAP);
        if status != reqwest::StatusCode::OK {
          let reason = if status.as_u16() == 403 { Some("Unauthorized - Invalid Access/Secret Key Combination.") } else { status.canonical_reason() };
          tracing::warn!(
            "Failed to send AWS notification to {}: {}{}error={}.",
            to,
            reason.unwrap_or(""),
            if reason.is_some() { ", " } else { "" },
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", text.chars().take(2000).collect::<String>());
          return (false, parsed);
        }
        tracing::info!("Sent AWS notification to \"{}\".", to);
        (true, parsed)
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending AWS notification to \"{}\".", to);
        tracing::debug!("Socket Exception: {}", e);
        (false, AwsResponse::default())
      }
    }
  }
}

#[async_trait]
impl Notify for Sns {
  fn schemas(&self) -> &[&str] {
    &["sns"]
  }
  fn service_name(&self) -> &str {
    "AWS Simple Notification Service (SNS)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    if self.mode == SnsMode::Topic { 256000 } else { 160 }
  }
  fn title_maxlen(&self) -> usize {
    if self.mode == SnsMode::Topic { 99 } else { 0 }
  }
  fn request_rate_per_sec(&self) -> f64 {
    2.5
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.phones.is_empty() && self.topics.is_empty() {
      tracing::warn!("No AWS targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let topic_title = (self.mode == SnsMode::Topic && !ctx.title.is_empty()).then_some(ctx.title.as_str());
    let mut has_error = false;

    for phone in &self.phones {
      let message = match topic_title {
        Some(title) => format!("{}\r\n{}", title, ctx.body),
        None => ctx.body.clone(),
      };
      let payload = [("Action", "Publish"), ("Message", message.as_str()), ("Version", API_VERSION), ("PhoneNumber", phone.as_str())];
      if !self.post(&client, &payload, phone).await.0 {
        has_error = true;
      }
    }

    for topic in &self.topics {
      // Creating a topic that already exists just returns its ARN
      let (ok, response) = self.post(&client, &[("Action", "CreateTopic"), ("Version", API_VERSION), ("Name", topic.as_str())], topic).await;
      if !ok {
        has_error = true;
        continue;
      }
      let Some(topic_arn) = response.get("topic_arn").filter(|arn| !arn.is_empty()) else {
        tracing::warn!("AWS SNS did not return a TopicArn for {}.", topic);
        has_error = true;
        continue;
      };

      let subject = topic_title.map(|title| clean_subject(title, self.title_maxlen()));
      let mut payload = vec![("Action", "Publish"), ("Version", API_VERSION), ("TopicArn", topic_arn), ("Message", ctx.body.as_str())];
      if let Some(subject) = &subject {
        payload.push(("Subject", subject.as_str()));
      }
      if !self.post(&client, &payload, topic).await.0 {
        has_error = true;
      }
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

  const KEY_ID: &str = "AHIAJGNT76XIMXDBIJYA";
  const SECRET: &str = "bu1dHSdO22pfaaVy/wmNsdljF4C07D3bndi9PQJ9";
  const REGION: &str = "us-east-2";
  const SESSION_TOKEN: &str = "FwoGZXIvYXdz+SESSION/TOKENABCDEFGHIJKLMNOPQRS=";

  fn sns(url: &str) -> Sns {
    Sns::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  fn arn_response(topic: &str) -> String {
    format!(
      "<CreateTopicResponse xmlns=\"http://sns.amazonaws.com/doc/2010-03-31/\"><CreateTopicResult><TopicArn>arn:aws:sns:us-east-2:000000000000:{}</TopicArn></CreateTopicResult>\
       <ResponseMetadata><RequestId>604bef0f-369c-50c5-a7a4-bbd474c83d6a</RequestId></ResponseMetadata></CreateTopicResponse>",
      topic
    )
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444",
      "sns://?access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlazkcevi7FQ&region=us-west-2&to=12223334444",
      "sns://T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/12223334444/12223334445",
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/us-east-1?to=12223334444",
      "sns://SESSIONTOKEN@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444",
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444?token=SESSIONTOKEN",
      "sns://?key=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlazkcevi7FQ&region=us-west-2&to=12223334444",
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444?mode=sms",
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444?mode=topic",
      // No recipients still loads (and fails at notify time)
      "sns://norecipient/norecipient/us-west-2",
      "sns://oh/yeah/us-west-2/abcdtopic/+12223334444",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "sns://",
      "sns://:@/",
      "sns://T1JJ3T3L2",
      "sns://T1JJ3TD4JD/TIiajkdnlazk7FQ/",
      "sns://nosecret",
      "sns://nosecret/noregion/",
      "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444?mode=invalid",
      // A region given by argument must still look like one
      "sns://T1JJ3T3L2/A1BRTD4JD/?region=nowhere",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_url_parsing() {
    let s = sns(&format!("sns://{}/{}/{}/", KEY_ID, SECRET, REGION));
    assert_eq!(s.access_key, KEY_ID);
    assert_eq!(s.secret_key, SECRET);
    assert_eq!(s.region, REGION);
    assert!(s.phones.is_empty() && s.topics.is_empty());

    // The region is matched in any case and stored lowercase
    let s = sns(&format!("sns://{}/{}/{}/+18001234567/MyTopic/", KEY_ID, SECRET, REGION.to_uppercase()));
    assert_eq!(s.region, REGION);
    assert_eq!(s.secret_key, SECRET);
    assert_eq!(s.phones, vec!["+18001234567"]);
    assert_eq!(s.topics, vec!["MyTopic"]);
    assert_eq!(s.mode, SnsMode::Sms);

    // Invalid targets are dropped; short numbers and bracketed topics
    let s = sns(&format!("sns://{}/{}/{}/+1809/%23(invalid-topic)", KEY_ID, SECRET, REGION));
    assert!(s.phones.is_empty() && s.topics.is_empty());

    // Query-string credentials; key= wins over access=
    let s = sns(&format!("sns://?key={}&access=OTHERID&secret={}&region={}&to=+18001234567,%23Topic", KEY_ID, SECRET, REGION));
    assert_eq!(s.access_key, KEY_ID);
    assert_eq!(s.secret_key, SECRET);
    assert_eq!(s.phones, vec!["+18001234567"]);
    assert_eq!(s.topics, vec!["Topic"]);
  }

  #[test]
  fn test_session_token() {
    let s = sns(&format!("sns://{}@{}/{}/{}/+18001234567", urlencoding::encode(SESSION_TOKEN), KEY_ID, SECRET, REGION));
    assert_eq!(s.session_token.as_deref(), Some(SESSION_TOKEN));
    // ?token= wins over the user part
    let s = sns(&format!("sns://OTHERTOKEN@{}/{}/{}/+18001234567?token={}", KEY_ID, SECRET, REGION, urlencoding::encode(SESSION_TOKEN)));
    assert_eq!(s.session_token.as_deref(), Some(SESSION_TOKEN));
    let s = sns(&format!("sns://{}/{}/{}/+18001234567", KEY_ID, SECRET, REGION));
    assert_eq!(s.session_token, None);
  }

  #[test]
  fn test_mode_detection() {
    let base = format!("sns://{}/{}/{}", KEY_ID, SECRET, REGION);
    let s = sns(&format!("{}/%23MyTopic", base));
    assert_eq!(s.mode, SnsMode::Topic);
    assert_eq!(s.title_maxlen(), 99);
    assert_eq!(s.body_maxlen(), 256000);

    let s = sns(&format!("{}/+18001234567", base));
    assert_eq!(s.mode, SnsMode::Sms);
    assert_eq!(s.title_maxlen(), 0);
    assert_eq!(s.body_maxlen(), 160);

    assert_eq!(sns(&format!("{}/+18001234567/%23MyTopic", base)).mode, SnsMode::Sms);
    assert_eq!(sns(&base).mode, SnsMode::Sms);
    assert_eq!(sns(&format!("{}/+18001234567?mode=topic", base)).mode, SnsMode::Topic);
    assert_eq!(sns(&format!("{}/%23MyTopic?mode=sms", base)).mode, SnsMode::Sms);
    assert_eq!(sns(&format!("{}/%23MyTopic?mode=to", base)).mode, SnsMode::Topic);
    assert_eq!(sns(&format!("{}/%23MyTopic?mode=S", base)).mode, SnsMode::Sms);
    assert!(Sns::from_url(&ParsedUrl::parse(&format!("{}/%23MyTopic?mode=invalid", base)).unwrap()).is_none());
  }

  #[test]
  fn test_clean_subject() {
    assert_eq!(clean_subject("Build\r\n failed\x07\ton host1", 99), "Build failed on host1");
    assert_eq!(clean_subject(&"t".repeat(150), 99), "t".repeat(99));
  }

  #[test]
  fn test_service_details() {
    let details = Sns::static_details();
    assert_eq!(details.service_name, "AWS Simple Notification Service (SNS)");
    assert_eq!(details.service_url, Some("https://aws.amazon.com/sns/"));
    assert!(details.protocols.contains(&"sns"));
    assert!(!details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_sms() {
    // Payload verified against upstream pycapture output
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(header("Content-Type", CONTENT_TYPE))
      .and(body_string("Action=Publish&Message=My%20Body&Version=2010-03-31&PhoneNumber=%2B12223334444"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444").unwrap();
    let ctx = NotifyContext { body: "My Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let req = &server.received_requests().await.unwrap()[0];
    let auth = req.headers.get("Authorization").unwrap().to_str().unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=T1JJ3T3L2/"));
    assert!(auth.contains("/us-west-2/sns/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature="));
    assert!(req.headers.get("X-Amz-Date").is_some());
    assert!(req.headers.get("X-Amz-Security-Token").is_none());
  }

  #[tokio::test]
  async fn test_send_session_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(header("X-Amz-Security-Token", SESSION_TOKEN)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let svc = from_url(&format!("sns://{}/{}/{}/+18001234567?token={}", KEY_ID, SECRET, REGION, urlencoding::encode(SESSION_TOKEN))).unwrap();
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("Authorization").unwrap().to_str().unwrap().contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token,"));
  }

  #[tokio::test]
  async fn test_send_topic_mode() {
    // Request sequence verified against upstream pycapture output
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Action=CreateTopic&Version=2010-03-31&Name=MyTopic"))
      .respond_with(ResponseTemplate::new(200).set_body_string(arn_response("MyTopic")))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_string("Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-2%3A000000000000%3AMyTopic&Message=My%20Body&Subject=My%20Title"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("sns://{}/{}/{}/%23MyTopic", KEY_ID, SECRET, REGION)).unwrap();
    let ctx = NotifyContext { title: "My Title".into(), body: "My Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_topic_without_title_has_no_subject() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Action=CreateTopic&Version=2010-03-31&Name=MyTopic"))
      .respond_with(ResponseTemplate::new(200).set_body_string(arn_response("MyTopic")))
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_string("Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-2%3A000000000000%3AMyTopic&Message=My%20Body"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("sns://{}/{}/{}/%23MyTopic", KEY_ID, SECRET, REGION)).unwrap();
    let ctx = NotifyContext { body: "My Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_forced_topic_mode_on_phone_prepends_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Action=Publish&Message=My%20Title%0D%0AMy%20Body&Version=2010-03-31&PhoneNumber=%2B18001234567"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("sns://{}/{}/{}/+18001234567?mode=topic", KEY_ID, SECRET, REGION)).unwrap();
    let ctx = NotifyContext { title: "My Title".into(), body: "My Body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };

    // No targets
    let svc = sns(&format!("sns://{}/{}/{}", KEY_ID, SECRET, REGION));
    assert!(!svc.send(&ctx).await.unwrap());

    // SMS publish rejected
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(400).set_body_string("<ErrorResponse><Error><Message>Fail</Message></Error></ErrorResponse>"))
      .mount(&server)
      .await;
    let svc = sns(&format!("sns://{}/{}/{}/+15555555555", KEY_ID, SECRET, REGION));
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Topic creation rejected: no publish is attempted
    let svc = sns(&format!("sns://{}/{}/{}/%23MyTopic", KEY_ID, SECRET, REGION));
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    // Topic created without an ARN in the reply
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("<CreateTopicResponse></CreateTopicResponse>")).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // Topic created but publishing fails; the other topic still goes out
    let server = MockServer::start().await;
    for topic in ["BadTopic", "GoodTopic"] {
      Mock::given(method("POST"))
        .and(body_string(format!("Action=CreateTopic&Version=2010-03-31&Name={}", topic)))
        .respond_with(ResponseTemplate::new(200).set_body_string(arn_response(topic)))
        .mount(&server)
        .await;
    }
    Mock::given(method("POST"))
      .and(body_string("Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-2%3A000000000000%3ABadTopic&Message=test"))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_string("Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-2%3A000000000000%3AGoodTopic&Message=test"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = sns(&format!("sns://{}/{}/{}/%23GoodTopic/%23BadTopic", KEY_ID, SECRET, REGION));
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_connection_error() {
    let svc = sns(&format!("sns://{}/{}/{}/+15555555555", KEY_ID, SECRET, REGION));
    let ctx = NotifyContext { body: "test".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}

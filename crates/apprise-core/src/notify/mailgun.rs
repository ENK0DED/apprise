use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{formataddr, is_email, parse_emails, parse_emails_all, valid_addresses};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::qsd::{Qsd, unquote, validate_token};
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

/// Recipients per request in batch mode.
const BATCH_SIZE: usize = 2000;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(200);

/// Sender display name.
#[derive(Debug, Clone, PartialEq)]
enum SenderName {
  /// Upstream default: the asset's app id.
  AppId,
  Named(String),
  /// `?from=` held a bare address: no display name.
  Bare,
}

pub struct Mailgun {
  apikey: String,
  domain: String,
  from_name: SenderName,
  from_email: String,
  /// `(display name, address)`
  targets: Vec<(Option<String>, String)>,
  cc: Vec<String>,
  bcc: Vec<String>,
  names: HashMap<String, Option<String>>,
  /// `+name=value` arguments, sent as `h:name`.
  headers: Vec<(String, String)>,
  /// `:name=value` arguments, sent as `v:name`.
  tokens: Vec<(String, String)>,
  region: String,
  batch: bool,
  throttle: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Mailgun {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = Qsd::from_url(url);
    let domain = url.host.clone().unwrap_or_default();

    // The first path entry is the API key; the rest are recipients
    let mut path = url.path_parts.iter();
    let apikey = path.next().and_then(|k| validate_token(k))?;
    let user = url.user.clone().filter(|u| !u.is_empty())?;

    let region = match qsd.value("region") {
      Some(r) => unquote(r).to_lowercase(),
      None => "us".to_string(),
    };
    if region != "us" && region != "eu" {
      tracing::warn!("The Mailgun region specified ({}) is invalid.", region);
      return None;
    }

    // ?from= is an address (optionally named by ?name=); ?name= alone only
    // names the default sender
    let from_arg = match (qsd.value("from"), qsd.value("name")) {
      (Some(from), Some(name)) => Some(formataddr(Some(&unquote(name)), &unquote(from))),
      (Some(from), None) => Some(unquote(from)),
      (None, Some(name)) => Some(unquote(name)),
      (None, None) => None,
    };
    let mut from_name = SenderName::AppId;
    let mut from_email = format!("{}@{}", user, domain);
    if let Some(from) = from_arg {
      match is_email(&from) {
        Some(e) => {
          from_name = e.display_name().map(SenderName::Named).unwrap_or(SenderName::Bare);
          from_email = e.full_email;
        }
        None => from_name = SenderName::Named(from),
      }
    }
    if is_email(&from_email).is_none() {
      tracing::warn!("Invalid ~From~ email format: {}", from_email);
      return None;
    }

    let mut raw_targets: Vec<String> = path.cloned().collect();
    if let Some(to) = qsd.value("to") {
      raw_targets.push(to.to_string());
    }
    let targets = if raw_targets.is_empty() {
      vec![(None, from_email.clone())]
    } else {
      valid_addresses(&parse_emails_all(&raw_targets), "To").into_iter().map(|e| (e.display_name(), e.full_email)).collect()
    };

    let mut names = HashMap::new();
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    for (list, raw, kind) in [(&mut cc, qsd.value("cc"), "Carbon Copy"), (&mut bcc, qsd.value("bcc"), "Blind Carbon Copy")] {
      for e in valid_addresses(&parse_emails(raw.unwrap_or_default()), kind) {
        names.insert(e.full_email.clone(), e.display_name());
        if !list.contains(&e.full_email) {
          list.push(e.full_email);
        }
      }
    }

    let unquote_pair = |(k, v): (String, String)| (unquote(&k), unquote(&v));
    Some(Self {
      apikey,
      domain,
      from_name,
      from_email,
      targets,
      cc,
      bcc,
      names,
      headers: qsd.prefixed('+').into_iter().map(unquote_pair).collect(),
      tokens: qsd.prefixed(':').into_iter().map(unquote_pair).collect(),
      region,
      batch: qsd.get("batch").map(parse_bool).unwrap_or(false),
      throttle: THROTTLE,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Mailgun",
      service_url: Some("https://www.mailgun.com/"),
      setup_url: Some("https://appriseit.com/services/mailgun/"),
      protocols: vec!["mailgun"],
      description: "Send email via Mailgun.",
      attachment_support: true,
    }
  }

  fn api_base(&self) -> String {
    service_base(if self.region == "eu" { "https://api.eu.mailgun.net" } else { "https://api.mailgun.net" })
  }
}

#[async_trait]
impl Notify for Mailgun {
  fn schemas(&self) -> &[&str] {
    &["mailgun"]
  }
  fn service_name(&self) -> &str {
    "Mailgun"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.2
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Email recipients to notify");
      return Ok(false);
    }

    let from_name = match &self.from_name {
      SenderName::AppId => Some(ctx.asset.app_id.as_str()),
      SenderName::Named(name) => Some(name.as_str()),
      SenderName::Bare => None,
    };
    let url = format!("{}/v3/{}/messages", self.api_base(), self.domain);
    let client = build_client(self.verify_certificate)?;
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, batch) in self.targets.chunks(batch_size).enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let in_batch = |addr: &String| batch.iter().any(|(_, t)| t == addr);
      let cc: Vec<&String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && !in_batch(a)).collect();
      let bcc: Vec<&String> = self.bcc.iter().filter(|a| !in_batch(a)).collect();

      let mut fields: Vec<(String, String)> = vec![
        ("o:skip-verification".into(), if self.verify_certificate { "False" } else { "True" }.into()),
        ("from".into(), formataddr(from_name, &self.from_email)),
        ("subject".into(), ctx.title.clone()),
        (if ctx.body_format == NotifyFormat::Html { "html" } else { "text" }.into(), ctx.body.clone()),
        ("to".into(), batch.iter().map(|(name, addr)| formataddr(name.as_deref(), addr)).collect::<Vec<_>>().join(",")),
      ];
      if !cc.is_empty() {
        let cc = cc.iter().map(|a| formataddr(self.names.get(*a).and_then(|n| n.as_deref()), a)).collect::<Vec<_>>().join(",");
        fields.push(("cc".into(), cc));
      }
      if !bcc.is_empty() {
        fields.push(("bcc".into(), bcc.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(",")));
      }
      fields.extend(self.tokens.iter().map(|(k, v)| (format!("v:{}", k), v.clone())));
      fields.extend(self.headers.iter().map(|(k, v)| (format!("h:{}", k), v.clone())));

      let request = client.post(&url).header("User-Agent", APP_ID).header("Accept", "application/json").basic_auth("api", Some(&self.apikey));
      let request = if ctx.attachments.is_empty() {
        request.form(&fields)
      } else {
        let mut form = reqwest::multipart::Form::new();
        for (k, v) in fields {
          form = form.text(k, v);
        }
        for (idx, att) in ctx.attachments.iter().enumerate() {
          let name = if att.name.is_empty() { format!("file{:03}.dat", idx + 1) } else { att.name.clone() };
          form = form.part(format!("attachment[{}]", idx), reqwest::multipart::Part::bytes(att.data.clone()).file_name(name));
        }
        request.multipart(form)
      };

      let dest = if batch.len() <= 3 { batch.iter().map(|(_, a)| a.as_str()).collect::<Vec<_>>().join(", ") } else { format!("{} recipients", batch.len()) };
      match request.send().await {
        Ok(resp) if resp.status().as_u16() == 200 => tracing::info!("Sent Mailgun notification to {}.", dest),
        Ok(resp) => {
          let status = resp.status().as_u16();
          tracing::warn!("Failed to send Mailgun notification to {}: {}error={}.", dest, status_message(status), status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Mailgun:{} notification.", dest);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

/// Upstream `MAILGUN_HTTP_ERROR_MAP`, formatted for the failure log line.
fn status_message(status: u16) -> &'static str {
  match status {
    400 => "A bad request was made to the server., ",
    401 => "The provided API Key was not valid., ",
    402 => "The request failed for a reason out of your control., ",
    404 => "The requested API query is not valid., ",
    413 => "Provided attachment is to big., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn token() -> String {
    format!("{}-{}-{}", "a".repeat(32), "b".repeat(8), "c".repeat(8))
  }

  fn parse(url: &str) -> Mailgun {
    Mailgun::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url))
  }

  fn form(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  fn ctx(body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: "title".into(), body: "body".into(), body_format, ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let t = token();
    for url in [
      "mailgun://".to_string(),
      "mailgun://:@/".to_string(),
      // No API key
      "mailgun://user@localhost.localdomain".to_string(),
      // API key but no user
      format!("mailgun://localhost.localdomain/{}", t),
      // Invalid from address
      format!("mailgun://\"@localhost.localdomain/{}", t),
      // Invalid region
      format!("mailgun://user@localhost.localdomain/{}?region=invalid", t),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let t = token();
    for url in [
      format!("mailgun://user@localhost.localdomain/{}", t),
      format!("mailgun://user@localhost.localdomain/{}?format=markdown", t),
      format!("mailgun://user@localhost.localdomain/{}?format=html", t),
      format!("mailgun://user@localhost.localdomain/{}?format=text", t),
      format!("mailgun://user@localhost.localdomain/{}?region=uS", t),
      format!("mailgun://user@localhost.localdomain/{}?region=EU", t),
      format!("mailgun://user@localhost.localdomain/{}?from=jack@gmail.com&name=Jason<jason@gmail.com>", t),
      format!("mailgun://user@localhost.localdomain/{}?+X-Customer-Campaign-ID=Apprise", t),
      format!("mailgun://user@localhost.localdomain/{}?:name=Chris&:status=admin", t),
      format!("mailgun://user@localhost.localdomain/{}?:from=Chris&:status=admin", t),
      format!("mailgun://user@localhost.localdomain/{}?bcc=user@example.com&cc=user2@example.com", t),
      format!("mailgun://user@localhost.localdomain/{}/test@example.com", t),
      format!("mailgun://user@localhost.localdomain/{}?to=test@example.com", t),
      format!("mailgun://user@localhost.localdomain/{}/test@example.com?name=\"Frodo\"", t),
      // Loads, but has no valid recipient
      format!("mailgun://user@localhost.localdomain/{}/invalid", t),
      format!(
        "mailgun://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com",
        t
      ),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let mg = parse(&format!(
      "mailgun://user@example.com/{}/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com&region=EU",
      token()
    ));
    assert_eq!(mg.domain, "example.com");
    assert_eq!(mg.region, "eu");
    assert_eq!(mg.from_email, "user@example.com");
    assert_eq!(mg.from_name, SenderName::AppId);
    assert_eq!(mg.targets, vec![(None, "user1@example.com".to_string()), (Some("User2".to_string()), "user2@example.com".to_string())]);
    assert_eq!(mg.cc, vec!["user4@example.com", "user5@example.com"]);
    assert_eq!(mg.bcc, vec!["user3@example.com", "user1@example.com"]);
    assert_eq!(mg.names.get("user5@example.com"), Some(&Some("Da".to_string())));
    assert!(!mg.batch);
  }

  #[test]
  fn test_from_and_name_handling() {
    // Upstream test_plugin_mailgun_header_check / url_from_round_trip
    let mg = parse("mailgun://user@localhost.localdomain/abc123?from=Luke%20Skywalker");
    assert_eq!((mg.from_name.clone(), mg.from_email.as_str()), (SenderName::Named("Luke Skywalker".into()), "user@localhost.localdomain"));
    assert_eq!(mg.targets, vec![(None, "user@localhost.localdomain".to_string())]);

    let mg = parse("mailgun://user@localhost.localdomain/abc123?from=Luke%20Skywalker<luke@rebels.com>");
    assert_eq!((mg.from_name.clone(), mg.from_email.as_str()), (SenderName::Named("Luke Skywalker".into()), "luke@rebels.com"));
    assert_eq!(mg.targets, vec![(None, "luke@rebels.com".to_string())]);

    let mg = parse("mailgun://user@localhost.localdomain/abc123?from=luke@rebels.com");
    assert_eq!((mg.from_name.clone(), mg.from_email.as_str()), (SenderName::Bare, "luke@rebels.com"));

    // name= names the from= address; a quoted name keeps its quotes out
    let mg = parse("mailgun://user@localhost.localdomain/abc123?from=jack@gmail.com&name=Jason<jason@gmail.com>");
    assert_eq!((mg.from_name.clone(), mg.from_email.as_str()), (SenderName::Named("Jason".into()), "jason@gmail.com"));

    // name= keeps a literal '+' (upstream does not decode it as a space)
    let mg = parse("mailgun://user@localhost.localdomain/abc123?name=John+Doe");
    assert_eq!(mg.from_name, SenderName::Named("John+Doe".into()));
  }

  #[test]
  fn test_headers_and_tokens_keep_order() {
    let mg = parse("mailgun://user@example.com/apikey/target@example.com?:zulu=z&:alpha=a&:mike=m&+X-B=2&+X-A=1");
    assert_eq!(mg.tokens, vec![("zulu".into(), "z".into()), ("alpha".into(), "a".into()), ("mike".into(), "m".into())]);
    assert_eq!(mg.headers, vec![("X-B".into(), "2".into()), ("X-A".into(), "1".into())]);
  }

  #[test]
  fn test_static_details() {
    let details = Mailgun::static_details();
    assert_eq!(details.service_name, "Mailgun");
    assert!(details.protocols.contains(&"mailgun"));
    assert!(details.attachment_support);
  }

  #[tokio::test]
  async fn test_send_default_payload() {
    // Matches upstream (apprise-go golden "default")
    let server = mock(200).await;
    let svc = from_url("mailgun://sender@domain.com/apikey/recipient@domain.com").unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Html);
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Html))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/v3/domain.com/messages");
    assert_eq!(req.headers.get("authorization").unwrap(), "Basic YXBpOmFwaWtleQ==");
    assert_eq!(req.headers.get("accept").unwrap(), "application/json");
    assert_eq!(req.headers.get("content-type").unwrap(), "application/x-www-form-urlencoded");
    assert_eq!(
      form(&req.body),
      vec![
        ("o:skip-verification".to_string(), "False".to_string()),
        ("from".to_string(), "Apprise <sender@domain.com>".to_string()),
        ("subject".to_string(), "title".to_string()),
        ("html".to_string(), "body".to_string()),
        ("to".to_string(), "recipient@domain.com".to_string()),
      ]
    );
  }

  #[tokio::test]
  async fn test_send_text_format_tokens_headers_and_copies() {
    let server = mock(200).await;
    let svc = from_url(
      "mailgun://user@example.com/apikey/user1@example.com/User2:user2@example.com?cc=user4@example.com,Da:user5@example.com,user1@example.com&bcc=user3@example.com,user4@example.com&:name=Chris&+X-Customer=Apprise&verify=no&name=John+Doe",
    )
    .unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Text))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let first = form(&reqs[0].body);
    assert_eq!(
      first,
      vec![
        ("o:skip-verification".to_string(), "True".to_string()),
        ("from".to_string(), "John+Doe <user@example.com>".to_string()),
        ("subject".to_string(), "title".to_string()),
        ("text".to_string(), "body".to_string()),
        ("to".to_string(), "user1@example.com".to_string()),
        // user4 is a bcc and user1 the recipient, so neither is a cc
        ("cc".to_string(), "Da <user5@example.com>".to_string()),
        ("bcc".to_string(), "user3@example.com,user4@example.com".to_string()),
        ("v:name".to_string(), "Chris".to_string()),
        ("h:X-Customer".to_string(), "Apprise".to_string()),
      ]
    );
    let second = form(&reqs[1].body);
    assert!(second.contains(&("to".to_string(), "User2 <user2@example.com>".to_string())));
    assert!(second.contains(&("cc".to_string(), "Da <user5@example.com>,user1@example.com".to_string())));
  }

  #[tokio::test]
  async fn test_send_eu_region_and_batch() {
    let server = mock(200).await;
    let svc = from_url("mailgun://no-reply@example.com/apikey/user1@example.com/user2@example.com?batch=yes&region=eu").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Html))).await.unwrap());
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(form(&reqs[0].body).contains(&("to".to_string(), "user1@example.com,user2@example.com".to_string())));
    assert_eq!(parse("mailgun://user@example.com/apikey?region=eu").api_base(), "https://api.eu.mailgun.net");
    assert_eq!(parse("mailgun://user@example.com/apikey").api_base(), "https://api.mailgun.net");
  }

  #[tokio::test]
  async fn test_send_attachments_multipart() {
    let server = mock(200).await;
    let svc = from_url("mailgun://sender@example.com/apikey/user@example.com?:zulu=z&:alpha=a").unwrap();
    let mut ctx = ctx(NotifyFormat::Html);
    ctx.attachments = vec![
      Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() },
      Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "application/octet-stream".into() },
    ];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let ct = reqs[0].headers.get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&reqs[0].body);
    let order: Vec<usize> = ["\"o:skip-verification\"", "\"from\"", "\"subject\"", "\"html\"", "\"to\"", "\"v:zulu\"", "\"v:alpha\"", "\"attachment[0]\""]
      .iter()
      .map(|n| body.find(&format!("name={}", n)).unwrap_or_else(|| panic!("missing field {}", n)))
      .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "fields out of order: {}", body);
    assert!(body.contains("name=\"attachment[0]\"; filename=\"report.pdf\"\r\n\r\nPDFDATA\r\n"));
    assert!(body.contains("name=\"attachment[1]\"; filename=\"file002.dat\""));
    assert!(body.contains("Apprise <sender@example.com>"));
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 401] {
      let server = mock(status).await;
      let svc = from_url(&format!("mailgun://user@localhost.localdomain/{}", token())).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Html))).await.unwrap(), "status {}", status);
    }

    // No valid recipient: nothing is sent
    let server = mock(200).await;
    let svc = from_url(&format!("mailgun://user@localhost.localdomain/{}/invalid", token())).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Html))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());

    // One failed recipient fails the notification but the others still go out
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/example.com/messages")).respond_with(ResponseTemplate::new(400)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let svc = from_url("mailgun://user@example.com/apikey/a@example.com/b@example.com").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx(NotifyFormat::Html))).await.unwrap());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
  }
}

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyFormat;
use crate::utils::aws::{SigV4Request, sign};
use crate::utils::email::{formataddr, header_encode, is_email, is_space, parse_emails, parse_emails_all, valid_addresses};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd::{Qsd, unquote, validate_token};
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::time::Duration;

/// Pause between requests (upstream `request_rate_per_sec`).
const THROTTLE: Duration = Duration::from_millis(2500);

/// Content type of the signed request body.
const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

pub struct Ses {
  access_key: String,
  secret_key: String,
  region: String,
  session_token: Option<String>,
  /// Sender address as given (upstream keeps it verbatim).
  from_email: String,
  from_name: Option<String>,
  /// `(display name, address)`
  reply_to: Option<(Option<String>, String)>,
  /// `(display name, address)`
  targets: Vec<(Option<String>, String)>,
  cc: Vec<String>,
  bcc: Vec<String>,
  names: HashMap<String, Option<String>>,
  throttle: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `IS_REGION`: `us-east-1`, `us-gov-west-1`, ... normalised to
/// lower case.
fn parse_region(entry: &str) -> Option<String> {
  let entry = entry.trim_matches(is_space);
  let (rest, no) = entry.rsplit_once('-')?;
  let (country, area) = rest.split_once('-')?;
  let valid = country.len() == 2
    && country.chars().all(|c| c.is_ascii_alphabetic())
    && !area.is_empty()
    && area.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
    && !no.is_empty()
    && no.chars().all(|c| c.is_ascii_digit());
  valid.then(|| format!("{}-{}-{}", country.to_lowercase(), area.to_lowercase(), no))
}

/// Upstream's `region` token check (`^[a-z]{2}-[a-z-]+?-[0-9]+$`); unlike
/// [`parse_region`] it allows no surrounding space and keeps the case.
fn validate_region(value: &str) -> Option<String> {
  (value.trim_matches(is_space) == value && parse_region(value).is_some()).then(|| value.to_string())
}

impl Ses {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let qsd = Qsd::from_url(url);

    // ses://{from}/{access_key}/{secret_key}/{region}/{targets}: the secret
    // may contain slashes, so it runs until the region (or a first email)
    let mut entries = url.path_parts.clone();
    let path_access_key = if entries.is_empty() { None } else { Some(entries.remove(0)) };
    let mut secret_parts = Vec::new();
    let mut path_region = None;
    let mut index = entries.len();
    for (i, entry) in entries.iter().enumerate() {
      if let Some(region) = parse_region(entry) {
        path_region = Some(region);
        index = i + 1;
        break;
      }
      if is_email(entry).is_some() {
        index = i;
        break;
      }
      secret_parts.push(entry.clone());
    }
    let mut raw_targets: Vec<String> = entries[index..].to_vec();
    if let Some(to) = qsd.value("to") {
      raw_targets.push(to.to_string());
    }

    let access_key = qsd.value("key").or_else(|| qsd.value("access")).map(unquote).or(path_access_key);
    let access_key = access_key.and_then(|k| validate_token(&k));
    let Some(access_key) = access_key else {
      tracing::warn!("An invalid AWS Access Key ID was specified.");
      return None;
    };
    let secret_key = qsd.value("secret").map(unquote).or_else(|| (!secret_parts.is_empty()).then(|| secret_parts.join("/")));
    let Some(secret_key) = secret_key.and_then(|s| validate_token(&s)) else {
      tracing::warn!("An invalid AWS Secret Access Key was specified.");
      return None;
    };
    let region = match qsd.value("region") {
      Some(r) => validate_region(&unquote(r)),
      None => path_region,
    };
    let Some(region) = region else {
      tracing::warn!("An invalid AWS Region was specified.");
      return None;
    };

    // ses://user:{token}@host/... or ?token=; the query wins
    let session_token = qsd.value("token").map(unquote).or_else(|| url.password.clone()).filter(|t| !t.is_empty());

    let user = url.user.clone().filter(|u| !u.is_empty());
    let host = url.host.clone().unwrap_or_default();
    let from_email = match qsd.value("from") {
      Some(from) => Some(unquote(from)),
      None => user.as_ref().map(|u| format!("{}@{}", u, host)),
    };
    let Some(from_email) = from_email.filter(|f| is_email(f).is_some()) else {
      tracing::warn!("An invalid AWS From ({}@{}) was specified.", user.unwrap_or_default(), host);
      return None;
    };
    let from_name = qsd.value("name").map(unquote);

    let reply_to = match qsd.value("reply").map(unquote) {
      Some(reply) => match is_email(&reply) {
        Some(e) => Some((e.display_name(), e.full_email)),
        None => {
          tracing::warn!("An invalid AWS Reply To ({}) was specified.", reply);
          return None;
        }
      },
      None => None,
    };

    let targets = if raw_targets.is_empty() {
      vec![(from_name.clone(), from_email.clone())]
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

    Some(Self {
      access_key,
      secret_key,
      region,
      session_token,
      from_email,
      from_name,
      reply_to,
      targets,
      cc,
      bcc,
      names,
      throttle: THROTTLE,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "AWS Simple Email Service (SES)",
      service_url: Some("https://aws.amazon.com/ses/"),
      setup_url: Some("https://appriseit.com/services/ses/"),
      protocols: vec!["ses"],
      description: "Send email via AWS SES.",
      attachment_support: true,
    }
  }

  fn named(&self, addr: &str) -> String {
    formataddr(self.names.get(addr).and_then(|n| n.as_deref()), addr)
  }
}

#[async_trait]
impl Notify for Ses {
  fn schemas(&self) -> &[&str] {
    &["ses"]
  }
  fn service_name(&self) -> &str {
    "AWS Simple Email Service (SES)"
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
    2.5
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no SES email recipients to notify");
      return Ok(false);
    }

    // The sender name falls back to the reply-to name, then the app description
    let from_name = self
      .from_name
      .clone()
      .filter(|n| !n.is_empty())
      .or_else(|| self.reply_to.as_ref().and_then(|(n, _)| n.clone()))
      .unwrap_or_else(|| ctx.asset.app_desc.clone());
    let reply_addr = self.reply_to.as_ref().map(|(_, a)| a.as_str()).unwrap_or(&self.from_email);

    let endpoint = format!("{}/", crate::notify::service_base(&format!("https://email.{}.amazonaws.com", self.region)));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    for (index, (to_name, to_addr)) in self.targets.iter().enumerate() {
      if index > 0 {
        tokio::time::sleep(self.throttle).await;
      }
      let cc: Vec<String> = self.cc.iter().filter(|a| !self.bcc.contains(a) && *a != to_addr).map(|a| self.named(a)).collect();
      let bcc: Vec<String> = self.bcc.iter().filter(|a| *a != to_addr).map(|a| self.named(a)).collect();

      let mut headers = vec![
        ("Subject", encode_subject(&ctx.title)),
        ("From", formataddr(Some(&from_name), &self.from_email)),
        ("To", formataddr(to_name.as_deref(), to_addr)),
      ];
      if reply_addr != self.from_email {
        headers.push(("Reply-To", formataddr(Some(&from_name), reply_addr)));
      }
      headers.push(("Cc", cc.join(",")));
      headers.push(("Date", chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000").to_string()));
      headers.push(("X-Application", ctx.asset.app_id.clone()));
      let html = ctx.body_format == NotifyFormat::Html;
      let raw = build_mime(&headers, &ctx.body, html, &ctx.attachments, &make_boundary());

      let mut payload: Vec<(String, String)> = vec![
        ("Action".into(), "SendRawEmail".into()),
        ("Version".into(), "2010-12-01".into()),
        ("RawMessage.Data".into(), base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())),
      ];
      for (no, dest) in std::iter::once(to_addr.clone()).chain(bcc).chain(cc).enumerate() {
        payload.push((format!("Destinations.member.{}", no + 1), dest));
      }
      payload.push(("Source".into(), format!("{} <{}>", quote(&from_name, " "), quote(&self.from_email, "@ "))));
      let body = payload.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&");

      let host = format!("email.{}.amazonaws.com", self.region);
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
          service: "ses",
        },
        chrono::Utc::now(),
      );
      let mut request = client
        .post(&endpoint)
        .header("User-Agent", APP_ID)
        .header("Content-Type", CONTENT_TYPE)
        .header("X-Amz-Date", signed.amz_date)
        .header("Authorization", signed.authorization);
      if let Some(token) = &self.session_token {
        request = request.header("X-Amz-Security-Token", token);
      }

      match request.body(body).send().await {
        Ok(resp) if resp.status().as_u16() == 200 => tracing::info!("Sent AWS SES notification to \"{}\".", to_addr),
        Ok(resp) => {
          let status = resp.status().as_u16();
          let detail = if status == 403 { "Unauthorized - Invalid Access/Secret Key Combination., " } else { "" };
          tracing::warn!("Failed to send AWS SES notification to {}: {}error={}.", to_addr, detail, status);
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending AWS SES notification to \"{}\".", to_addr);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

// ── MIME message, as Python's email package writes it ────────────────────────

/// A boundary like `email.generator._make_boundary()`.
fn make_boundary() -> String {
  format!("==============={:019}==", rand::random::<u64>() >> 1)
}

/// Python's `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = s.char_indices().peekable();
  while let Some((i, c)) = chars.next() {
    if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
      lines.push(&s[start..i]);
      let mut end = i + c.len_utf8();
      if c == '\r' && chars.peek().map(|(_, n)| *n) == Some('\n') {
        chars.next();
        end += 1;
      }
      start = end;
    }
  }
  if start < s.len() {
    lines.push(&s[start..]);
  }
  lines
}

/// `Header(title, "utf-8").encode()` without folding: one encoded word per
/// line of the title, continuation lines indented (an empty first line is
/// dropped, as Python does).
fn encode_subject(title: &str) -> String {
  let lines = py_splitlines(title);
  let Some((first, rest)) = lines.split_first() else { return String::new() };
  let mut out: Vec<String> = Vec::new();
  if !first.is_empty() {
    out.push(header_encode(first));
  }
  out.extend(rest.iter().map(|line| format!(" {}", header_encode(&format!(" {}", line.trim_start_matches(is_space))))));
  out.join("\n")
}

/// A header value as the compat32 policy writes it: ASCII as is, anything
/// else as one encoded word.
fn header_value(value: &str) -> String {
  if value.is_ascii() { value.to_string() } else { header_encode(value) }
}

/// `email.quoprimime.body_encode()` over the UTF-8 bytes of `body`.
fn qp_body_encode(body: &str) -> String {
  if body.is_empty() {
    return String::new();
  }
  let mut encoded = String::with_capacity(body.len());
  for b in body.bytes() {
    match b {
      b'\r' | b'\n' | b'\t' | b' '..=b'<' | b'>'..=b'~' => encoded.push(b as char),
      _ => encoded.push_str(&format!("={:02X}", b)),
    }
  }

  const MAXLINELEN: isize = 76;
  let mut out: Vec<String> = Vec::new();
  for line in py_splitlines(&encoded) {
    let bytes = line.as_bytes();
    let len = bytes.len() as isize;
    let mut start: isize = 0;
    let laststart = len - 1 - MAXLINELEN;
    while start <= laststart {
      let stop = start + MAXLINELEN - 1;
      if bytes[(stop - 2) as usize] == b'=' {
        out.push(line[start as usize..(stop - 1) as usize].to_string());
        start = stop - 2;
      } else if bytes[(stop - 1) as usize] == b'=' {
        out.push(line[start as usize..stop as usize].to_string());
        start = stop - 1;
      } else {
        out.push(format!("{}=", &line[start as usize..stop as usize]));
        start = stop;
      }
    }
    match bytes.last() {
      Some(&last) if last == b' ' || last == b'\t' => {
        let room = start - laststart;
        let quoted = format!("={:02X}", last);
        let tail = if room >= 3 {
          quoted
        } else if room == 2 {
          format!("{}=\n", last as char)
        } else {
          format!("=\n{}", quoted)
        };
        out.push(format!("{}{}", &line[start as usize..(len - 1) as usize], tail));
      }
      _ => out.push(line[start as usize..].to_string()),
    }
  }
  if encoded.ends_with(['\r', '\n']) {
    out.push(String::new());
  }
  out.join("\n")
}

/// `base64.encodebytes()`: 76-character lines, each ending in a newline.
fn base64_lines(data: &[u8]) -> String {
  let encoded = base64::engine::general_purpose::STANDARD.encode(data);
  encoded.as_bytes().chunks(76).map(|c| format!("{}\n", String::from_utf8_lossy(c))).collect()
}

/// The `MIMEText` (or `MIMEMultipart` with attachments) message upstream
/// sends through `SendRawEmail`, as `Message.as_string()` renders it.
fn build_mime(headers: &[(&str, String)], body: &str, html: bool, attachments: &[Attachment], boundary: &str) -> String {
  let text_part =
    format!("Content-Type: text/{}; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\n", if html { "html" } else { "plain" });
  let envelope: String = headers.iter().map(|(k, v)| format!("{}: {}\n", k, if *k == "Subject" { v.clone() } else { header_value(v) })).collect();

  if attachments.is_empty() {
    return format!("{}{}\n{}", text_part, envelope, qp_body_encode(body));
  }

  let mut parts = vec![format!("{}\n{}", text_part, qp_body_encode(body))];
  for (no, att) in attachments.iter().enumerate() {
    let name = if att.name.is_empty() { format!("file{:03}.dat", no + 1) } else { att.name.clone() };
    parts.push(format!(
      "Content-Transfer-Encoding: base64\nMIME-Version: 1.0\nContent-Type: {}\nContent-Disposition: {}\n\n{}",
      att.mime_type,
      header_value(&format!("attachment; filename=\"{}\"", name)),
      base64_lines(&att.data)
    ));
  }
  let mut out = format!("Content-Type: multipart/mixed; boundary=\"{}\"\nMIME-Version: 1.0\n{}\n", boundary, envelope);
  out.push_str(&format!("--{}\n", boundary));
  out.push_str(&parts.join(&format!("\n--{}\n", boundary)));
  out.push_str(&format!("\n--{}--\n", boundary));
  out
}

/// Python's `urllib.parse.quote(value, safe)`.
fn quote(value: &str, safe: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for b in value.bytes() {
    if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || safe.as_bytes().contains(&b) {
      out.push(b as char);
    } else {
      out.push_str(&format!("%{:02X}", b));
    }
  }
  out
}

/// Python's `urllib.parse.quote_plus(value)`, as `urlencode()` applies it.
fn quote_plus(value: &str) -> String {
  quote(value, " ").replace(' ', "+")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::method;
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TEST_ACCESS_KEY_ID: &str = "AHIAJGNT76XIMXDBIJYA";
  const TEST_ACCESS_KEY_SECRET: &str = "bu1dHSdO22pfaaVy/wmNsdljF4C07D3bndi9PQJ9";
  const TEST_REGION: &str = "us-east-2";

  fn parse(url: &str) -> Ses {
    let mut ses = Ses::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should parse: {}", url));
    ses.throttle = Duration::ZERO;
    ses
  }

  fn form(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
  }

  fn raw_message(fields: &[(String, String)]) -> String {
    let data = &fields.iter().find(|(k, _)| k == "RawMessage.Data").unwrap().1;
    String::from_utf8(base64::engine::general_purpose::STANDARD.decode(data).unwrap()).unwrap()
  }

  /// Strip the `Date:` header and the random boundary so messages compare.
  fn normalise(raw: &str) -> String {
    let mut out: Vec<String> = raw.lines().filter(|l| !l.starts_with("Date: ")).map(str::to_string).collect();
    if let Some(start) = raw.find("boundary=\"") {
      let b = &raw[start + 10..];
      let boundary = &b[..b.find('"').unwrap()];
      out = out.into_iter().map(|l| l.replace(boundary, "BOUNDARY")).collect();
    }
    out.join("\n")
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format: NotifyFormat::Html, ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "ses://",
      "ses://:@/",
      // Missing secret and region
      "ses://user@example.com/T1JJ3T3L2",
      // Missing region
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/",
      // No from address
      "ses://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2",
      // Email reached before a region
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/user2@example.com",
      // Invalid reply-to
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2?reply=invalid-email",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2",
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/user2@example.ca/user3@example.eu",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlaevi7FQ/us-east-1?to=user2@example.ca",
      "ses://?from=user@example.com&region=us-west-2&access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlaevi7FQ&reply=No One <noreply@yahoo.ca>&bcc=user.bcc@example.com,user2.bcc@example.com,invalid-email&cc=user.cc@example.com,user2.cc@example.com,invalid-email&to=user2@example.ca",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?name=From%20Name&to=user2@example.ca,invalid-email",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?format=text",
      // Loads, but has no valid recipient
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?to=invalid-email",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/user2@example.com?token=SESSIONTOKEN",
      "ses://user@example.com/?key=T1JJ3T3L2&secret=A1BRTD4JD/TIiacevi7FQ&region=us-west-2&to=user2@example.com",
      "ses://user:SESSIONTOKEN@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/user2@example.com",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_url_parsing() {
    // Upstream test_plugin_ses_url_parsing
    let ses = parse(&format!("ses://user@example.com/{}/{}/{}/", TEST_ACCESS_KEY_ID, TEST_ACCESS_KEY_SECRET, TEST_REGION));
    assert_eq!(ses.access_key, TEST_ACCESS_KEY_ID);
    assert_eq!(ses.secret_key, TEST_ACCESS_KEY_SECRET);
    assert_eq!(ses.region, TEST_REGION);
    assert_eq!(ses.targets, vec![(None, "user@example.com".to_string())]);

    let ses = parse(&format!(
      "ses://user@example.com/{}/{}/{}/user1@example.ca/user2@example.eu",
      TEST_ACCESS_KEY_ID,
      TEST_ACCESS_KEY_SECRET,
      TEST_REGION.to_uppercase()
    ));
    assert_eq!(ses.region, TEST_REGION);
    assert_eq!(ses.targets, vec![(None, "user1@example.ca".to_string()), (None, "user2@example.eu".to_string())]);

    // us-gov-west-1 style regions
    assert_eq!(parse("ses://user@example.com/key/sec/ret/us-gov-west-1").region, "us-gov-west-1");
    assert_eq!(parse("ses://user@example.com/key/sec/ret/us-gov-west-1").secret_key, "sec/ret");
  }

  #[test]
  fn test_query_overrides_and_session_token() {
    let ses = parse("ses://user@example.com/?key=T1JJ3T3L2&secret=A1BRTD4JD/TIiacevi7FQ&region=us-west-2&to=user2@example.com");
    assert_eq!((ses.access_key.as_str(), ses.secret_key.as_str(), ses.region.as_str()), ("T1JJ3T3L2", "A1BRTD4JD/TIiacevi7FQ", "us-west-2"));
    assert_eq!(ses.session_token, None);

    // ?key= wins over ?access=
    assert_eq!(parse("ses://user@example.com/?key=KEY&access=OLD&secret=S&region=us-west-2").access_key, "KEY");
    assert_eq!(parse("ses://user@example.com/?access=OLD&secret=S&region=us-west-2").access_key, "OLD");

    // Session token from the password field or ?token= (which wins)
    let ses = parse("ses://user:SESSIONTOKEN@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/user2@example.com");
    assert_eq!(ses.session_token.as_deref(), Some("SESSIONTOKEN"));
    assert_eq!(ses.from_email, "user@example.com");
    let ses = parse("ses://user:FIELD@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?token=QUERY");
    assert_eq!(ses.session_token.as_deref(), Some("QUERY"));
  }

  #[test]
  fn test_reply_cc_bcc_names() {
    let ses = parse(
      "ses://?from=user@example.com&region=us-west-2&access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlaevi7FQ&reply=No One <noreply@yahoo.ca>&bcc=user.bcc@example.com,Jane%20Doe<jane@example.com>,invalid-email&cc=Chris<l2g@nuxref.com>,user2.cc@example.com,invalid-email&to=user2@example.ca",
    );
    assert_eq!(ses.reply_to, Some((Some("No One".to_string()), "noreply@yahoo.ca".to_string())));
    assert_eq!(ses.cc, vec!["l2g@nuxref.com", "user2.cc@example.com"]);
    assert_eq!(ses.bcc, vec!["user.bcc@example.com", "jane@example.com"]);
    assert_eq!(ses.names.get("l2g@nuxref.com"), Some(&Some("Chris".to_string())));
    assert_eq!(ses.names.get("jane@example.com"), Some(&Some("Jane Doe".to_string())));
    assert_eq!(ses.targets, vec![(None, "user2@example.ca".to_string())]);
  }

  #[test]
  fn test_static_details() {
    let details = Ses::static_details();
    assert_eq!(details.service_name, "AWS Simple Email Service (SES)");
    assert!(details.protocols.contains(&"ses"));
    assert!(details.attachment_support);
  }

  #[test]
  fn test_mime_matches_python() {
    // Expected messages come from Python's email package (MIMEText,
    // MIMEMultipart, Header) with Apprise's UTF-8 quoted-printable charset.
    let headers = |title: &str| {
      vec![
        ("Subject", encode_subject(title)),
        ("From", formataddr(Some("Apprise Notifications"), "sender@example.com")),
        ("To", formataddr(Some("Jörg Ä"), "to@example.com")),
        ("Reply-To", formataddr(Some("Dr. Who"), "reply@example.com")),
        ("Cc", String::new()),
        ("Date", "Mon, 01 Jan 2024 00:00:00 +0000".to_string()),
        ("X-Application", "Apprise".to_string()),
      ]
    };
    // (title, body, html, message) generated by Python
    let cases: &[(&str, &str, bool, &str)] = &[
      (
        "Test Subject",
        "hello",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?Test_Subject?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\nhello",
      ),
      (
        "",
        "",
        false,
        "Content-Type: text/plain; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: \nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\n",
      ),
      (
        "a\nb",
        "line one \nline two\t\r\nthird=\n",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?a?=\n =?utf-8?q?_b?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\nline one=20\nline two=09\nthird=3D\n",
      ),
      (
        "Jörg",
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx=yzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\néééééééééééééééééééééééééééééééééééééééé",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?J=C3=B6rg?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx=\n=3Dyzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz=\nzzzzzzzzz\n=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=\n=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=\n=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=\n=A9=C3=A9=C3=A9",
      ),
      (
        "x",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?x?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa=\n=20",
      ),
      (
        "x",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?x?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa =\n",
      ),
      (
        "x",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?x?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa=\na=20",
      ),
      (
        "x",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?x?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa=\n=C3=A9",
      ),
      (
        "\nlead\r\nwith seps",
        "\r\n\r\nend\r",
        false,
        "Content-Type: text/plain; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject:  =?utf-8?q?_lead?=\n =?utf-8?q?_with?=\n =?utf-8?q?_seps?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\n\n\nend\n",
      ),
      (
        "x",
        "tab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\t",
        true,
        "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?x?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n\ntab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab=\n\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab\ttab=09",
      ),
    ];
    for (title, body, is_html, expected) in cases {
      assert_eq!(build_mime(&headers(title), body, *is_html, &[], "BOUNDARY"), *expected, "title {:?} body {:?}", title, body);
    }

    let attachments = vec![
      Attachment { name: "rapport é.pdf".into(), data: b"PDFDATA".repeat(20), mime_type: "application/pdf".into() },
      Attachment { name: "notes.txt".into(), data: b"plain text attachment\n".to_vec(), mime_type: "text/plain".into() },
    ];
    let envelope = "Subject: =?utf-8?q?T?=\nFrom: Apprise Notifications <sender@example.com>\nTo: =?utf-8?q?J=C3=B6rg_=C3=84?= <to@example.com>\nReply-To: \"Dr. Who\" <reply@example.com>\nCc: \nDate: Mon, 01 Jan 2024 00:00:00 +0000\nX-Application: Apprise\n";
    let html = "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\n";
    let expected = format!(
      "Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\nMIME-Version: 1.0\n{}\n--BOUNDARY\n{}\nbody\n--BOUNDARY\nContent-Transfer-Encoding: base64\nMIME-Version: 1.0\nContent-Type: application/pdf\nContent-Disposition: =?utf-8?q?attachment=3B_filename=3D=22rapport_=C3=A9=2Epdf=22?=\n\nUERGREFUQVBERkRBVEFQREZEQVRBUERGREFUQVBERkRBVEFQREZEQVRBUERGREFUQVBERkRBVEFQ\nREZEQVRBUERGREFUQVBERkRBVEFQREZEQVRBUERGREFUQVBERkRBVEFQREZEQVRBUERGREFUQVBE\nRkRBVEFQREZEQVRBUERGREFUQVBERkRBVEE=\n\n--BOUNDARY\nContent-Transfer-Encoding: base64\nMIME-Version: 1.0\nContent-Type: text/plain\nContent-Disposition: attachment; filename=\"notes.txt\"\n\ncGxhaW4gdGV4dCBhdHRhY2htZW50Cg==\n\n--BOUNDARY--\n",
      envelope, html
    );
    assert_eq!(build_mime(&headers("T"), "body", true, &attachments, "BOUNDARY"), expected);
  }

  #[test]
  fn test_url_quoting_matches_python() {
    assert_eq!(quote("Reply+Guy", " "), "Reply%2BGuy");
    assert_eq!(quote("Apprise Notifications", " "), "Apprise Notifications");
    assert_eq!(quote("a/b~c", " "), "a%2Fb~c");
    assert_eq!(quote_plus("Apprise Notifications <a@b.com>"), "Apprise+Notifications+%3Ca%40b.com%3E");
    assert_eq!(quote_plus("x=y&z/é"), "x%3Dy%26z%2F%C3%A9");
  }

  #[tokio::test]
  async fn test_send_basic_request() {
    // Matches upstream (apprise-go golden "basic") apart from Date/signature
    let server = mock(200).await;
    let svc = parse("ses://sender@example.com/access123/secret123/us-east-1/target@example.com");
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test Subject", "hello"))).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/");
    assert_eq!(req.headers.get("content-type").unwrap(), CONTENT_TYPE);
    let amz_date = req.headers.get("x-amz-date").unwrap().to_str().unwrap().to_string();
    let auth = req.headers.get("authorization").unwrap().to_str().unwrap().to_string();
    assert!(
      auth.starts_with(&format!(
        "AWS4-HMAC-SHA256 Credential=access123/{}/us-east-1/ses/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=",
        &amz_date[..8]
      )),
      "{}",
      auth
    );
    assert!(req.headers.get("x-amz-security-token").is_none());

    let fields = form(&req.body);
    let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["Action", "Version", "RawMessage.Data", "Destinations.member.1", "Source"]);
    assert_eq!(fields[0].1, "SendRawEmail");
    assert_eq!(fields[1].1, "2010-12-01");
    assert_eq!(fields[3].1, "target@example.com");
    assert_eq!(fields[4].1, "Apprise Notifications <sender@example.com>");
    assert_eq!(
      normalise(&raw_message(&fields)),
      "Content-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\nSubject: =?utf-8?q?Test_Subject?=\nFrom: Apprise Notifications <sender@example.com>\nTo: target@example.com\nCc: \nX-Application: Apprise\n\nhello"
    );
  }

  #[tokio::test]
  async fn test_send_names_reply_copies_and_attachment() {
    // Upstream sends one message per recipient; Destinations are the
    // recipient, then the named bcc and cc entries
    let server = mock(200).await;
    let svc = parse(
      "ses://sender@example.com/access123/secret123/us-east-1/target@example.com/Jo:jo@example.com?cc=Chris<cc@example.com>,cc2@example.com&bcc=bcc@example.com&reply=Reply+Guy<reply@example.com>",
    );
    let mut ctx = ctx("Tést, Subject", "héllo <b>world</b> = 100%");
    ctx.attachments = vec![Attachment { name: "notes.txt".into(), data: b"plain text attachment\n".to_vec(), mime_type: "text/plain".into() }];
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let fields = form(&reqs[1].body);
    let dest: Vec<&str> = fields.iter().filter(|(k, _)| k.starts_with("Destinations.member.")).map(|(_, v)| v.as_str()).collect();
    assert_eq!(dest, vec!["jo@example.com", "bcc@example.com", "Chris <cc@example.com>", "cc2@example.com"]);
    assert_eq!(fields.last().unwrap(), &("Source".to_string(), "Reply%2BGuy <sender@example.com>".to_string()));
    assert_eq!(
      normalise(&raw_message(&fields)),
      "Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\nMIME-Version: 1.0\nSubject: =?utf-8?q?T=C3=A9st=2C_Subject?=\nFrom: Reply+Guy <sender@example.com>\nTo: Jo <jo@example.com>\nReply-To: Reply+Guy <reply@example.com>\nCc: Chris <cc@example.com>,cc2@example.com\nX-Application: Apprise\n\n--BOUNDARY\nContent-Type: text/html; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: quoted-printable\n\nh=C3=A9llo <b>world</b> =3D 100%\n--BOUNDARY\nContent-Transfer-Encoding: base64\nMIME-Version: 1.0\nContent-Type: text/plain\nContent-Disposition: attachment; filename=\"notes.txt\"\n\ncGxhaW4gdGV4dCBhdHRhY2htZW50Cg==\n\n--BOUNDARY--"
    );
  }

  #[tokio::test]
  async fn test_send_text_format_and_session_token() {
    let server = mock(200).await;
    let svc = parse("ses://sender:sessiontoken123@example.com/access123/secret123/us-east-1/target@example.com?name=From%20Name");
    let mut ctx = ctx("t", "plain");
    ctx.body_format = NotifyFormat::Text;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let req = requests.iter().find(|r| r.method == wiremock::http::Method::POST).unwrap();
    assert_eq!(req.headers.get("x-amz-security-token").unwrap(), "sessiontoken123");
    // Temporary credentials only work when the token is part of the signature.
    let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
    assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token,"), "{}", auth);
    let fields = form(&req.body);
    let raw = raw_message(&fields);
    assert!(raw.starts_with("Content-Type: text/plain; charset=\"utf-8\""), "{}", raw);
    assert!(raw.contains("\nFrom: From Name <sender@example.com>\n"));
    assert!(!raw.contains("Reply-To"));
    assert_eq!(fields.last().unwrap().1, "From Name <sender@example.com>");
  }

  #[tokio::test]
  async fn test_send_failures() {
    let server = mock(403).await;
    let svc = parse("ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/user2@example.com");
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());

    let server = mock(999).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());

    // No valid recipient: nothing is sent
    let server = mock(200).await;
    let svc = parse("ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?to=invalid-email");
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
  }
}

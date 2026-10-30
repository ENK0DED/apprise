use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list};
use async_trait::async_trait;
use serde_json::json;

/// Call signs per request in batch mode.
const DEFAULT_BATCH_SIZE: usize = 50;

/// Priority prefixes; the first one the value starts with wins.
const PRIORITY_MAP: &[(&str, bool)] = &[("n", false), ("e", true), ("0", false), ("1", true)];

pub struct Dapnet {
  user: String,
  password: String,
  targets: Vec<String>,
  txgroups: Vec<String>,
  emergency: bool,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Length of `[0-9a-z]{1,2}[0-9][a-z0-9]{1,3}` matches at the start of
/// `chars`, longest first as the greedy regex tries them.
fn callsign_lengths(chars: &[char]) -> Vec<usize> {
  let alnum = |i: usize| chars.get(i).is_some_and(char::is_ascii_alphanumeric);
  let mut lengths = Vec::new();
  for prefix in [2, 1] {
    if !(0..prefix).all(alnum) || !chars.get(prefix).is_some_and(char::is_ascii_digit) {
      continue;
    }
    for suffix in [3, 2, 1] {
      if (prefix + 1..prefix + 1 + suffix).all(alnum) {
        lengths.push(prefix + 1 + suffix);
      }
    }
  }
  lengths
}

/// Lengths of the optional `-[0-9]{1,2}` SSID at the start of `chars`,
/// longest first, then none.
fn ssid_lengths(chars: &[char]) -> Vec<usize> {
  let mut lengths = Vec::new();
  if chars.first() == Some(&'-') {
    let digits = chars[1..].iter().take(2).take_while(|c| c.is_ascii_digit()).count();
    lengths.extend((1..=digits).rev().map(|d| d + 1));
  }
  lengths.push(0);
  lengths
}

/// `findall()` of upstream `CALL_SIGN_DETECTION_RE`:
/// `\s*([0-9a-z]{1,2}[0-9][a-z0-9]{1,3}(?:-[0-9]{1,2})?)(?=\s*$|\s*[,;\s]+)`.
fn find_call_signs(s: &str) -> Vec<String> {
  let chars: Vec<char> = s.chars().collect();
  // (?=\s*$|\s*[,;\s]+)
  let followed_ok = |at: usize| {
    let rest = &chars[at..];
    let ws = rest.iter().take_while(|c| c.is_whitespace()).count();
    ws == rest.len() || matches!(rest.get(ws), Some(',' | ';')) || ws > 0
  };
  let mut found = Vec::new();
  let mut pos = 0;
  'scan: while pos < chars.len() {
    let start = pos + chars[pos..].iter().take_while(|c| c.is_whitespace()).count();
    for call in callsign_lengths(&chars[start..]) {
      for ssid in ssid_lengths(&chars[start + call..]) {
        let end = start + call + ssid;
        if followed_ok(end) {
          found.push(chars[start..end].iter().collect());
          pos = end;
          continue 'scan;
        }
      }
    }
    pos += 1;
  }
  found
}

/// Upstream `parse_call_sign()`: content without any call sign is kept,
/// split on the list delimiters, so it can be reported.
fn parse_call_sign(entries: &[String]) -> Vec<String> {
  let mut result = Vec::new();
  for entry in entries.iter().filter(|e| !e.is_empty()) {
    let found = find_call_signs(entry);
    if found.is_empty() {
      result.extend(entry.split(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | ';' | ',')).filter(|s| !s.is_empty()).map(str::to_string));
    } else {
      result.extend(found);
    }
  }
  result
}

/// Upstream `is_call_sign()`: `^([0-9a-z]{1,2}[0-9][a-z0-9]{1,3})(-[0-9]{1,2})?\s*$`.
/// Returns the call sign (without SSID) in upper case.
fn is_call_sign(target: &str) -> Option<String> {
  let chars: Vec<char> = target.trim_end().chars().collect();
  for call in callsign_lengths(&chars) {
    for ssid in ssid_lengths(&chars[call..]) {
      if call + ssid == chars.len() {
        return Some(chars[..call].iter().collect::<String>().to_uppercase());
      }
    }
  }
  None
}

impl Dapnet {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // dapnet://user:password@callsign/callsigns
    let user = url.user.clone().unwrap_or_default();
    let password = url.password.clone().unwrap_or_default();
    if user.is_empty() || password.is_empty() {
      tracing::warn!("A Dapnet user/pass was not provided.");
      return None;
    }

    let mut entries: Vec<String> = url.host.iter().cloned().collect();
    entries.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      entries.extend(parse_list(to));
    }
    let mut targets: Vec<String> = Vec::new();
    for target in parse_call_sign(&entries) {
      match is_call_sign(&target) {
        Some(call) if !targets.contains(&call) => targets.push(call),
        Some(_) => {}
        None => tracing::warn!("Dropping invalid Amateur radio call sign ({}).", target),
      }
    }

    // An unknown priority is normal
    let emergency = url.get("priority").map(str::trim).filter(|v| !v.is_empty()).and_then(|p| {
      let p = p.to_lowercase();
      PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, e)| *e)
    });
    let txgroups = url.get("txgroups").map(|t| parse_list(&t.to_lowercase())).filter(|g| !g.is_empty()).unwrap_or_else(|| vec!["dl-all".to_string()]);
    let batch = url.get("batch").map(parse_bool).unwrap_or(false);

    Some(Self {
      user,
      password,
      targets,
      txgroups,
      emergency: emergency.unwrap_or(false),
      batch,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Dapnet",
      service_url: Some("https://hampager.de/"),
      setup_url: Some("https://appriseit.com/services/dapnet/"),
      protocols: vec!["dapnet"],
      description: "Send pager messages via DAPNET.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Dapnet {
  fn schemas(&self) -> &[&str] {
    &["dapnet"]
  }
  fn service_name(&self) -> &str {
    "Dapnet"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    80
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Amateur radio callsigns to notify");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/calls", service_base("http://www.hampager.de:8080"));
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, chunk) in self.targets.chunks(batch_size).enumerate() {
      // Skip a batch that already went out so a retry does not deliver it
      // to those recipients twice
      let key = (index * batch_size).to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }

      let payload = json!({
        "text": ctx.body,
        "callSignNames": chunk,
        "transmitterGroupNames": self.txgroups,
        "emergency": self.emergency,
      });
      tracing::debug!("DAPNET POST URL: {}", url);
      crate::notify::throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json; charset=utf-8")
        .basic_auth(&self.user, Some(&self.password))
        .body(serde_json::to_string(&payload)?)
        .send()
        .await;
      match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::CREATED => {
          tracing::info!("Sent DAPNET notification to {:?}", chunk);
          crate::delivery::mark_delivered(&key, false);
        }
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!("Failed to send DAPNET notification to {:?}: error={}.", chunk, status.as_u16());
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending DAPNET notification to {:?}", chunk);
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

  fn dapnet(url: &str) -> Dapnet {
    Dapnet::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["dapnet://", "dapnet://:@/", "dapnet://user:pass", "dapnet://user@host"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "dapnet://user:pass@DF1ABC",
      "dapnet://user:pass@DF1ABC/DF1DEF",
      "dapnet://user:pass@DF1ABC-1/DF1ABC/DF1ABC-15",
      "dapnet://user:pass@?to=DF1ABC,DF1DEF",
      "dapnet://user:pass@DF1ABC?priority=normal",
      "dapnet://user:pass@DF1ABC?priority=em&batch=false",
      "dapnet://user:pass@DF1ABC?priority=invalid",
      "dapnet://user:pass@DF1ABC?txgroups=dl-all,all",
      "dapnet://user:pass@DF1ABC?txgroups=invalid",
      // No valid call signs (notify fails)
      "dapnet://user:pass@abcdefghi/a",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_call_sign_parsing() {
    // Results captured from upstream parse_call_sign()
    let p = |s: &str| parse_call_sign(&[s.to_string()]);
    assert_eq!(p("DF1ABC"), vec!["DF1ABC"]);
    assert_eq!(p("abcdefgh"), vec!["abcdefgh"]);
    assert_eq!(p("ABCDEF1GH"), vec!["EF1GH"]);
    assert_eq!(p("DF1ABC-12 DB0X"), vec!["DF1ABC-12", "DB0X"]);
    assert_eq!(p("DF1ABC-123"), vec!["123"]);
    assert_eq!(p("1A1B"), vec!["1A1B"]);
    assert_eq!(p("DF1ABC,DF2ABC;x"), vec!["DF1ABC", "DF2ABC"]);

    assert_eq!(is_call_sign("df1abc-15").as_deref(), Some("DF1ABC"));
    assert_eq!(is_call_sign("123").as_deref(), Some("123"));
    assert_eq!(is_call_sign("abcdefgh"), None);
    assert_eq!(is_call_sign("DF1ABC-123"), None);
  }

  #[test]
  fn test_from_url_fields() {
    let d = dapnet("dapnet://user:pass@df1abc/DF1ABC-1/abcdefgh/dl1xyz?to=DB0ABC,EF1GH&txgroups=DL-NW,dl-all&priority=em&batch=yes");
    assert_eq!(d.user, "user");
    assert_eq!(d.password, "pass");
    // Upper-cased, de-duplicated, invalid ones dropped
    assert_eq!(d.targets, vec!["DF1ABC", "DL1XYZ", "DB0ABC", "EF1GH"]);
    assert_eq!(d.txgroups, vec!["dl-all", "dl-nw"]);
    assert!(d.emergency);
    assert!(d.batch);

    let d = dapnet("dapnet://user:pass@DF1ABC?priority=invalid&txgroups=");
    assert!(!d.emergency);
    assert_eq!(d.txgroups, vec!["dl-all"]);
    assert!(dapnet("dapnet://user:pass@DF1ABC?priority=1").emergency);
    assert!(dapnet("dapnet://user:pass@abcdefghi/a").targets.is_empty());
  }

  #[test]
  fn test_credentials_from_arguments() {
    let d = dapnet("dapnet://DF1ABC?user=user&pass=pass");
    assert_eq!((d.user.as_str(), d.password.as_str()), ("user", "pass"));
  }

  #[test]
  fn test_service_details() {
    let d = Dapnet::static_details();
    assert_eq!(d.service_name, "Dapnet");
    assert!(d.protocols.contains(&"dapnet"));
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/calls"))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(body_json(json!({"text": "title\r\nbody", "callSignNames": ["DF1ABC", "DL1XYZ", "DB0ABC", "EF1GH"], "transmitterGroupNames": ["dl-all", "dl-nw"], "emergency": true})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("dapnet://user:pass@df1abc/DF1ABC-1/abcdefgh/dl1xyz?to=DB0ABC,EF1GH&txgroups=DL-NW,dl-all&priority=em&batch=yes").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_per_call_sign_and_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    let server = MockServer::start().await;
    for call in ["DF1ABC", "DF1DEF"] {
      Mock::given(method("POST"))
        .and(path("/calls"))
        .and(body_json(json!({"text": "b", "callSignNames": [call], "transmitterGroupNames": ["dl-all"], "emergency": false})))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("dapnet://user:pass@DF1ABC/DF1DEF").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Only 201 Created is success
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/calls")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    assert!(!dapnet("dapnet://user:pass@abcdefghi/a").send(&ctx).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_call_signs() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    let server = MockServer::start().await;
    let calls = || Mock::given(method("POST")).and(path("/calls"));
    calls().and(body_partial_json(json!({"callSignNames": ["DF1ABC"]}))).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The second call sign fails once, then accepts the retry
    let second = json!({"callSignNames": ["DF1DEF"]});
    calls().and(body_partial_json(&second)).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    calls().and(body_partial_json(&second)).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("dapnet://user:pass@DF1ABC/DF1DEF?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx)).await.is_success());
  }
}

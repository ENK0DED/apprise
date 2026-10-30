//! Web configuration (upstream `apprise/config/http.py`).
//!
//! The configuration is fetched with a POST to the URL's path; the query
//! string configures Apprise and is not sent. `?+Name=value` and
//! `?-Name=value` add request headers, user and password become Basic
//! auth, and the `Content-Type` of the reply picks the format unless
//! `?format=` already did.

use std::sync::OnceLock;
use std::time::Duration;

use regex_lite::Regex;

use super::{ConfigFormat, MAX_BUFFER_SIZE, loggable};
use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::utils::parse::{ParsedUrl, URL_PATH_SAFE_CHARS, parse_bool, quote, unquote};

/// Upstream `URLBase` socket timeouts, in seconds.
const DEFAULT_CONNECT_TIMEOUT: f64 = 4.0;
const DEFAULT_READ_TIMEOUT: f64 = 4.0;

/// The URL the configuration is fetched from: schema, host, port and
/// path, without the query string.
pub(crate) fn fetch_url(parsed: &ParsedUrl) -> String {
  let mut url = format!("{}://{}", parsed.schema, parsed.host.as_deref().unwrap_or_default());
  if let Some(port) = parsed.port {
    url.push_str(&format!(":{}", port));
  }
  // Upstream requests its decoded fullpath, which the HTTP library then
  // quotes again (so an escaped slash becomes a separator).
  url.push_str(&quote(&unquote(&parsed.fullpath()), URL_PATH_SAFE_CHARS));
  url
}

/// Request headers: ?-name first, then ?+name, over the User-Agent.
pub(crate) fn headers(parsed: &ParsedUrl, asset: &AppriseAsset) -> Vec<(String, String)> {
  let mut out: Vec<(String, String)> = vec![("User-Agent".into(), asset.app_id.clone())];
  for (k, v) in parsed.qsd_del.iter().chain(parsed.qsd_add.iter()) {
    if let Some(existing) = out.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(k)) {
      existing.1 = v.clone();
    } else {
      out.push((k.clone(), v.clone()));
    }
  }
  out
}

fn timeout(parsed: &ParsedUrl, key: &str, default: f64) -> Duration {
  let secs = parsed.get(key).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(default);
  Duration::from_secs_f64(secs)
}

/// Upstream MIME_IS_YAML / MIME_IS_TEXT (matched at the start).
fn format_from_content_type(content_type: &str) -> Option<ConfigFormat> {
  static YAML: OnceLock<Regex> = OnceLock::new();
  static TEXT: OnceLock<Regex> = OnceLock::new();
  if YAML.get_or_init(|| Regex::new(r"(?i)^(text|application)/(x-)?yaml").unwrap()).is_match(content_type) {
    Some(ConfigFormat::Yaml)
  } else if TEXT.get_or_init(|| Regex::new(r"(?i)^text/(plain|html)").unwrap()).is_match(content_type) {
    Some(ConfigFormat::Text)
  } else {
    None
  }
}

/// Fetch a web configuration.
pub async fn read_http(parsed: &ParsedUrl, asset: &AppriseAsset) -> Result<(String, Option<ConfigFormat>), ConfigError> {
  crate::ensure_crypto_provider();
  let url = fetch_url(parsed);
  let verify = parsed.verify_certificate();
  let redirects = parsed.get("redirect").map(parse_bool).unwrap_or(true);
  tracing::debug!("HTTP POST URL: {} (cert_verify={})", loggable(&url, asset), verify);

  let client = reqwest::Client::builder()
    .connect_timeout(timeout(parsed, "cto", DEFAULT_CONNECT_TIMEOUT))
    .read_timeout(timeout(parsed, "rto", DEFAULT_READ_TIMEOUT))
    .danger_accept_invalid_certs(!verify)
    .redirect(if redirects { reqwest::redirect::Policy::default() } else { reqwest::redirect::Policy::none() })
    .build()?;

  let mut request = client.post(&url);
  for (k, v) in headers(parsed, asset) {
    request = request.header(k, v);
  }
  if parsed.user.is_some() || parsed.password.is_some() {
    // Basic auth also permits a password with an empty user name
    request = request.basic_auth(parsed.user.clone().unwrap_or_default(), parsed.password.clone());
  }

  let fail = |e: reqwest::Error| {
    tracing::debug!("Socket Exception: {}", e);
    ConfigError::Other(format!("A Connection error occurred retrieving HTTP configuration from {}.", parsed.host.as_deref().unwrap_or_default()))
  };
  let response = request.send().await.map_err(fail)?;

  let status = response.status();
  if status.is_redirection() {
    return Err(ConfigError::Other(format!("HTTP redirect encountered but redirect following is disabled: {}", loggable(&url, asset))));
  }
  if !status.is_success() {
    return Err(ConfigError::Other(format!(
      "Failed to retrieve HTTP configuration from {} (error={}).",
      parsed.host.as_deref().unwrap_or_default(),
      status.as_u16()
    )));
  }

  let too_big = || ConfigError::Other(format!("HTTP config response exceeds maximum buffer length ({}KB);", MAX_BUFFER_SIZE / 1024));
  if response.content_length().is_some_and(|n| n as usize > MAX_BUFFER_SIZE) {
    return Err(too_big());
  }
  let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
  // Read at most what MAX_BUFFER_SIZE characters can take in UTF-8, so a
  // reply without Content-Length cannot exhaust memory.
  let bytes = crate::utils::http::read_bounded_response(response, MAX_BUFFER_SIZE * 4).await.map_err(fail)?.ok_or_else(too_big)?;
  let body = String::from_utf8_lossy(&bytes).into_owned();
  if body.chars().count() > MAX_BUFFER_SIZE {
    return Err(too_big());
  }

  Ok((body, format_from_content_type(&content_type)))
}

#[cfg(test)]
mod tests {
  use super::*;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parsed(url: &str) -> ParsedUrl {
    ParsedUrl::parse(url).unwrap()
  }

  #[test]
  fn test_fetch_url_drops_query() {
    assert_eq!(fetch_url(&parsed("https://example.com/cfg?format=yaml&cache=no")), "https://example.com/cfg");
    assert_eq!(fetch_url(&parsed("http://u:p@example.com:8080/a%20b/c%2Fd/100%25?x=1")), "http://example.com:8080/a%20b/c/d/100%25");
    assert_eq!(fetch_url(&parsed("http://example.com")), "http://example.com/");
  }

  #[test]
  fn test_headers() {
    let h = headers(&parsed("http://example.com/?-X-Del=1&+X-Token=abc&+User-Agent=custom"), &AppriseAsset::default());
    assert!(h.contains(&("User-Agent".to_string(), "custom".to_string())));
    assert!(h.contains(&("X-Token".to_string(), "abc".to_string())));
    assert!(h.contains(&("X-Del".to_string(), "1".to_string())));
  }

  #[test]
  fn test_content_type_format() {
    assert_eq!(format_from_content_type("text/yaml"), Some(ConfigFormat::Yaml));
    assert_eq!(format_from_content_type("application/x-yaml; charset=utf-8"), Some(ConfigFormat::Yaml));
    assert_eq!(format_from_content_type("text/plain"), Some(ConfigFormat::Text));
    assert_eq!(format_from_content_type("text/html"), Some(ConfigFormat::Text));
    assert_eq!(format_from_content_type("application/json"), None);
  }

  #[tokio::test]
  async fn test_read_http_posts_and_detects_yaml() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/get/cfg"))
      .and(header("User-Agent", "Apprise"))
      .and(header("X-Token", "abc"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(body_string(""))
      .respond_with(ResponseTemplate::new(200).set_body_raw("urls:\n  - json://localhost\n", "text/yaml"))
      .expect(1)
      .mount(&server)
      .await;

    let url = server.uri().replace("http://", "http://user:pass@") + "/get/cfg?+X-Token=abc&format=bogus";
    let (body, format) = read_http(&parsed(&url), &AppriseAsset::default()).await.unwrap();
    assert!(body.contains("json://localhost"));
    assert_eq!(format, Some(ConfigFormat::Yaml));
  }

  #[tokio::test]
  async fn test_read_http_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/missing")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("POST"))
      .and(path("/big"))
      .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(MAX_BUFFER_SIZE + 1)))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/moved")).respond_with(ResponseTemplate::new(302).insert_header("Location", "/elsewhere")).mount(&server).await;

    let asset = AppriseAsset::default();
    assert!(read_http(&parsed(&format!("{}/missing", server.uri())), &asset).await.is_err());
    assert!(read_http(&parsed(&format!("{}/big", server.uri())), &asset).await.is_err());
    assert!(read_http(&parsed(&format!("{}/moved?redirect=no", server.uri())), &asset).await.is_err());
  }

  #[tokio::test]
  async fn test_read_http_stops_at_the_limit_without_content_length() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A chunked reply that would go on far past the limit
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
      loop {
        let Ok((mut socket, _)) = listener.accept().await else { return };
        let mut request = [0u8; 4096];
        let n = socket.read(&mut request).await.unwrap_or(0);
        if !request[..n].starts_with(b"POST") {
          continue; // a stray local probe
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n").await.ok();
        let chunk = format!("{:x}\r\n{}\r\n", 64 * 1024, "a".repeat(64 * 1024));
        for _ in 0..64 {
          if socket.write_all(chunk.as_bytes()).await.is_err() {
            return; // the client stopped reading
          }
        }
        return;
      }
    });
    let parsed = ParsedUrl::parse(&format!("http://127.0.0.1:{}/cfg", port)).unwrap();
    let err = read_http(&parsed, &AppriseAsset::default()).await.unwrap_err();
    assert!(err.to_string().contains("exceeds maximum buffer length"), "{}", err);
    server.abort();
  }
}

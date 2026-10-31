//! Web attachments (upstream `AttachHTTP`).

use std::collections::HashMap;

use super::{CommonOptions, MAX_FILE_SIZE, UNKNOWN_MIMETYPE, basename, exceeds, final_name, guess_mime, parse_query};
use crate::error::AttachError;
use crate::notify::{APP_ID, Attachment, ClientDefaults, HttpOptions, client_builder, current_http_options, with_http_options};
use crate::utils::cwe312::cwe312_loggable;
use crate::utils::parse::parse_bool;

/// Query arguments the attachment itself consumes (upstream
/// `AttachBase.template_args`); the rest go to the server.
const RESERVED: &[&str] = &["cache", "mime", "name", "verify"];

/// Content fetched over HTTP(S).
#[derive(Debug, Clone)]
pub struct HttpSource {
  /// The URL requested: credentials and reserved arguments removed.
  pub url: url::Url,
  pub user: Option<String>,
  pub password: Option<String>,
  pub headers: Vec<(String, String)>,
  pub name: Option<String>,
  pub mime: Option<String>,
  pub http: HttpOptions,
  /// The largest download accepted, in bytes (upstream `max_file_size`);
  /// `0` disables the check.
  pub max_file_size: u64,
  /// The source with secrets masked, for logs.
  pub display: String,
}

impl HttpSource {
  pub fn from_url(source: &str) -> Result<Self, AttachError> {
    let shown = cwe312_loggable(source, true);
    let mut url = url::Url::parse(source.trim()).map_err(|_| {
      tracing::warn!("Unparseable URL {}.", shown);
      AttachError::Other(format!("Unparseable URL {shown}."))
    })?;
    if url.host_str().is_none_or(str::is_empty) {
      tracing::warn!("Unparseable URL {}.", shown);
      return Err(AttachError::Other(format!("Unparseable URL {shown}.")));
    }
    let decode = |s: &str| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
    let user = Some(decode(url.username())).filter(|u| !u.is_empty());
    let password = url.password().map(decode);
    let pairs = parse_query(url.query().unwrap_or(""));
    let qsd: HashMap<String, String> = pairs.iter().filter(|(k, _)| !k.starts_with(['+', '-'])).map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
    let opts = CommonOptions::from_query(&qsd)?;
    let headers = pairs.iter().filter_map(|(k, v)| k.strip_prefix(['+', '-']).filter(|h| !h.is_empty()).map(|h| (h.to_string(), v.clone()))).collect();
    let timeout = |key: &str| qsd.get(key).and_then(|v| v.trim().parse::<f64>().ok()).filter(|t| t.is_finite() && *t > 0.0);
    let http = HttpOptions {
      connect_timeout: timeout("cto"),
      read_timeout: timeout("rto"),
      // `None` when the URL says nothing; the load takes the default.
      redirect: qsd.get("redirect").map(|v| parse_bool(v)),
      verify: Some(qsd.get("verify").is_none_or(|v| parse_bool(v))),
    };
    // Header values are never sent as query arguments.
    let forward: Vec<(String, String)> =
      pairs.into_iter().filter(|(k, _)| !k.starts_with(['+', '-']) && !RESERVED.contains(&k.to_lowercase().as_str())).collect();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if forward.is_empty() {
      url.set_query(None);
    } else {
      url.query_pairs_mut().clear().extend_pairs(forward);
    }
    Ok(Self { url, user, password, headers, name: opts.name, mime: opts.mime, http, max_file_size: MAX_FILE_SIZE, display: shown })
  }

  /// Download the content, refusing anything over
  /// [`max_file_size`](Self::max_file_size) before reading past it.
  ///
  /// Redirects follow the URL's `?redirect=`, else the caller's
  /// [`with_http_options`] (the asset's `http_redirects`, as upstream
  /// `URLBase.redirects`), else they are followed.
  pub async fn load(&self) -> Result<Attachment, AttachError> {
    let mut http = self.http.clone();
    http.redirect = http.redirect.or(current_http_options().redirect);
    with_http_options(http, self.fetch(self.max_file_size)).await
  }

  async fn fetch(&self, limit: u64) -> Result<Attachment, AttachError> {
    let fail = |e: reqwest::Error| {
      tracing::error!("A Connection error occurred retrieving HTTP attachment from {}.", self.url.host_str().unwrap_or(""));
      tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
      AttachError::NotFound(self.display.clone())
    };
    let client = client_builder(true, ClientDefaults::default()).build().map_err(fail)?;
    let mut request = client.get(self.url.clone()).header("User-Agent", APP_ID);
    for (k, v) in &self.headers {
      request = request.header(k.as_str(), v.as_str());
    }
    if let Some(user) = &self.user {
      request = request.basic_auth(user, self.password.as_deref());
    }
    tracing::debug!("HTTP Attachment Fetch URL: {} (cert_verify={})", cwe312_loggable(self.url.as_str(), true), self.http.verify.unwrap_or(true));
    let mut response = request.send().await.map_err(fail)?;
    let status = response.status();
    if status.is_redirection() {
      tracing::error!("HTTP redirect encountered but redirect following is disabled: {}", self.display);
      return Err(AttachError::NotFound(self.display.clone()));
    }
    if !status.is_success() {
      tracing::error!("Failed to retrieve HTTP attachment {}: error={}", self.display, status.as_u16());
      return Err(AttachError::NotFound(self.display.clone()));
    }
    let too_large = || {
      tracing::error!("HTTP response exceeds allowable maximum file length ({}KB): {}", limit / 1024, self.display);
      AttachError::TooLarge { source_url: self.display.clone(), limit }
    };
    if response.content_length().is_some_and(|len| exceeds(len, limit)) {
      return Err(too_large());
    }
    let header = |name: reqwest::header::HeaderName| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let content_type = header(reqwest::header::CONTENT_TYPE).filter(|v| !v.is_empty());
    let disposition = header(reqwest::header::CONTENT_DISPOSITION).and_then(|d| disposition_filename(&d));
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(fail)? {
      if exceeds((data.len() + chunk.len()) as u64, limit) {
        return Err(too_large());
      }
      data.extend_from_slice(&chunk);
    }
    let path_name = urlencoding::decode(basename(self.url.path())).map(|n| n.into_owned()).unwrap_or_default();
    let detected = disposition.unwrap_or(path_name);
    let mime_type =
      self.mime.clone().or(content_type).or_else(|| guess_mime(self.name.as_deref().unwrap_or(&detected))).unwrap_or_else(|| UNKNOWN_MIMETYPE.to_string());
    let name = final_name(self.name.as_deref(), Some(&detected), &mime_type);
    Ok(Attachment { name, data, mime_type })
  }
}

/// The `filename=` of a Content-Disposition header, without directories.
fn disposition_filename(value: &str) -> Option<String> {
  let lower = value.to_ascii_lowercase();
  let start = lower.find("filename=")? + "filename=".len();
  let rest = value[start..].trim_start_matches(['"', '\'']);
  let end = rest.find(['"', '\'', ';']).unwrap_or(rest.len());
  Some(basename(rest[..end].trim()).to_string()).filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_disposition_filename() {
    assert_eq!(disposition_filename("attachment; filename=\"report.txt\""), Some("report.txt".into()));
    assert_eq!(disposition_filename("attachment; FILENAME='a b.pdf'"), Some("a b.pdf".into()));
    assert_eq!(disposition_filename("attachment; filename=x.bin; size=3"), Some("x.bin".into()));
    assert_eq!(disposition_filename("attachment; filename=\"../../etc/passwd\""), Some("passwd".into()));
    assert_eq!(disposition_filename("inline"), None);
  }
}

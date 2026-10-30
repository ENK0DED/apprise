//! Options every service URL accepts, regardless of the plugin.
//!
//! Upstream Apprise parses these in `URLBase` and `NotifyBase`: `?format=`,
//! `?overflow=`, `?emojis=`, `?store=`, `?tz=`, `?retry=`, `?wait=`,
//! `?optional=`, `?redirect=`, `?cto=`, `?rto=` and `?verify=`. Plugins are
//! not expected to parse them themselves. `registry::from_url` wraps every
//! plugin in a [`ConfiguredService`] that carries the parsed values, so the
//! orchestrator can read them through [`Notify::service_options`].

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, OverflowMode, ServiceDetails};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool};

/// Highest `?retry=` value accepted (upstream `APPRISE_MAX_SERVICE_RETRY`).
pub const MAX_SERVICE_RETRY: u32 = 10;

/// Highest `?wait=` value accepted, in seconds (upstream `APPRISE_MAX_SERVICE_WAIT`).
pub const MAX_SERVICE_WAIT: f64 = 20.0;

/// Common URL options. `None` means the URL did not set the value, so the
/// plugin or asset default applies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceOptions {
  /// `?format=`: requested body format.
  pub format: Option<NotifyFormat>,
  /// `?overflow=`: how to handle bodies longer than the plugin allows.
  pub overflow: Option<OverflowMode>,
  /// `?emojis=`: interpret `:emoji:` shortcodes.
  pub emojis: Option<bool>,
  /// `?store=`: allow persistent storage for this service.
  pub store: Option<bool>,
  /// `?tz=`: timezone name used when rendering times.
  pub tz: Option<String>,
  /// `?retry=`: extra attempts after a failed send (0..=MAX_SERVICE_RETRY).
  pub retry: Option<u32>,
  /// `?wait=`: seconds between retry attempts (0..=MAX_SERVICE_WAIT).
  pub wait: Option<f64>,
  /// `?optional=`: a failure of this service does not fail the run.
  pub optional: Option<bool>,
  /// `?redirect=`: follow HTTP 3xx redirects.
  pub redirect: Option<bool>,
  /// `?cto=`: socket connect timeout in seconds.
  pub cto: Option<f64>,
  /// `?rto=`: socket read timeout in seconds.
  pub rto: Option<f64>,
  /// `?verify=`: verify TLS certificates.
  pub verify: Option<bool>,
}

impl ServiceOptions {
  /// Parse the common options from a URL.
  ///
  /// Returns `Err` for values upstream rejects outright (a negative or
  /// non-finite `retry` or `wait`). The URL must not load in that case.
  /// Unparseable values fall back to the default, as upstream does.
  pub fn from_url(url: &ParsedUrl) -> Result<Self, String> {
    let mut opts = Self::default();

    if let Some(v) = url.get("format") {
      match v.parse::<NotifyFormat>() {
        Ok(f) => opts.format = Some(f),
        Err(_) => tracing::warn!("Invalid format specified ({}); using default", v),
      }
    }

    if let Some(v) = url.get("overflow") {
      match v.parse::<OverflowMode>() {
        Ok(m) => opts.overflow = Some(m),
        Err(_) => tracing::warn!("Invalid overflow method specified ({}); using default", v),
      }
    }

    opts.emojis = url.get("emojis").map(parse_bool);
    opts.store = url.get("store").map(parse_bool);
    opts.tz = url.get("tz").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    opts.optional = url.get("optional").map(parse_bool);
    opts.redirect = url.get("redirect").map(parse_bool);
    opts.verify = url.get("verify").map(parse_bool);

    if let Some(v) = url.get("retry") {
      match v.trim().parse::<i64>() {
        Ok(n) if n < 0 => return Err(format!("Service retry count must be >= 0; got {}", v)),
        Ok(n) => opts.retry = Some((n as u64).min(MAX_SERVICE_RETRY as u64) as u32),
        Err(_) => tracing::warn!("Invalid retry value specified ({}); using default", v),
      }
    }

    if let Some(v) = url.get("wait") {
      match v.trim().parse::<f64>() {
        Ok(n) if !n.is_finite() || n < 0.0 => return Err(format!("Service retry wait must be >= 0; got {}", v)),
        Ok(n) => opts.wait = Some(n.min(MAX_SERVICE_WAIT)),
        Err(_) => tracing::warn!("Invalid wait value specified ({}); using default", v),
      }
    }

    opts.cto = parse_timeout(url.get("cto"), "cto");
    opts.rto = parse_timeout(url.get("rto"), "rto");

    Ok(opts)
  }
}

fn parse_timeout(value: Option<&str>, name: &str) -> Option<f64> {
  let v = value?;
  match v.trim().parse::<f64>() {
    Ok(n) if n.is_finite() && n > 0.0 => Some(n),
    _ => {
      tracing::warn!("Invalid {} value specified ({}); using default", name, v);
      None
    }
  }
}

/// A plugin instance together with the common options from its URL.
///
/// Every method of [`Notify`] must be forwarded to `inner` here. A method
/// that is not forwarded silently falls back to the trait default and
/// bypasses the plugin's own implementation.
pub struct ConfiguredService {
  inner: Box<dyn Notify>,
  options: ServiceOptions,
}

impl ConfiguredService {
  pub fn new(inner: Box<dyn Notify>, options: ServiceOptions) -> Self {
    Self { inner, options }
  }

  /// The wrapped plugin.
  pub fn plugin(&self) -> &dyn Notify {
    self.inner.as_ref()
  }
}

#[async_trait]
impl Notify for ConfiguredService {
  fn schemas(&self) -> &[&str] {
    self.inner.schemas()
  }
  fn service_name(&self) -> &str {
    self.inner.service_name()
  }
  fn details(&self) -> ServiceDetails {
    self.inner.details()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    self.inner.send(ctx).await
  }
  fn attachment_support(&self) -> bool {
    self.inner.attachment_support()
  }
  fn tags(&self) -> Vec<String> {
    self.inner.tags()
  }
  fn notify_format(&self) -> NotifyFormat {
    self.inner.notify_format()
  }
  fn body_maxlen(&self) -> usize {
    self.inner.body_maxlen()
  }
  fn title_maxlen(&self) -> usize {
    self.inner.title_maxlen()
  }
  fn request_rate_per_sec(&self) -> f64 {
    self.inner.request_rate_per_sec()
  }
  fn overflow_mode(&self) -> OverflowMode {
    self.inner.overflow_mode()
  }
  fn body_max_line_count(&self) -> usize {
    self.inner.body_max_line_count()
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    self.inner.dialect_convert(body, body_format)
  }
  fn service_options(&self) -> Option<&ServiceOptions> {
    Some(&self.options)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn opts(url: &str) -> Result<ServiceOptions, String> {
    ServiceOptions::from_url(&ParsedUrl::parse(url).unwrap())
  }

  #[test]
  fn test_defaults_are_unset() {
    assert_eq!(opts("json://localhost").unwrap(), ServiceOptions::default());
  }

  #[test]
  fn test_parses_all_options() {
    let o =
      opts("json://localhost/?format=markdown&overflow=split&emojis=yes&store=no&tz=UTC&retry=3&wait=1.5&optional=yes&redirect=no&cto=2&rto=7.5&verify=no")
        .unwrap();
    assert_eq!(o.format, Some(NotifyFormat::Markdown));
    assert_eq!(o.overflow, Some(OverflowMode::Split));
    assert_eq!(o.emojis, Some(true));
    assert_eq!(o.store, Some(false));
    assert_eq!(o.tz.as_deref(), Some("UTC"));
    assert_eq!(o.retry, Some(3));
    assert_eq!(o.wait, Some(1.5));
    assert_eq!(o.optional, Some(true));
    assert_eq!(o.redirect, Some(false));
    assert_eq!(o.cto, Some(2.0));
    assert_eq!(o.rto, Some(7.5));
    assert_eq!(o.verify, Some(false));
  }

  #[test]
  fn test_retry_and_wait_are_clamped() {
    let o = opts("json://localhost/?retry=99&wait=100").unwrap();
    assert_eq!(o.retry, Some(MAX_SERVICE_RETRY));
    assert_eq!(o.wait, Some(MAX_SERVICE_WAIT));
  }

  #[test]
  fn test_negative_retry_or_wait_rejects_url() {
    assert!(opts("json://localhost/?retry=-1").is_err());
    assert!(opts("json://localhost/?wait=-0.5").is_err());
    assert!(opts("json://localhost/?wait=inf").is_err());
  }

  #[test]
  fn test_invalid_values_fall_back_to_default() {
    let o = opts("json://localhost/?retry=abc&wait=abc&format=bogus&overflow=bogus&cto=0&rto=-1").unwrap();
    assert_eq!(o, ServiceOptions::default());
  }

  #[test]
  fn test_from_url_wraps_plugins() {
    let svc = crate::notify::registry::from_url("json://localhost/?retry=2").unwrap();
    assert_eq!(svc.service_options().and_then(|o| o.retry), Some(2));
    assert!(crate::notify::registry::from_url("json://localhost/?retry=-2").is_none());
  }
}

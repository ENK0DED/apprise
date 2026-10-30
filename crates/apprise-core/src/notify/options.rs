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
use crate::notify::{Notify, NotifyContext, OverflowMode, ServiceDetails, ThrottleClock};
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
  url: Option<String>,
  clock: ThrottleClock,
}

impl ConfiguredService {
  pub fn new(inner: Box<dyn Notify>, options: ServiceOptions) -> Self {
    Self { inner, options, url: None, clock: ThrottleClock::default() }
  }

  /// Remember the URL the service was loaded from (for masked display and
  /// its storage namespace).
  pub fn with_url(mut self, url: &str) -> Self {
    self.url = Some(url.to_string());
    self
  }

  /// The wrapped plugin.
  pub fn plugin(&self) -> &dyn Notify {
    self.inner.as_ref()
  }
}

/// The persistent storage namespace of a service (upstream `url_id()`): a
/// SHA-256 over `salt` and the plugin's [`Notify::url_identifier`], or over
/// the URL's schema, credentials, host, port and path, cut to `idlen`
/// characters (`0` keeps all 64). `None` when the service has no URL.
pub fn url_id(svc: &dyn Notify, salt: &[u8], idlen: usize) -> Option<String> {
  use sha2::{Digest, Sha256};
  let mut engine = Sha256::new();
  engine.update(salt);
  if let Some(parts) = svc.url_identifier() {
    for part in parts {
      engine.update(part.as_bytes());
    }
  } else {
    let url = ParsedUrl::parse(svc.source_url()?)?;
    let field = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(|s| s.as_bytes().to_vec()).unwrap_or_else(|| b"\0".to_vec());
    engine.update(url.schema.as_bytes());
    engine.update(url.password.as_deref().map(|p| p.as_bytes().to_vec()).unwrap_or_else(|| b"\0".to_vec()));
    engine.update(url.user.as_deref().map(|u| u.as_bytes().to_vec()).unwrap_or_else(|| b"\0".to_vec()));
    engine.update(field(&url.host));
    engine.update(url.port.map(|p| p.to_string().into_bytes()).unwrap_or_else(|| b"\0".to_vec()));
    engine.update(format!("/{}", url.path).trim_end_matches('/').as_bytes());
    engine.update(if url.is_secure() { b"s" } else { b"i" });
  }
  let id = hex::encode(engine.finalize());
  Some(if idlen > 0 && idlen < id.len() { id[..idlen].to_string() } else { id })
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
    // Many plugins only declare support in their details.
    self.inner.attachment_support() || self.inner.details().attachment_support
  }
  fn attachment_send_index(&self, total: usize) -> usize {
    self.inner.attachment_send_index(total)
  }
  fn tags(&self) -> Vec<String> {
    self.inner.tags()
  }
  fn notify_format(&self) -> NotifyFormat {
    self.inner.notify_format()
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    self.inner.notify_formats()
  }
  fn notify_format_overridable(&self) -> bool {
    self.inner.notify_format_overridable()
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
  fn overflow_mode_overridable(&self) -> bool {
    self.inner.overflow_mode_overridable()
  }
  fn body_max_line_count(&self) -> usize {
    self.inner.body_max_line_count()
  }
  fn overflow_amalgamate_title(&self) -> bool {
    self.inner.overflow_amalgamate_title()
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    self.inner.dialect_convert(body, body_format)
  }
  fn has_dialect(&self) -> bool {
    self.inner.has_dialect()
  }
  fn service_options(&self) -> Option<&ServiceOptions> {
    Some(&self.options)
  }
  fn source_url(&self) -> Option<&str> {
    self.url.as_deref().or_else(|| self.inner.source_url())
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    self.inner.url_identifier()
  }
  fn throttle_clock(&self) -> Option<ThrottleClock> {
    Some(self.inner.throttle_clock().unwrap_or_else(|| self.clock.clone()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A plugin that overrides every [`Notify`] method with a non-default value.
  /// When a method is added to the trait, override it here and compare it in
  /// [`assert_forwards`]; the test then fails until `ConfiguredService`
  /// forwards it.
  struct EveryOverride;

  #[async_trait]
  impl Notify for EveryOverride {
    fn schemas(&self) -> &[&str] {
      &["every"]
    }
    fn service_name(&self) -> &str {
      "Every Override"
    }
    fn details(&self) -> ServiceDetails {
      ServiceDetails {
        service_name: "Every Override",
        service_url: None,
        setup_url: None,
        protocols: vec!["every"],
        description: "x",
        attachment_support: true,
      }
    }
    async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
      Ok(false)
    }
    fn attachment_support(&self) -> bool {
      true
    }
    fn attachment_send_index(&self, total: usize) -> usize {
      total.saturating_sub(1)
    }
    fn tags(&self) -> Vec<String> {
      vec!["tag".into()]
    }
    fn notify_format(&self) -> NotifyFormat {
      NotifyFormat::Markdown
    }
    fn notify_formats(&self) -> Vec<NotifyFormat> {
      vec![NotifyFormat::Markdown, NotifyFormat::Html]
    }
    fn notify_format_overridable(&self) -> bool {
      false
    }
    fn body_maxlen(&self) -> usize {
      7
    }
    fn title_maxlen(&self) -> usize {
      3
    }
    fn request_rate_per_sec(&self) -> f64 {
      2.5
    }
    fn overflow_mode(&self) -> OverflowMode {
      OverflowMode::Split
    }
    fn overflow_mode_overridable(&self) -> bool {
      false
    }
    fn body_max_line_count(&self) -> usize {
      4
    }
    fn overflow_amalgamate_title(&self) -> bool {
      true
    }
    fn dialect_convert(&self, body: &str, _body_format: &NotifyFormat) -> String {
      format!("<{}>", body)
    }
    fn has_dialect(&self) -> bool {
      true
    }
  }

  /// Every overridable method of `wrapped` must return what `direct` returns.
  fn assert_forwards(direct: &dyn Notify, wrapped: &dyn Notify) {
    assert_eq!(wrapped.schemas(), direct.schemas());
    assert_eq!(wrapped.service_name(), direct.service_name());
    assert_eq!(wrapped.details().to_json(), direct.details().to_json());
    assert_eq!(wrapped.attachment_support(), direct.attachment_support());
    assert_eq!(wrapped.attachment_send_index(5), direct.attachment_send_index(5));
    assert_eq!(wrapped.tags(), direct.tags());
    assert_eq!(wrapped.notify_format(), direct.notify_format());
    assert_eq!(wrapped.notify_formats(), direct.notify_formats());
    assert_eq!(wrapped.notify_format_overridable(), direct.notify_format_overridable());
    assert_eq!(wrapped.body_maxlen(), direct.body_maxlen());
    assert_eq!(wrapped.title_maxlen(), direct.title_maxlen());
    assert_eq!(wrapped.request_rate_per_sec(), direct.request_rate_per_sec());
    assert_eq!(wrapped.overflow_mode(), direct.overflow_mode());
    assert_eq!(wrapped.overflow_mode_overridable(), direct.overflow_mode_overridable());
    assert_eq!(wrapped.body_max_line_count(), direct.body_max_line_count());
    assert_eq!(wrapped.overflow_amalgamate_title(), direct.overflow_amalgamate_title());
    for format in [NotifyFormat::Text, NotifyFormat::Html, NotifyFormat::Markdown] {
      assert_eq!(wrapped.dialect_convert("**x** & y", &format), direct.dialect_convert("**x** & y", &format));
    }
    assert_eq!(wrapped.has_dialect(), direct.has_dialect());
  }

  #[test]
  fn test_every_method_is_forwarded() {
    let wrapped = ConfiguredService::new(Box::new(EveryOverride), ServiceOptions::default());
    assert_forwards(&EveryOverride, &wrapped);
    // The wrapper reports its own options; the plugin itself has none.
    assert!(wrapped.service_options().is_some());
    assert!(EveryOverride.service_options().is_none());
    let result = tokio_test::block_on(wrapped.send(&NotifyContext::default()));
    assert!(!result.unwrap());
  }

  #[test]
  fn test_registry_plugins_keep_their_overrides() {
    // A plugin loaded through the registry must behave like the plugin itself.
    for url in ["json://localhost/", "tgram://123456789:abcdefghijklmnopqrstuvwxyz0123456789/12345", "gotify://localhost/abcdefghijklmno"] {
      let parsed = ParsedUrl::parse(url).unwrap();
      let direct: Box<dyn Notify> = match parsed.schema.as_str() {
        "json" => Box::new(crate::notify::custom_json::Json::from_url(&parsed).unwrap()),
        "tgram" => Box::new(crate::notify::telegram::Telegram::from_url(&parsed).unwrap()),
        _ => Box::new(crate::notify::gotify::Gotify::from_url(&parsed).unwrap()),
      };
      let wrapped = crate::notify::registry::from_url(url).unwrap_or_else(|| panic!("{url} loads"));
      assert_forwards(direct.as_ref(), wrapped.as_ref());
    }
  }

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
  fn test_attachment_support_follows_details() {
    // Custom JSON declares attachment support only in its details.
    let svc = crate::notify::registry::from_url("json://localhost").unwrap();
    assert!(svc.attachment_support());
    let svc = crate::notify::registry::from_url("gotify://localhost/abcdefghijklmno").unwrap();
    assert_eq!(svc.attachment_support(), svc.details().attachment_support);
  }

  #[test]
  fn test_url_id() {
    let svc = crate::notify::registry::from_url("json://user:pass@localhost/path/").unwrap();
    assert_eq!(svc.source_url(), Some("json://user:pass@localhost/path/"));
    let id = url_id(svc.as_ref(), b"", 8).unwrap();
    assert_eq!(id.len(), 8);
    assert_eq!(url_id(svc.as_ref(), b"", 0).unwrap().len(), 64);
    // Query options do not change the namespace; credentials and salt do.
    let same = crate::notify::registry::from_url("json://user:pass@localhost/path?retry=2").unwrap();
    assert_eq!(url_id(same.as_ref(), b"", 8).unwrap(), id);
    let other = crate::notify::registry::from_url("json://user:other@localhost/path/").unwrap();
    assert_ne!(url_id(other.as_ref(), b"", 8).unwrap(), id);
    assert_ne!(url_id(svc.as_ref(), b"salt", 8).unwrap(), id);
  }

  #[test]
  fn test_from_url_wraps_plugins() {
    let svc = crate::notify::registry::from_url("json://localhost/?retry=2").unwrap();
    assert_eq!(svc.service_options().and_then(|o| o.retry), Some(2));
    assert!(crate::notify::registry::from_url("json://localhost/?retry=-2").is_none());
  }
}

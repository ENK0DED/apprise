use std::borrow::Cow;
use std::path::PathBuf;

use crate::types::{NotifyType, StorageMode};
use crate::utils::time::Timezone;

/// Upstream's default image URL mask (the GitHub-hosted theme icons).
pub const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

/// Upstream's default logo URL.
pub const DEFAULT_IMAGE_URL_LOGO: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-logo.png";

/// Image sizes the theme provides (upstream `NotifyImageSize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NotifyImageSize {
  Xy32,
  Xy72,
  Xy128,
  #[default]
  Xy256,
}

impl NotifyImageSize {
  /// `32x32`, `72x72`, `128x128` or `256x256`.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Xy32 => "32x32",
      Self::Xy72 => "72x72",
      Self::Xy128 => "128x128",
      Self::Xy256 => "256x256",
    }
  }
}

impl std::fmt::Display for NotifyImageSize {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Default budget for one service, retries and waits included, in seconds
/// (upstream `AppriseAsset._service_timeout`).
pub const DEFAULT_SERVICE_TIMEOUT: f64 = 60.0;

/// Branding and run-time settings shared by every service of an `Apprise`
/// instance.
///
/// Mirrors Python's `AppriseAsset` class. Plugins can use the branding fields
/// for app identification, logos, etc.; the orchestrator reads the delivery
/// settings.
#[derive(Debug, Clone)]
pub struct AppriseAsset {
  pub app_id: String,
  pub app_desc: String,
  pub app_url: String,
  /// Mask for notification icon URLs; `{THEME}`, `{TYPE}`, `{XY}` and
  /// `{EXTENSION}` are filled in. `None` disables image URLs.
  pub image_url_mask: Option<String>,
  /// Mask for the logo URL (same placeholders). `None` disables it.
  pub image_url_logo: Option<String>,
  /// Mask for icon files on disk (same placeholders). `None` uses the icons
  /// bundled with the build (the `images` feature) for the default theme.
  pub image_path_mask: Option<String>,
  /// Extension used when a caller does not give one.
  pub default_extension: String,
  /// Size used by [`image_url`](Self::image_url) when none is given.
  pub default_image_size: NotifyImageSize,
  pub theme: String,
  pub body_format: Option<String>,
  /// Mask secrets (CWE-312) in anything Apprise logs.
  pub secure_logging: bool,
  /// Notify services concurrently. `false` sends them one after another.
  pub async_mode: bool,
  /// Retries after a first failure for services without `?retry=`.
  pub default_service_retry: u32,
  /// Seconds between retries for services without `?wait=`.
  pub default_service_wait: f64,
  /// Stop the call as soon as one tag chain has exhausted its priority
  /// groups, instead of letting every chain finish.
  pub abort_on_chain_failure: bool,
  /// Follow HTTP 3xx redirects unless a URL sets `?redirect=`.
  pub http_redirects: bool,
  /// Interpret backslash escapes (`\n`, `\t`, ...) in every notification,
  /// as if each set `NotifyContext::interpret_escapes`.
  pub interpret_escapes: bool,
  /// Generate a PGP key pair in a service's storage directory when one is
  /// needed and missing (email and Delta Chat `?pgp=`: a self-addressed
  /// encrypted mail, the sender's Autocrypt key). `false` uses existing
  /// keys only.
  pub pgp_autogen: bool,
  /// Generate PEM keys in a service's storage directory when missing
  /// (VAPID's key pair, and its starter `subscriptions.json`). `false`
  /// uses existing files only.
  pub pem_autogen: bool,
  /// Seconds one service may take, retries and waits included; it reports
  /// a timeout after that. `0` disables the limit.
  pub service_timeout: f64,
  /// Where persistent storage lives. `None` keeps everything in memory.
  pub storage_path: Option<PathBuf>,
  /// How persistent storage writes to disk.
  pub storage_mode: StorageMode,
  /// Salt mixed into every storage namespace.
  pub storage_salt: Vec<u8>,
  /// Length of storage namespace names (URL IDs); `0` keeps the full hash.
  pub storage_idlen: usize,
  /// Zone for the times services render (the email `Date` header) when a
  /// URL sets no `?tz=`. `None` is the system's local zone, upstream's
  /// default. Build one with [`zoneinfo`](crate::utils::time::zoneinfo)
  /// or set it from a YAML `asset: timezone:`.
  pub timezone: Option<Timezone>,
}

impl Default for AppriseAsset {
  fn default() -> Self {
    Self {
      app_id: "Apprise".to_string(),
      app_desc: "Apprise Notifications".to_string(),
      app_url: "https://github.com/caronc/apprise".to_string(),
      image_url_mask: Some(DEFAULT_IMAGE_URL_MASK.to_string()),
      image_url_logo: Some(DEFAULT_IMAGE_URL_LOGO.to_string()),
      image_path_mask: None,
      default_extension: ".png".to_string(),
      default_image_size: NotifyImageSize::Xy256,
      theme: "default".to_string(),
      body_format: None,
      secure_logging: true,
      async_mode: true,
      default_service_retry: 0,
      default_service_wait: 0.5,
      abort_on_chain_failure: false,
      http_redirects: true,
      interpret_escapes: false,
      pgp_autogen: true,
      pem_autogen: true,
      service_timeout: DEFAULT_SERVICE_TIMEOUT,
      storage_path: None,
      storage_mode: StorageMode::Auto,
      storage_salt: Vec::new(),
      storage_idlen: 8,
      timezone: None,
    }
  }
}

impl AppriseAsset {
  fn apply_mask(&self, mask: &str, notify_type: &NotifyType, image_size: NotifyImageSize, extension: Option<&str>) -> String {
    let extension = extension.unwrap_or(&self.default_extension);
    replace_ci(mask, &[("{THEME}", &self.theme), ("{TYPE}", notify_type.as_str()), ("{XY}", image_size.as_str()), ("{EXTENSION}", extension)])
  }

  /// The icon URL for `notify_type` (upstream `AppriseAsset.image_url`),
  /// or the logo URL with `logo`. `None` when the mask is unset.
  pub fn image_url(&self, notify_type: &NotifyType, image_size: Option<NotifyImageSize>, logo: bool, extension: Option<&str>) -> Option<String> {
    let mask = if logo { self.image_url_logo.as_deref() } else { self.image_url_mask.as_deref() }.filter(|m| !m.is_empty())?;
    Some(self.apply_mask(mask, notify_type, image_size.unwrap_or(self.default_image_size), extension))
  }

  /// The icon file for `notify_type` from `image_path_mask` (upstream
  /// `image_path`). With `must_exist`, `None` unless the file is there.
  pub fn image_path(&self, notify_type: &NotifyType, image_size: NotifyImageSize, must_exist: bool, extension: Option<&str>) -> Option<PathBuf> {
    let mask = self.image_path_mask.as_deref().filter(|m| !m.is_empty())?;
    let path = PathBuf::from(self.apply_mask(mask, notify_type, image_size, extension));
    (!must_exist || path.is_file()).then_some(path)
  }

  /// The icon's bytes (upstream `image_raw`): read from `image_path_mask`
  /// when set, else the PNG bundled for the default theme.
  pub fn image_raw(&self, notify_type: &NotifyType, image_size: NotifyImageSize, extension: Option<&str>) -> Option<Cow<'static, [u8]>> {
    if self.image_path_mask.as_deref().is_some_and(|m| !m.is_empty()) {
      let path = self.image_path(notify_type, image_size, true, extension)?;
      return std::fs::read(path).ok().map(Cow::Owned);
    }
    let extension = extension.unwrap_or(&self.default_extension);
    if self.theme != "default" || !extension.eq_ignore_ascii_case(".png") {
      return None;
    }
    bundled_image(notify_type, image_size).map(Cow::Borrowed)
  }

  /// The HTML colour for `notify_type`, e.g. `#3AA3E3` (upstream `color()`).
  pub fn color(&self, notify_type: &NotifyType) -> String {
    notify_type.color_hex()
  }

  /// The colour as an integer (upstream `color(color_type=int)`).
  pub fn color_int(&self, notify_type: &NotifyType) -> u32 {
    notify_type.color()
  }

  /// The colour as `(red, green, blue)` (upstream `color(color_type=tuple)`).
  pub fn color_rgb(&self, notify_type: &NotifyType) -> (u8, u8, u8) {
    let c = notify_type.color();
    ((c >> 16) as u8, (c >> 8) as u8, c as u8)
  }

  /// A short ASCII marker for `notify_type` (upstream `ascii()`).
  pub fn ascii(&self, notify_type: &NotifyType) -> &'static str {
    match notify_type {
      NotifyType::Info => "[i]",
      NotifyType::Success => "[+]",
      NotifyType::Failure => "[!]",
      NotifyType::Warning => "[~]",
    }
  }

  /// The asset's public settings (upstream `AppriseAsset.details()`).
  pub fn details(&self) -> serde_json::Value {
    serde_json::json!({
      "app_id": self.app_id,
      "app_desc": self.app_desc,
      "default_extension": self.default_extension,
      "theme": self.theme,
      "image_path_mask": self.image_path_mask,
      "image_url_mask": self.image_url_mask,
      "image_url_logo": self.image_url_logo,
    })
  }

  /// The per-service time limit, or `None` when disabled.
  pub fn service_timeout(&self) -> Option<std::time::Duration> {
    (self.service_timeout.is_finite() && self.service_timeout > 0.0).then(|| crate::utils::time::secs(self.service_timeout))
  }
}

/// Replace each placeholder, ignoring its case, as upstream's regex does.
fn replace_ci(mask: &str, map: &[(&str, &str)]) -> String {
  let mut out = String::with_capacity(mask.len() + 32);
  let mut rest = mask;
  'outer: while !rest.is_empty() {
    for (key, value) in map {
      if rest.len() >= key.len() && rest.is_char_boundary(key.len()) && rest[..key.len()].eq_ignore_ascii_case(key) {
        out.push_str(value);
        rest = &rest[key.len()..];
        continue 'outer;
      }
    }
    let c = rest.chars().next().expect("non-empty");
    out.push(c);
    rest = &rest[c.len_utf8()..];
  }
  out
}

#[cfg(feature = "images")]
fn bundled_image(notify_type: &NotifyType, image_size: NotifyImageSize) -> Option<&'static [u8]> {
  macro_rules! png {
    ($t:literal, $xy:literal) => {
      include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/themes/default/apprise-", $t, "-", $xy, ".png")).as_slice()
    };
  }
  use NotifyImageSize::*;
  Some(match (notify_type, image_size) {
    (NotifyType::Info, Xy32) => png!("info", "32x32"),
    (NotifyType::Info, Xy72) => png!("info", "72x72"),
    (NotifyType::Info, Xy128) => png!("info", "128x128"),
    (NotifyType::Info, Xy256) => png!("info", "256x256"),
    (NotifyType::Success, Xy32) => png!("success", "32x32"),
    (NotifyType::Success, Xy72) => png!("success", "72x72"),
    (NotifyType::Success, Xy128) => png!("success", "128x128"),
    (NotifyType::Success, Xy256) => png!("success", "256x256"),
    (NotifyType::Warning, Xy32) => png!("warning", "32x32"),
    (NotifyType::Warning, Xy72) => png!("warning", "72x72"),
    (NotifyType::Warning, Xy128) => png!("warning", "128x128"),
    (NotifyType::Warning, Xy256) => png!("warning", "256x256"),
    (NotifyType::Failure, Xy32) => png!("failure", "32x32"),
    (NotifyType::Failure, Xy72) => png!("failure", "72x72"),
    (NotifyType::Failure, Xy128) => png!("failure", "128x128"),
    (NotifyType::Failure, Xy256) => png!("failure", "256x256"),
  })
}

#[cfg(not(feature = "images"))]
fn bundled_image(_notify_type: &NotifyType, _image_size: NotifyImageSize) -> Option<&'static [u8]> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_asset() {
    let asset = AppriseAsset::default();
    assert_eq!(asset.app_id, "Apprise");
    assert_eq!(asset.app_desc, "Apprise Notifications");
    assert_eq!(asset.app_url, "https://github.com/caronc/apprise");
    assert_eq!(asset.image_url_mask.as_deref(), Some(DEFAULT_IMAGE_URL_MASK));
    assert_eq!(asset.image_url_logo.as_deref(), Some(DEFAULT_IMAGE_URL_LOGO));
    assert_eq!(asset.theme, "default");
    assert!(asset.body_format.is_none());
    assert!(asset.secure_logging);
    assert!(asset.async_mode);
    assert_eq!(asset.default_service_wait, 0.5);
    assert_eq!(asset.service_timeout(), Some(std::time::Duration::from_secs(60)));
    assert_eq!(asset.storage_idlen, 8);
    assert!(asset.timezone.is_none());
    assert!(asset.pgp_autogen && asset.pem_autogen);
  }

  // Values checked against upstream's AppriseAsset.
  #[test]
  fn test_image_url() {
    let asset = AppriseAsset::default();
    assert_eq!(
      asset.image_url(&NotifyType::Info, None, false, None).as_deref(),
      Some("https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png")
    );
    assert_eq!(
      asset.image_url(&NotifyType::Failure, Some(NotifyImageSize::Xy72), false, Some(".jpeg")).as_deref(),
      Some("https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-72x72.jpeg")
    );
    assert_eq!(
      asset.image_url(&NotifyType::Info, None, true, None).as_deref(),
      Some("https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-logo.png")
    );
    let custom = AppriseAsset { image_url_mask: Some("http://x/{theme}/{Type}-{xy}{EXTENSION}".into()), theme: "dark".into(), ..Default::default() };
    assert_eq!(custom.image_url(&NotifyType::Warning, Some(NotifyImageSize::Xy32), false, None).as_deref(), Some("http://x/dark/warning-32x32.png"));
    let none = AppriseAsset { image_url_mask: None, image_url_logo: Some(String::new()), ..Default::default() };
    assert_eq!(none.image_url(&NotifyType::Info, None, false, None), None);
    assert_eq!(none.image_url(&NotifyType::Info, None, true, None), None);
  }

  #[test]
  fn test_image_path_and_raw() {
    let asset = AppriseAsset::default();
    assert_eq!(asset.image_path(&NotifyType::Info, NotifyImageSize::Xy72, true, None), None);
    #[cfg(feature = "images")]
    {
      let raw = asset.image_raw(&NotifyType::Success, NotifyImageSize::Xy72, None).unwrap();
      assert_eq!(&raw[1..4], b"PNG");
    }
    assert!(asset.image_raw(&NotifyType::Success, NotifyImageSize::Xy72, Some(".ico")).is_none());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("info-32x32.png"), b"img").unwrap();
    let mask = format!("{}/{{TYPE}}-{{XY}}{{EXTENSION}}", dir.path().display());
    let disk = AppriseAsset { image_path_mask: Some(mask), ..Default::default() };
    assert_eq!(disk.image_path(&NotifyType::Info, NotifyImageSize::Xy32, true, None), Some(dir.path().join("info-32x32.png")));
    assert_eq!(disk.image_path(&NotifyType::Failure, NotifyImageSize::Xy32, true, None), None);
    assert!(disk.image_path(&NotifyType::Failure, NotifyImageSize::Xy32, false, None).is_some());
    assert_eq!(disk.image_raw(&NotifyType::Info, NotifyImageSize::Xy32, None).as_deref(), Some(b"img".as_slice()));
  }

  #[test]
  fn test_color_and_ascii() {
    let asset = AppriseAsset::default();
    assert_eq!(asset.color(&NotifyType::Info), "#3AA3E3");
    assert_eq!(asset.color_int(&NotifyType::Failure), 0xA32037);
    assert_eq!(asset.color_rgb(&NotifyType::Success), (0x3A, 0xA3, 0x37));
    assert_eq!(asset.ascii(&NotifyType::Warning), "[~]");
  }

  #[test]
  fn test_service_timeout_zero_disables() {
    let asset = AppriseAsset { service_timeout: 0.0, ..Default::default() };
    assert_eq!(asset.service_timeout(), None);
  }

  #[test]
  fn test_custom_asset() {
    let asset = AppriseAsset {
      app_id: "MyApp".to_string(),
      app_desc: "My Application".to_string(),
      app_url: "https://example.com".to_string(),
      image_url_mask: Some("https://example.com/mask.png".to_string()),
      image_url_logo: Some("https://example.com/logo.png".to_string()),
      theme: "dark".to_string(),
      body_format: Some("html".to_string()),
      secure_logging: false,
      ..Default::default()
    };
    assert_eq!(asset.app_id, "MyApp");
    assert!(asset.image_url_mask.is_some());
    assert_eq!(asset.theme, "dark");
    assert_eq!(asset.body_format, Some("html".to_string()));
    assert!(!asset.secure_logging);
  }
}

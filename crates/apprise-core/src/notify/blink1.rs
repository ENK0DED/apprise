use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

/// USB vendor and product IDs shared by every blink(1) revision.
#[cfg_attr(not(feature = "blink1"), allow(dead_code))]
const VENDOR_ID: u16 = 0x27b8;
#[cfg_attr(not(feature = "blink1"), allow(dead_code))]
const PRODUCT_ID: u16 = 0x01ed;

/// Feature report ID and the "fade to RGB" command byte (`c`).
const REPORT_ID: u8 = 0x01;
const CMD_FADE: u8 = b'c';

/// How long (ms) the colour is held after the fade, at most 5 minutes.
const DEFAULT_DURATION_MS: u32 = 5000;
const MAX_DURATION_MS: u32 = 300_000;

/// Fade transition time (ms), at most 10 seconds; 0 is instant.
const DEFAULT_FADE_MS: u32 = 0;
const MAX_FADE_MS: u32 = 10_000;

/// blink(1) USB LED: lights up in the notification type's colour, holds it,
/// then switches off.
///
/// The USB HID I/O needs the `blink1` Cargo feature (off by default). Without
/// it, URLs still load but sending fails with an explanation.
pub struct Blink1 {
  /// Device serial number; `None` uses the first device found
  #[cfg_attr(not(feature = "blink1"), allow(dead_code))]
  serial: Option<String>,
  duration_ms: u32,
  fade_ms: u32,
  /// 0 for all LEDs, 1 or 2 for one of them
  ledn: u8,
  tags: Vec<String>,
}

impl Blink1 {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // An underscore (or nothing) means "the first device found"
    let serial = url.host.as_deref().map(unquote).map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "_");

    let duration_ms = match url.get("duration").filter(|v| !v.is_empty()) {
      None => DEFAULT_DURATION_MS,
      Some(v) => match parse_ms(v, MAX_DURATION_MS) {
        Some(ms) => ms,
        None => {
          tracing::warn!("blink(1) duration ({}) must be between 0 and {} ms.", v, MAX_DURATION_MS);
          return None;
        }
      },
    };
    let fade_ms = match url.get("fade").filter(|v| !v.is_empty()) {
      None => DEFAULT_FADE_MS,
      Some(v) => match parse_ms(v, MAX_FADE_MS) {
        Some(ms) => ms,
        None => {
          tracing::warn!("blink(1) fade ({}) must be between 0 and {} ms.", v, MAX_FADE_MS);
          return None;
        }
      },
    };

    // Unrecognised LED selectors fall back to all LEDs
    let ledn = match url.get("ledn").filter(|v| !v.is_empty()).map(str::to_lowercase).as_deref() {
      Some("1") => 1,
      Some("2") => 2,
      _ => 0,
    };

    Some(Self { serial, duration_ms, fade_ms, ledn, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "blink(1)",
      service_url: Some("https://blink1.thingm.com/"),
      setup_url: Some("https://appriseit.com/services/blink1/"),
      protocols: vec!["blink1"],
      description: "Light a blink(1) USB LED in the notification type's colour.",
      attachment_support: false,
    }
  }

  #[cfg_attr(not(feature = "blink1"), allow(dead_code))]
  /// The 9-byte feature report that fades the LED(s) to an RGB colour.
  ///
  /// `[report id, 'c', r, g, b, fade high, fade low, ledn, 0]`, where the
  /// fade time is in units of 10 ms, big-endian.
  fn fade_report(&self, (r, g, b): (u8, u8, u8), fade_ms: u32) -> [u8; 9] {
    let [th, tl] = ((fade_ms / 10) as u16).to_be_bytes();
    [REPORT_ID, CMD_FADE, r, g, b, th, tl, self.ledn, 0x00]
  }

  #[cfg_attr(not(feature = "blink1"), allow(dead_code))]
  /// Fade to the notification colour, hold for fade + duration, then
  /// switch off instantly. False (after logging why) if a write fails.
  async fn flash<D: FeatureReports>(&self, device: D, notify_type: &NotifyType) -> bool {
    let color = notify_type.color();
    let on = self.fade_report(((color >> 16) as u8, (color >> 8) as u8, color as u8), self.fade_ms);
    let off = self.fade_report((0, 0, 0), 0);

    let Some(device) = write_report(device, on).await else { return false };
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(self.fade_ms) + u64::from(self.duration_ms))).await;
    // The device closes when dropped
    write_report(device, off).await.is_some()
  }
}

#[cfg_attr(not(feature = "blink1"), allow(dead_code))]
/// Something that takes HID feature reports: the blink(1), or a fake in tests.
trait FeatureReports: Send + 'static {
  fn send_feature_report(&mut self, report: &[u8]) -> Result<(), String>;
}

#[cfg_attr(not(feature = "blink1"), allow(dead_code))]
/// Write one report off the async runtime, handing the device back on success.
async fn write_report<D: FeatureReports>(mut device: D, report: [u8; 9]) -> Option<D> {
  match tokio::task::spawn_blocking(move || device.send_feature_report(&report).map(|_| device)).await {
    Ok(Ok(device)) => Some(device),
    Ok(Err(e)) => {
      tracing::warn!("blink(1) HID write failed: {}", e);
      None
    }
    Err(e) => {
      tracing::warn!("blink(1) HID write failed: {}", e);
      None
    }
  }
}

#[cfg(feature = "blink1")]
impl FeatureReports for hidapi::HidDevice {
  fn send_feature_report(&mut self, report: &[u8]) -> Result<(), String> {
    hidapi::HidDevice::send_feature_report(self, report).map_err(|e| e.to_string())
  }
}

/// Open the blink(1) with this serial number, or the first one found.
#[cfg(feature = "blink1")]
fn open_device(serial: Option<&str>) -> Result<hidapi::HidDevice, String> {
  let api = hidapi::HidApi::new().map_err(|e| e.to_string())?;
  match serial {
    Some(sn) => api.open_serial(VENDOR_ID, PRODUCT_ID, sn),
    None => api.open(VENDOR_ID, PRODUCT_ID),
  }
  .map_err(|e| e.to_string())
}

/// A whole number of milliseconds between 0 and `max`.
fn parse_ms(value: &str, max: u32) -> Option<u32> {
  value.trim().parse::<i64>().ok().filter(|ms| (0..=i64::from(max)).contains(ms)).map(|ms| ms as u32)
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

#[async_trait]
impl Notify for Blink1 {
  fn schemas(&self) -> &[&str] {
    &["blink1"]
  }
  fn service_name(&self) -> &str {
    "blink(1)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // A light has no use for a title
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    #[cfg(feature = "blink1")]
    {
      let serial = self.serial.clone();
      let device = match tokio::task::spawn_blocking(move || open_device(serial.as_deref())).await {
        Ok(Ok(device)) => device,
        Ok(Err(e)) => {
          let serial = self.serial.as_ref().map(|s| format!(" (serial={})", s)).unwrap_or_default();
          tracing::warn!("Failed to open blink(1) device{}: {}", serial, e);
          return Ok(false);
        }
        Err(e) => return Err(NotifyError::Other(e.to_string())),
      };
      let ok = self.flash(device, &ctx.notify_type).await;
      if ok {
        tracing::info!("Sent blink(1) notification.");
      }
      Ok(ok)
    }
    #[cfg(not(feature = "blink1"))]
    {
      let _ = ctx;
      tracing::warn!("blink(1) support is not compiled in; build with --features blink1.");
      Err(NotifyError::Other("blink(1) support is not compiled in; build with --features blink1".into()))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use std::sync::{Arc, Mutex};

  fn parse(url: &str) -> Option<Blink1> {
    Blink1::from_url(&ParsedUrl::parse(url)?)
  }

  /// Records every report; fails the write numbered `fail_at` (0-based).
  #[derive(Clone, Default)]
  struct FakeDevice {
    reports: Arc<Mutex<Vec<Vec<u8>>>>,
    fail_at: Option<usize>,
  }

  impl FeatureReports for FakeDevice {
    fn send_feature_report(&mut self, report: &[u8]) -> Result<(), String> {
      let mut reports = self.reports.lock().unwrap();
      if self.fail_at == Some(reports.len()) {
        return Err("short write".into());
      }
      reports.push(report.to_vec());
      Ok(())
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "blink1://".to_string(),
      "blink1://_/".to_string(),
      "blink1://ABCD1234/".to_string(),
      "blink1://?duration=0&fade=0&ledn=0".to_string(),
      "blink1://?ledn=1".to_string(),
      "blink1://?ledn=2".to_string(),
      "blink1://?ledn=bogus".to_string(),
      format!("blink1://?duration={}", MAX_DURATION_MS),
      format!("blink1://?fade={}", MAX_FADE_MS),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "blink1://?duration=abc".to_string(),
      "blink1://?duration=-1".to_string(),
      format!("blink1://?duration={}", MAX_DURATION_MS + 1),
      "blink1://?fade=abc".to_string(),
      "blink1://?fade=-1".to_string(),
      format!("blink1://?fade={}", MAX_FADE_MS + 1),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse("blink1://").unwrap();
    assert_eq!((obj.serial.as_deref(), obj.duration_ms, obj.fade_ms, obj.ledn), (None, 5000, 0, 0));
    assert!(parse("blink1://_/").unwrap().serial.is_none());
    assert_eq!(parse("blink1://DEADBEEF/").unwrap().serial.as_deref(), Some("DEADBEEF"));

    let obj = parse("blink1://?fade=200&duration=100&ledn=2").unwrap();
    assert_eq!((obj.duration_ms, obj.fade_ms, obj.ledn), (100, 200, 2));
    for (ledn, expected) in [("0", 0), ("all", 0), ("A", 0), ("1", 1), ("2", 2), ("3", 0), ("bogus", 0)] {
      assert_eq!(parse(&format!("blink1://?ledn={}", ledn)).unwrap().ledn, expected, "{}", ledn);
    }
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url("blink1://").unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.request_rate_per_sec(), 0.0);
    assert!(!svc.attachment_support());
    assert_eq!(Blink1::static_details().service_name, "blink(1)");
  }

  #[test]
  fn test_fade_report_layout() {
    let obj = parse("blink1://?ledn=1").unwrap();
    assert_eq!(obj.fade_report((0x11, 0x22, 0x33), 2560), [0x01, 0x63, 0x11, 0x22, 0x33, 0x01, 0x00, 0x01, 0x00]);
    assert_eq!(obj.fade_report((0, 0, 0), 0), [0x01, 0x63, 0, 0, 0, 0, 0, 0x01, 0]);
    // Fade time is in units of 10 ms
    assert_eq!(obj.fade_report((0, 0, 0), 10_000)[5..7], [0x03, 0xe8]);
  }

  #[tokio::test]
  async fn test_flash_writes_colour_then_off() {
    let obj = parse("blink1://?fade=0&duration=0&ledn=2").unwrap();
    for (notify_type, rgb) in [
      (NotifyType::Info, [0x3a, 0xa3, 0xe3]),
      (NotifyType::Success, [0x3a, 0xa3, 0x37]),
      (NotifyType::Warning, [0xca, 0xcf, 0x29]),
      (NotifyType::Failure, [0xa3, 0x20, 0x37]),
    ] {
      let device = FakeDevice::default();
      assert!(obj.flash(device.clone(), &notify_type).await);
      let reports = device.reports.lock().unwrap().clone();
      assert_eq!(reports, vec![vec![0x01, 0x63, rgb[0], rgb[1], rgb[2], 0, 0, 2, 0], vec![0x01, 0x63, 0, 0, 0, 0, 0, 2, 0]]);
    }
  }

  #[tokio::test]
  async fn test_flash_holds_for_fade_and_duration() {
    let obj = parse("blink1://?fade=30&duration=50").unwrap();
    let device = FakeDevice::default();
    let started = std::time::Instant::now();
    assert!(obj.flash(device.clone(), &NotifyType::Info).await);
    assert!(started.elapsed() >= std::time::Duration::from_millis(80));
    assert_eq!(device.reports.lock().unwrap()[0][5..7], [0x00, 0x03]);
  }

  #[tokio::test]
  async fn test_flash_write_failures() {
    let obj = parse("blink1://?duration=0").unwrap();
    // The colour write fails: nothing else is written
    let device = FakeDevice { fail_at: Some(0), ..Default::default() };
    assert!(!obj.flash(device.clone(), &NotifyType::Info).await);
    assert!(device.reports.lock().unwrap().is_empty());
    // The off write fails
    let device = FakeDevice { fail_at: Some(1), ..Default::default() };
    assert!(!obj.flash(device.clone(), &NotifyType::Info).await);
    assert_eq!(device.reports.lock().unwrap().len(), 1);
  }

  #[cfg(not(feature = "blink1"))]
  #[tokio::test]
  async fn test_send_without_feature() {
    let svc = from_url("blink1://").unwrap();
    assert!(svc.send(&NotifyContext::default()).await.is_err());
  }
}

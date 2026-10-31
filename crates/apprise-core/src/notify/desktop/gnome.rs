//! Gnome notifications (`gnome://`): the freedesktop service the way
//! upstream's libnotify bindings use it. A libnotify notification has no
//! separate title, so the title is folded into the text.

use async_trait::async_trait;

use super::bus::{self, Hint};
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

/// libnotify's NOTIFY_EXPIRES_DEFAULT: the server picks the duration.
const EXPIRES_DEFAULT: i32 = -1;

pub struct Gnome {
  urgency: u8,
  include_image: bool,
  tags: Vec<String>,
}

impl Gnome {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Some(Self { urgency: super::parse_urgency(url), include_image: super::include_image(url), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Gnome Notification",
      service_url: Some("https://www.gnome.org/"),
      setup_url: Some("https://appriseit.com/services/gnome/"),
      protocols: vec!["gnome"],
      description: "Show a Gnome desktop notification.",
      attachment_support: false,
    }
  }

  /// The call libnotify makes for `Notify.Notification.new(body)`.
  fn notification(&self, ctx: &NotifyContext) -> bus::Notification {
    let mut hints = vec![("urgency", Hint::Byte(self.urgency))];
    if self.include_image
      && let Some(image) = bus::icon(ctx)
    {
      hints.push(("image-data", Hint::Image(image)));
    }
    hints.push(("sender-pid", Hint::Int64(std::process::id().into())));
    bus::Notification { app_name: ctx.asset.app_id.clone(), summary: ctx.body.clone(), body: String::new(), hints, expire_timeout: EXPIRES_DEFAULT }
  }
}

#[async_trait]
impl Notify for Gnome {
  fn schemas(&self) -> &[&str] {
    &["gnome"]
  }
  fn service_name(&self) -> &str {
    "Gnome Notification"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_max_line_count(&self) -> usize {
    10
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    crate::notify::throttle().await;
    match bus::send(&self.notification(ctx)).await {
      Ok(_) => {
        tracing::info!("Sent Gnome notification.");
        Ok(true)
      }
      Err(e) => {
        tracing::warn!("Failed to send Gnome notification.");
        tracing::debug!("Gnome Exception: {}", e);
        Err(e)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(url: &str) -> Gnome {
    Gnome::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    assert_eq!(parse("gnome://").urgency, 1);
    assert_eq!(parse("gnome://_/?urgency=high").urgency, 2);
    assert_eq!(parse("gnome://?priority=0").urgency, 0);
    assert!(parse("gnome://").include_image);
    assert!(!parse("gnome://?image=no").include_image);
  }

  #[test]
  fn test_notification() {
    let ctx = NotifyContext { body: "Title\r\nBody".into(), ..Default::default() };
    let n = parse("gnome://?urgency=high").notification(&ctx);
    assert_eq!((n.app_name.as_str(), n.summary.as_str(), n.body.as_str(), n.expire_timeout), ("Apprise", "Title\r\nBody", "", -1));
    assert_eq!(n.hints[0], ("urgency", Hint::Byte(2)));
    assert!(matches!(&n.hints[1], ("image-data", Hint::Image(i)) if i.width == 128));
    assert_eq!(n.hints[2], ("sender-pid", Hint::Int64(std::process::id().into())));

    let n = parse("gnome://?image=no").notification(&ctx);
    assert_eq!(n.hints.len(), 2);
  }

  #[test]
  fn test_custom_asset_icon() {
    // Upstream loads the asset's image_path(notify_type, extension=".ico")
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("info.png"), bus::asset_icon_tests::png_file(3, 2)).unwrap();
    let asset = crate::asset::AppriseAsset { image_path_mask: Some(format!("{}/{{TYPE}}.png", dir.path().display())), ..Default::default() };
    let n = parse("gnome://").notification(&NotifyContext { body: "B".into(), asset, ..Default::default() });
    assert!(matches!(&n.hints[1], ("image-data", Hint::Image(i)) if (i.width, i.height) == (3, 2)), "{:?}", n.hints[1]);
  }
}

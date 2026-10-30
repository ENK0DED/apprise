//! GLib notifications (`glib://`, `gio://`): the freedesktop service as
//! upstream reaches it through a Gio D-Bus proxy. The call is the same one
//! dbus:// makes.

use async_trait::async_trait;

use super::bus;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

const SCHEMAS: &[&str] = &["glib", "gio"];

pub struct GLib {
  urgency: u8,
  position: Option<(i32, i32)>,
  include_image: bool,
  tags: Vec<String>,
}

impl GLib {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    if !SCHEMAS.contains(&url.schema.as_str()) {
      tracing::warn!("The schema specified ({}) is not supported.", url.schema);
      return None;
    }
    Some(Self { urgency: super::parse_urgency(url), position: super::parse_position(url).ok()?, include_image: super::include_image(url), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "GLib Notification",
      service_url: Some("https://lazka.github.io/pgi-docs/Gio-2.0/classes/DBusProxy.html"),
      setup_url: Some("https://appriseit.com/services/dbus/"),
      protocols: SCHEMAS.to_vec(),
      description: "Show a desktop notification through GLib/Gio.",
      attachment_support: false,
    }
  }

  fn notification(&self, ctx: &NotifyContext) -> bus::Notification {
    bus::freedesktop_notification(ctx, self.urgency, self.position, self.include_image)
  }
}

#[async_trait]
impl Notify for GLib {
  fn schemas(&self) -> &[&str] {
    SCHEMAS
  }
  fn service_name(&self) -> &str {
    "GLib Notification"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
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
        tracing::info!("Sent GLib/Gio notification.");
        Ok(true)
      }
      Err(e) => {
        tracing::warn!("Failed to send GLib/Gio notification.");
        tracing::debug!("GLib/Gio Exception: {}", e);
        Err(e)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::desktop::bus::{Hint, Notification};
  use crate::types::NotifyType;

  fn parse(url: &str) -> Option<GLib> {
    ParsedUrl::parse(url).and_then(|p| GLib::from_url(&p))
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_urls() {
    for url in ["glib://", "glib://_/", "gio://", "glib://?image=yes", "glib://?urgency=high", "gio://?priority=low", "glib://_/?x=0&y=0"] {
      assert!(parse(url).is_some(), "should load: {}", url);
    }
    for url in ["glib://?x=invalid&y=1", "glib://?y=5", "dbus://"] {
      assert!(parse(url).is_none(), "should not load: {}", url);
    }
    let d = parse("glib://?urgency=high").unwrap();
    assert_eq!(d.urgency, 2);
    assert_eq!(parse("gio://?priority=low").unwrap().urgency, 0);
    assert_eq!(parse("glib://?x=100&y=200").unwrap().position, Some((100, 200)));
    assert!(parse("glib://").unwrap().include_image);
    assert!(!parse("glib://?image=no").unwrap().include_image);
  }

  #[test]
  fn test_notification() {
    let d = parse("gio://?x=100&y=200&urgency=high").unwrap();
    let n = d.notification(&ctx("T", "B"));
    assert_eq!(n.app_name, "Apprise");
    assert_eq!((n.summary.as_str(), n.body.as_str(), n.expire_timeout), ("T", "B", 13000));
    assert_eq!(&n.hints[..3], &[("urgency", Hint::Byte(2)), ("x", Hint::Int(100)), ("y", Hint::Int(200))]);
    let (name, Hint::Image(image)) = &n.hints[3] else { panic!("no icon: {:?}", n.hints[3].0) };
    assert_eq!(*name, "icon_data");
    assert_eq!((image.width, image.height), (128, 128));

    // Without a title the body is shown as the summary; no image when off
    let d = parse("glib://?image=no").unwrap();
    let n = d.notification(&NotifyContext { notify_type: NotifyType::Failure, ..ctx("", "B") });
    assert_eq!(
      n,
      Notification { app_name: "Apprise".into(), summary: "B".into(), body: String::new(), hints: vec![("urgency", Hint::Byte(1))], expire_timeout: 13000 }
    );

    // A zero coordinate is still sent
    let n = parse("glib://_/?x=0&y=0&image=no").unwrap().notification(&ctx("T", "B"));
    assert_eq!(&n.hints[1..], &[("x", Hint::Int(0)), ("y", Hint::Int(0))]);
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn test_wire() {
    let n = parse("glib://_/?x=0&y=5&image=no&urgency=low").unwrap().notification(&ctx("Title", "Body"));
    let (_, (app, _, _, summary, body, _, hints, timeout)) = crate::notify::desktop::bus::tests::capture(&n).await;
    assert_eq!((app.as_str(), summary.as_str(), body.as_str(), timeout), ("Apprise", "Title", "Body", 13000));
    assert_eq!(u8::try_from(&*hints["urgency"]).unwrap(), 0);
    assert_eq!(i32::try_from(&*hints["x"]).unwrap(), 0);
    assert_eq!(i32::try_from(&*hints["y"]).unwrap(), 5);
    assert!(!hints.contains_key("icon_data"));
  }

  #[test]
  fn test_static_details() {
    assert_eq!(GLib::static_details().protocols, vec!["glib", "gio"]);
  }
}

//! The freedesktop notification service, which dbus://, glib:// and
//! gnome:// all reach over the D-Bus session bus.
//! See https://specifications.freedesktop.org/notification-spec/latest/

use crate::error::NotifyError;
use crate::notify::NotifyContext;
use crate::types::NotifyType;

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// How long dbus:// and glib:// notifications stay up.
const MESSAGE_TIMEOUT_MS: i32 = 13000;

/// libdbus's default reply timeout, which upstream's bindings use.
#[cfg(unix)]
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// One `Notify` call. The replaced id (0), icon name ("") and actions (none)
/// never change.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Notification {
  pub app_name: String,
  pub summary: String,
  pub body: String,
  pub hints: Vec<(&'static str, Hint)>,
  /// Milliseconds; -1 leaves it to the server.
  pub expire_timeout: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Hint {
  Byte(u8),
  Int(i32),
  Int64(i64),
  Image(Image),
}

/// Raw pixels, sent as the `(iiibiiay)` structure GdkPixbuf describes.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Image {
  pub width: i32,
  pub height: i32,
  pub rowstride: i32,
  pub has_alpha: bool,
  pub bits_per_sample: i32,
  pub channels: i32,
  pub data: Vec<u8>,
}

/// How dbus:// and glib:// build their call (upstream dbus.py and glib.py
/// send the same one).
pub(super) fn freedesktop_notification(ctx: &NotifyContext, urgency: u8, position: Option<(i32, i32)>, include_image: bool) -> Notification {
  // Without a title, the body becomes the summary to avoid an empty line
  let (summary, body) = if ctx.title.is_empty() { (ctx.body.clone(), String::new()) } else { (ctx.title.clone(), ctx.body.clone()) };
  let mut hints = vec![("urgency", Hint::Byte(urgency))];
  if let Some((x, y)) = position {
    hints.push(("x", Hint::Int(x)));
    hints.push(("y", Hint::Int(y)));
  }
  if include_image && let Some(image) = icon(&ctx.notify_type) {
    hints.push(("icon_data", Hint::Image(image)));
  }
  Notification { app_name: ctx.asset.app_id.clone(), summary, body, hints, expire_timeout: MESSAGE_TIMEOUT_MS }
}

/// The icon for `notify_type` as raw pixels. A failure is logged and the
/// notification goes out without one, as upstream does.
pub(super) fn icon(notify_type: &NotifyType) -> Option<Image> {
  let decode = || -> Result<Image, String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(super::image_png(notify_type)));
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut data = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
    data.truncate(info.buffer_size());
    let channels = match (info.color_type, info.bit_depth) {
      (png::ColorType::Rgba, png::BitDepth::Eight) => 4,
      (png::ColorType::Rgb, png::BitDepth::Eight) => 3,
      other => return Err(format!("unsupported pixel format {:?}", other)),
    };
    Ok(Image {
      width: info.width as i32,
      height: info.height as i32,
      rowstride: info.line_size as i32,
      has_alpha: channels == 4,
      bits_per_sample: 8,
      channels,
      data,
    })
  };
  decode().map_err(|e| tracing::warn!("Could not load notification icon ({}): {}", notify_type.as_str(), e)).ok()
}

/// Show `notification` through the session bus, returning its id.
#[cfg(unix)]
pub(super) async fn send(notification: &Notification) -> Result<u32, NotifyError> {
  let call = async {
    let conn = zbus::Connection::session().await?;
    call(&conn, notification).await
  };
  match tokio::time::timeout(CALL_TIMEOUT, call).await {
    Ok(result) => result.map_err(|e| NotifyError::Other(format!("D-Bus: {}", e))),
    Err(_) => Err(NotifyError::Other("D-Bus: timed out waiting for the notification service".into())),
  }
}

#[cfg(not(unix))]
pub(super) async fn send(_notification: &Notification) -> Result<u32, NotifyError> {
  Err(NotifyError::NotSupported)
}

#[cfg(unix)]
async fn call(conn: &zbus::Connection, n: &Notification) -> zbus::Result<u32> {
  let hints: std::collections::HashMap<&str, zbus::Variant<'_>> = n
    .hints
    .iter()
    .map(|(name, hint)| {
      let value = match hint {
        Hint::Byte(v) => zbus::Variant::U8(*v),
        Hint::Int(v) => zbus::Variant::I32(*v),
        Hint::Int64(v) => zbus::Variant::I64(*v),
        Hint::Image(i) => zbus::Variant::from((i.width, i.height, i.rowstride, i.has_alpha, i.bits_per_sample, i.channels, i.data.clone())),
      };
      (*name, value)
    })
    .collect();
  let body = (n.app_name.as_str(), 0u32, "", n.summary.as_str(), n.body.as_str(), Vec::<&str>::new(), hints, n.expire_timeout);
  let reply = conn.call_method(Some(DESTINATION), PATH, Some(DESTINATION), "Notify", &body).await?;
  reply.body().deserialize()
}

#[cfg(all(test, unix))]
pub(super) mod tests {
  use super::*;
  use std::collections::HashMap;
  use zbus::OwnedVariant;

  /// What the notification service received for one `Notify` call.
  pub type Received = (String, u32, String, String, String, Vec<String>, HashMap<String, OwnedVariant>, i32);

  /// Make the `Notify` call against a fake service over a socket pair.
  pub async fn capture(notification: &Notification) -> (String, Received) {
    use zbus::export::futures_core::Stream;

    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let server = zbus::connection::Builder::unix_stream(a).server(zbus::Guid::generate()).p2p().build();
    let client = zbus::connection::Builder::unix_stream(b).p2p().build();
    let (server, client) = tokio::try_join!(server, client).unwrap();

    let service = async {
      let mut stream = zbus::MessageStream::from(&server);
      let msg = std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream).poll_next(cx)).await.unwrap().unwrap();
      let header = msg.header();
      let target = format!("{} {} {}", header.path().unwrap(), header.interface().unwrap(), header.member().unwrap());
      let body: Received = msg.body().deserialize().unwrap();
      server.reply(&header, &(42u32,)).await.unwrap();
      (target, body)
    };
    let (received, id) = tokio::join!(service, call(&client, notification));
    assert_eq!(id.unwrap(), 42);
    received
  }

  #[tokio::test]
  async fn test_wire_format() {
    let notification = Notification {
      app_name: "Apprise".into(),
      summary: "T".into(),
      body: "B".into(),
      hints: vec![
        ("urgency", Hint::Byte(2)),
        ("x", Hint::Int(0)),
        ("sender-pid", Hint::Int64(7)),
        ("icon_data", Hint::Image(icon(&NotifyType::Info).unwrap())),
      ],
      expire_timeout: 13000,
    };
    let (target, (app, id, icon_name, summary, body, actions, hints, timeout)) = capture(&notification).await;
    assert_eq!(target, "/org/freedesktop/Notifications org.freedesktop.Notifications Notify");
    assert_eq!((app.as_str(), id, icon_name.as_str(), summary.as_str(), body.as_str(), actions.len(), timeout), ("Apprise", 0, "", "T", "B", 0, 13000));
    assert_eq!(u8::try_from(&*hints["urgency"]).unwrap(), 2);
    assert_eq!(i32::try_from(&*hints["x"]).unwrap(), 0);
    assert_eq!(i64::try_from(&*hints["sender-pid"]).unwrap(), 7);
    let image = hints["icon_data"].try_clone().unwrap();
    let (w, h, stride, alpha, bits, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) = image.try_into().unwrap();
    assert_eq!((w, h, stride, alpha, bits, channels, data.len()), (128, 128, 512, true, 8, 4, 128 * 512));
  }
}

#[cfg(test)]
mod icon_tests {
  use super::*;

  #[test]
  fn test_icon() {
    for notify_type in [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure] {
      let image = icon(&notify_type).unwrap();
      assert_eq!((image.width, image.height, image.rowstride, image.has_alpha, image.bits_per_sample, image.channels), (128, 128, 512, true, 8, 4));
      assert_eq!(image.data.len(), 128 * 128 * 4);
    }
  }
}

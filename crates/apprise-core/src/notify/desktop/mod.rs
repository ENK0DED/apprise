//! Desktop notifications.
//!
//! - `dbus://`, `kde://`, `qt://`: the freedesktop notification service over
//!   D-Bus (upstream `dbus.py`).
//! - `glib://`, `gio://`: the same service as upstream reaches it through
//!   GLib/Gio (`glib.py`).
//! - `gnome://`: the same service the way libnotify uses it (`gnome.py`).
//! - `macosx://`: macOS through terminal-notifier (`macosx.py`).
//! - `windows://`: a Windows taskbar balloon (`windows.py`).
//!
//! Each is only registered on the platforms where it can work.

#[cfg(any(unix, test))]
mod bus;
#[cfg(any(unix, test))]
pub mod dbus;
#[cfg(any(unix, test))]
pub mod glib;
#[cfg(any(unix, test))]
pub mod gnome;
#[cfg(any(target_os = "macos", test))]
pub mod macosx;
#[cfg(any(windows, test))]
pub mod windows;

use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

/// The 128x128 icons every desktop plugin uses (upstream `NotifyImageSize.XY_128`).
const IMAGE_SIZE: &str = "128x128";

/// The default theme's icons, kept in the binary so no files need installing.
fn image_png(notify_type: &NotifyType) -> &'static [u8] {
  match notify_type {
    NotifyType::Info => include_bytes!("assets/apprise-info-128x128.png"),
    NotifyType::Success => include_bytes!("assets/apprise-success-128x128.png"),
    NotifyType::Warning => include_bytes!("assets/apprise-warning-128x128.png"),
    NotifyType::Failure => include_bytes!("assets/apprise-failure-128x128.png"),
  }
}

/// A local copy of the icon (upstream `image_path()`), written to the cache
/// directory the first time it is needed.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn image_path(notify_type: &NotifyType) -> Option<std::path::PathBuf> {
  let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("apprise").join("themes").join("default");
  let path = dir.join(format!("apprise-{}-{}.png", notify_type.as_str(), IMAGE_SIZE));
  let data = image_png(notify_type);
  if std::fs::metadata(&path).is_ok_and(|m| m.len() == data.len() as u64) {
    return Some(path);
  }
  // Write next to the target first so a reader never sees half a file
  let partial = dir.join(format!(".{}-apprise-{}-{}.png", std::process::id(), notify_type.as_str(), IMAGE_SIZE));
  let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&partial, data)).and_then(|_| std::fs::rename(&partial, &path));
  match written {
    Ok(()) => Some(path),
    Err(e) => {
      tracing::warn!("Could not write the notification icon ({}): {}", path.display(), e);
      None
    }
  }
}

/// `?urgency=` (or its `?priority=` alias) as 0 (low), 1 (normal) or 2 (high).
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
fn parse_urgency(url: &ParsedUrl) -> u8 {
  const NORMAL: u8 = 1;
  // urgency= wins over priority=
  let Some(value) = url.get("urgency").filter(|v| !v.is_empty()).or_else(|| url.get("priority").filter(|v| !v.is_empty())) else {
    return NORMAL;
  };
  // Matched by prefix, in upstream's order: low, moderate, normal, high, emergency
  let value = value.to_lowercase();
  [("l", 0), ("m", 0), ("n", 1), ("h", 2), ("e", 2), ("0", 0), ("1", 1), ("2", 2)].iter().find(|(k, _)| value.starts_with(k)).map_or(NORMAL, |(_, v)| *v)
}

/// The `?x=`/`?y=` screen position. Both are needed once either is given;
/// `Err` means they could not be read.
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
fn parse_position(url: &ParsedUrl) -> Result<Option<(i32, i32)>, ()> {
  let x = url.get("x").filter(|v| !v.is_empty());
  let y = url.get("y").filter(|v| !v.is_empty());
  if x.is_none() && y.is_none() {
    return Ok(None);
  }
  match (x.and_then(|v| v.trim().parse().ok()), y.and_then(|v| v.trim().parse().ok())) {
    (Some(x), Some(y)) => Ok(Some((x, y))),
    _ => {
      tracing::warn!("The x,y coordinates specified ({},{}) are invalid.", x.unwrap_or("None"), y.unwrap_or("None"));
      Err(())
    }
  }
}

/// `?image=`, on unless turned off.
fn include_image(url: &ParsedUrl) -> bool {
  url.get("image").is_none_or(crate::utils::parse::parse_bool)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn url(s: &str) -> ParsedUrl {
    ParsedUrl::parse(s).unwrap()
  }

  #[test]
  fn test_urgency() {
    for (query, expected) in [
      ("", 1),
      ("?urgency=high", 2),
      ("?urgency=emergency", 2),
      ("?urgency=low", 0),
      ("?urgency=moderate", 0),
      ("?urgency=normal", 1),
      ("?urgency=0", 0),
      ("?urgency=2", 2),
      ("?urgency=12", 1),
      ("?urgency=invalid", 1),
      ("?priority=low", 0),
      ("?priority=low&urgency=high", 2),
      ("?urgency=&priority=high", 2),
    ] {
      assert_eq!(parse_urgency(&url(&format!("dbus://{}", query))), expected, "{}", query);
    }
  }

  #[test]
  fn test_position() {
    assert_eq!(parse_position(&url("dbus://")), Ok(None));
    assert_eq!(parse_position(&url("dbus://?x=100&y=200")), Ok(Some((100, 200))));
    // A coordinate of 0 is a valid screen edge
    assert_eq!(parse_position(&url("dbus://_/?x=0&y=0")), Ok(Some((0, 0))));
    assert_eq!(parse_position(&url("dbus://_/?x=0&y=5")), Ok(Some((0, 5))));
    assert_eq!(parse_position(&url("dbus://?x=&y=")), Ok(None));
    assert_eq!(parse_position(&url("dbus://?x=invalid&y=1")), Err(()));
    assert_eq!(parse_position(&url("dbus://?x=0")), Err(()));
    assert_eq!(parse_position(&url("dbus://?y=5")), Err(()));
  }

  #[test]
  fn test_images() {
    for notify_type in [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure] {
      assert!(image_png(&notify_type).starts_with(b"\x89PNG"));
      let path = image_path(&notify_type).unwrap();
      assert_eq!(std::fs::read(&path).unwrap(), image_png(&notify_type));
    }
  }

  #[test]
  fn test_registered() {
    use crate::notify::registry::from_url;

    // Each schema loads where it can work, and only there
    let bus = cfg!(unix);
    for (url, name, expected) in [
      ("dbus://", "DBus Notification", bus),
      ("kde://", "DBus Notification", bus),
      ("qt://", "DBus Notification", bus),
      ("glib://", "GLib Notification", bus),
      ("gio://", "GLib Notification", bus),
      ("gnome://", "Gnome Notification", bus),
      ("macosx://", "MacOSX Notification", cfg!(target_os = "macos")),
      ("windows://", "Windows Notification", cfg!(windows)),
    ] {
      let loaded = from_url(url);
      assert_eq!(loaded.is_some(), expected, "{}", url);
      if let Some(svc) = loaded {
        assert_eq!(svc.service_name(), name);
      }
      let listed = crate::notify::registry::all_service_details().iter().any(|d| d.service_name == name);
      assert_eq!(listed, expected, "{}", name);
    }

    // Bad options still refuse to load
    assert!(from_url("dbus://?x=invalid&y=1").is_none());
  }

  #[test]
  fn test_include_image() {
    assert!(include_image(&url("dbus://")));
    assert!(include_image(&url("dbus://?image=yes")));
    assert!(!include_image(&url("dbus://?image=no")));
    assert!(!include_image(&url("dbus://?image=False")));
  }
}

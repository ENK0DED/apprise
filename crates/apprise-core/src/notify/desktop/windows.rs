//! Windows notifications (`windows://`): a taskbar balloon, as upstream
//! shows one through pywin32's Shell_NotifyIcon.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

/// Seconds the balloon stays up.
const DEFAULT_DURATION_SECS: u64 = 12;

pub struct Windows {
  include_image: bool,
  duration: Duration,
  tags: Vec<String>,
}

impl Windows {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Anything but a positive whole number keeps the default
    let duration = url.get("duration").and_then(|v| v.trim().parse::<u64>().ok()).filter(|d| *d > 0).unwrap_or(DEFAULT_DURATION_SECS);
    Some(Self { include_image: super::include_image(url), duration: Duration::from_secs(duration), tags: url.tags() })
  }

  /// The balloon's icon: the asset's own `.ico` file or the bundled one;
  /// `None` shows the stock application icon.
  fn icon(&self, ctx: &NotifyContext) -> Option<super::IconSource> {
    if self.include_image { super::icon_source(ctx) } else { None }
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Windows Notification",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/windows/"),
      protocols: vec!["windows"],
      description: "Show a Windows taskbar notification.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Windows {
  fn schemas(&self) -> &[&str] {
    &["windows"]
  }
  fn service_name(&self) -> &str {
    "Windows Notification"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_max_line_count(&self) -> usize {
    2
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    #[cfg(windows)]
    {
      let (title, body, duration) = (ctx.title.clone(), ctx.body.clone(), self.duration);
      let icon = self.icon(ctx);
      // The balloon is shown, then taken down again after our duration
      crate::notify::throttle().await;
      let shown = tokio::task::spawn_blocking(move || balloon::show(&title, &body, icon, duration)).await.map_err(|e| NotifyError::Other(e.to_string()))?;
      match shown {
        Ok(()) => {
          tracing::info!("Sent Windows notification.");
          Ok(true)
        }
        Err(e) => {
          tracing::warn!("Failed to send Windows notification.");
          tracing::debug!("Windows Exception: {}", e);
          Err(NotifyError::Other(format!("Windows: {}", e)))
        }
      }
    }
    #[cfg(not(windows))]
    {
      let _ = (self.icon(ctx), self.duration);
      Err(NotifyError::NotSupported)
    }
  }
}

#[cfg(windows)]
mod balloon {
  use std::time::Duration;

  use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
  use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
  use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
  use windows_sys::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR,
    LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadIconW, LoadImageW, PostQuitMessage, RegisterClassW, UnregisterClassW, WM_DESTROY, WM_USER, WNDCLASSW, WS_OVERLAPPED,
    WS_SYSMENU,
  };

  use super::super::IconSource;

  const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
  const CALLBACK_MESSAGE: u32 = WM_USER + 20;

  fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
  }

  /// Copy `value` into a fixed buffer, cut to fit with its terminator.
  fn fill(buf: &mut [u16], value: &str) {
    let units: Vec<u16> = value.encode_utf16().take(buf.len() - 1).collect();
    buf[..units.len()].copy_from_slice(&units);
    buf[units.len()] = 0;
  }

  unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
      // Take our icon off the taskbar with the window
      unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        Shell_NotifyIconW(NIM_DELETE, &nid);
        PostQuitMessage(0);
      }
      return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
  }

  pub(super) fn show(title: &str, body: &str, icon: Option<IconSource>, duration: Duration) -> Result<(), String> {
    let class_name = wide("AppriseTaskbar");
    let window_name = wide("Taskbar");
    unsafe {
      let hinstance = GetModuleHandleW(std::ptr::null());
      let mut class: WNDCLASSW = std::mem::zeroed();
      class.hInstance = hinstance;
      class.lpszClassName = class_name.as_ptr();
      class.lpfnWndProc = Some(window_proc);
      if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return Err(format!("could not register the window class ({})", GetLastError()));
      }

      let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        window_name.as_ptr(),
        WS_OVERLAPPED | WS_SYSMENU,
        0,
        0,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null(),
      );
      if hwnd.is_null() {
        let error = GetLastError();
        UnregisterClassW(class_name.as_ptr(), hinstance);
        return Err(format!("could not create the window ({})", error));
      }

      // Our icon, or the stock application icon
      let mut owned: HICON = std::ptr::null_mut();
      match &icon {
        // The asset's own icon file, loaded as upstream loads it
        Some(IconSource::File(path)) => {
          let name = wide(&path.to_string_lossy());
          owned = LoadImageW(std::ptr::null_mut(), name.as_ptr(), IMAGE_ICON, 0, 0, LR_LOADFROMFILE | LR_DEFAULTSIZE) as HICON;
          if owned.is_null() {
            tracing::warn!("Could not load windows notification icon ({}): {}", path.display(), GetLastError());
          }
        }
        // Windows reads PNG-compressed icons straight from memory
        Some(IconSource::Bundled(png)) => {
          owned = CreateIconFromResourceEx(png.as_ptr(), png.len() as u32, 1, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR);
          if owned.is_null() {
            tracing::warn!("Could not load windows notification icon ({})", GetLastError());
          }
        }
        None => {}
      }
      let hicon = if owned.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { owned };

      let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
      nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
      nid.hWnd = hwnd;
      nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
      nid.uCallbackMessage = CALLBACK_MESSAGE;
      nid.hIcon = hicon;
      fill(&mut nid.szTip, "Tooltip");
      let added = Shell_NotifyIconW(NIM_ADD, &nid) != 0;

      nid.uFlags = NIF_INFO;
      fill(&mut nid.szTip, "Balloon Tooltip");
      fill(&mut nid.szInfo, body);
      fill(&mut nid.szInfoTitle, title);
      nid.Anonymous.uTimeout = 200;
      let shown = added && Shell_NotifyIconW(NIM_MODIFY, &nid) != 0;

      // Take a rest, then take everything down again
      if shown {
        std::thread::sleep(duration);
      }
      DestroyWindow(hwnd);
      UnregisterClassW(class_name.as_ptr(), hinstance);
      if !owned.is_null() {
        DestroyIcon(owned);
      }
      if shown { Ok(()) } else { Err("the taskbar refused the notification".into()) }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(url: &str) -> Windows {
    Windows::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    let w = parse("windows://");
    assert!(w.include_image);
    assert_eq!(w.duration, Duration::from_secs(12));
    assert!(!parse("windows://?image=no").include_image);
    assert_eq!(parse("windows://_/?duration=20").duration, Duration::from_secs(20));
    // Anything but a positive whole number keeps the default
    for value in ["0", "-5", "5.5", "invalid", ""] {
      assert_eq!(parse(&format!("windows://?duration={}", value)).duration, Duration::from_secs(12), "{}", value);
    }
  }

  #[test]
  fn test_static_details() {
    let details = Windows::static_details();
    assert_eq!(details.service_name, "Windows Notification");
    assert_eq!(details.protocols, vec!["windows"]);
  }

  #[test]
  fn test_icon_follows_the_asset() {
    use super::super::IconSource;
    use crate::asset::AppriseAsset;
    let dir = tempfile::tempdir().unwrap();
    let custom = dir.path().join("custom-info-128x128.ico");
    std::fs::write(&custom, b"ico").unwrap();
    let ctx = |mask: Option<String>, theme: &str| {
      let asset = AppriseAsset { image_path_mask: mask, theme: theme.to_string(), ..Default::default() };
      NotifyContext { asset, ..Default::default() }
    };
    let w = parse("windows://");
    // The asset's own file, as upstream's image_path(..., ".ico")
    let mask = dir.path().join("custom-{TYPE}-{XY}{EXTENSION}").display().to_string();
    assert_eq!(w.icon(&ctx(Some(mask), "default")), Some(IconSource::File(custom)));
    // A mask naming no existing file gives the stock icon
    let missing = dir.path().join("missing-{TYPE}{EXTENSION}").display().to_string();
    assert_eq!(w.icon(&ctx(Some(missing), "default")), None);
    // Without a mask, the bundled default theme
    assert!(matches!(w.icon(&ctx(None, "default")), Some(IconSource::Bundled(_))));
    assert_eq!(w.icon(&ctx(None, "custom")), None);
    assert_eq!(parse("windows://?image=no").icon(&ctx(None, "default")), None);
  }

  #[cfg(not(windows))]
  #[tokio::test]
  async fn test_not_supported_elsewhere() {
    assert!(matches!(parse("windows://").send(&NotifyContext::default()).await, Err(NotifyError::NotSupported)));
  }
}

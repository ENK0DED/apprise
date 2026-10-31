// The local logger only exists on Unix; elsewhere from_url() refuses the URL
// and the rest goes unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use crate::error::NotifyError;
use crate::notify::rsyslog::{facility_name, resolve_facility, severity};
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;

/// The local syslog socket, as `_PATH_LOG` in `<syslog.h>`.
#[cfg(target_os = "macos")]
const LOG_PATH: &str = "/var/run/syslog";
#[cfg(not(target_os = "macos"))]
const LOG_PATH: &str = "/dev/log";

/// Upstream `NotifySyslog`: writes to the local system logger the way the
/// C library's `syslog()` does after `openlog(app_id, LOG_PID, facility)`.
pub struct Syslog {
  facility: u8,
  log_pid: bool,
  log_perror: bool,
  tags: Vec<String>,
}

impl Syslog {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Python only provides the syslog module on Unix systems.
    if cfg!(not(unix)) {
      return None;
    }

    // The facility may be given as the host (syslog://local5) or as the
    // last path entry.
    let mut tokens: Vec<String> = url.host.iter().filter(|h| !h.is_empty()).cloned().collect();
    tokens.extend(url.path_parts.iter().cloned());
    let facility = match resolve_facility(&tokens, url.get("facility")) {
      Ok(f) => f,
      Err(name) => {
        tracing::warn!("An invalid syslog facility ({}) was specified.", name);
        return None;
      }
    };

    Some(Self {
      facility,
      // An unrecognised value reads as false, as upstream `parse_bool()` does
      log_pid: url.get("logpid").map(parse_bool).unwrap_or(true),
      log_perror: url.get("logperror").map(parse_bool).unwrap_or(false),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Syslog",
      service_url: Some("https://tools.ietf.org/html/rfc5424"),
      setup_url: Some("https://appriseit.com/services/syslog/"),
      protocols: vec!["syslog"],
      description: "Send notifications to the local system logger.",
      attachment_support: false,
    }
  }

  /// The tag the C library puts after the timestamp: `Apprise[PID]` with
  /// `LOG_PID`, otherwise just the identity.
  fn tag(&self, app_id: &str, pid: u32) -> String {
    if self.log_pid { format!("{}[{}]", app_id, pid) } else { app_id.to_string() }
  }

  /// The record the C library writes to the log socket:
  /// `<PRI>Mmm dd hh:mm:ss TAG: MESSAGE` (BSD syslog, local time).
  fn record(&self, ctx: &NotifyContext, timestamp: &chrono::DateTime<chrono::Local>, pid: u32) -> String {
    let priority = self.facility as u16 + severity(&ctx.notify_type) as u16;
    format!("<{}>{} {}: {}", priority, timestamp.format("%b %e %H:%M:%S"), self.tag(&ctx.asset.app_id, pid), message(ctx))
  }
}

fn message(ctx: &NotifyContext) -> String {
  if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) }
}

/// Write one record to the syslog socket at `path`. A datagram socket is
/// tried first; a stream socket gets the record NUL-terminated, as the C
/// library does.
#[cfg(unix)]
fn write_record(path: &str, record: &[u8]) -> std::io::Result<()> {
  use std::io::Write;
  let socket = std::os::unix::net::UnixDatagram::unbound()?;
  match socket.connect(path) {
    Ok(()) => socket.send(record).map(|_| ()),
    Err(e) if e.raw_os_error() == Some(libc_eprototype()) => {
      let mut stream = std::os::unix::net::UnixStream::connect(path)?;
      stream.write_all(record)?;
      stream.write_all(b"\0")
    }
    Err(e) => Err(e),
  }
}

/// `EPROTOTYPE`: connecting a datagram socket to a stream listener.
#[cfg(unix)]
fn libc_eprototype() -> i32 {
  if cfg!(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd")) { 41 } else { 91 }
}

#[async_trait]
impl Notify for Syslog {
  fn schemas(&self) -> &[&str] {
    &["syslog"]
  }
  fn service_name(&self) -> &str {
    "Syslog"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// Upstream does not throttle syslog
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }

  #[cfg(unix)]
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let pid = std::process::id();
    let record = self.record(ctx, &chrono::Local::now(), pid);
    let path = crate::notify::service_base(LOG_PATH);

    if self.log_perror {
      // LOG_PERROR echoes the record, without its priority and timestamp.
      eprintln!("{}: {}", self.tag(&ctx.asset.app_id, pid), message(ctx));
    }

    crate::notify::throttle().await;
    match tokio::task::spawn_blocking(move || write_record(&path, record.as_bytes())).await {
      Ok(Ok(())) => {
        tracing::info!("Sent Syslog notification.");
        Ok(true)
      }
      Ok(Err(e)) => {
        tracing::warn!("Could not write to the local syslog ({}) using facility {}.", LOG_PATH, facility_name(self.facility));
        tracing::debug!("Syslog Exception: {}", e);
        Ok(false)
      }
      Err(e) => Err(NotifyError::Other(e.to_string())),
    }
  }

  #[cfg(not(unix))]
  async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
    Err(NotifyError::NotSupported)
  }
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use chrono::TimeZone;

  fn parse(url: &str) -> Option<Syslog> {
    ParsedUrl::parse(url).and_then(|p| Syslog::from_url(&p))
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "syslog://",
      "syslog://:@/",
      "syslog://?logpid=no&logperror=yes",
      "syslog://_/?facility=local5",
      "syslog://_/?facility=d",
      "syslog://kern?logpid=no&logperror=y",
      "syslog://kern?facility=d",
      "syslog://local5",
      "syslog://user/",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["syslog://_/?facility=invalid", "syslog://localhost", "syslog://_", "syslog://kern:garbage"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let s = parse("syslog://").unwrap();
    assert_eq!(s.facility, 8);
    assert!(s.log_pid);
    assert!(!s.log_perror);

    let s = parse("syslog://?logpid=no&logperror=yes").unwrap();
    assert!(!s.log_pid);
    assert!(s.log_perror);

    assert_eq!(parse("syslog://_/?facility=local5").unwrap().facility, 168);
    assert_eq!(parse("syslog://_/?facility=d").unwrap().facility, 24);
    // The host is the facility
    let s = parse("syslog://kern?logpid=no&logperror=y").unwrap();
    assert_eq!(s.facility, 0);
    assert!(!s.log_pid);
    assert!(s.log_perror);
    // ?facility= wins over the host
    assert_eq!(parse("syslog://kern?facility=d").unwrap().facility, 24);
  }

  #[test]
  fn test_record_format() {
    let ts = chrono::Local.with_ymd_and_hms(2026, 3, 5, 7, 8, 9).unwrap();
    let s = parse("syslog://").unwrap();
    assert_eq!(s.record(&ctx("", "body", NotifyType::Info), &ts, 4242), "<14>Mar  5 07:08:09 Apprise[4242]: body");
    assert_eq!(s.record(&ctx("title", "body", NotifyType::Failure), &ts, 4242), "<10>Mar  5 07:08:09 Apprise[4242]: title: body");

    let s = parse("syslog://local7?logpid=no").unwrap();
    let ts = chrono::Local.with_ymd_and_hms(2026, 11, 25, 23, 0, 1).unwrap();
    assert_eq!(s.record(&ctx("", "body", NotifyType::Success), &ts, 1), "<189>Nov 25 23:00:01 Apprise: body");
    assert_eq!(s.record(&ctx("", "body", NotifyType::Warning), &ts, 1), "<188>Nov 25 23:00:01 Apprise: body");
  }

  #[tokio::test]
  async fn test_send_writes_to_log_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let server = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
    server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

    let svc = from_url("syslog://daemon").unwrap();
    let ok = crate::notify::with_test_base(path.to_str().unwrap(), svc.send(&ctx("Title", "Body", NotifyType::Warning))).await.unwrap();
    assert!(ok);

    let mut buf = [0u8; 512];
    let n = server.recv(&mut buf).unwrap();
    let record = String::from_utf8_lossy(&buf[..n]).to_string();
    // <PRI>Mmm dd hh:mm:ss is 3 + 1 + 15 characters
    assert!(record.starts_with("<28>"), "{}", record);
    assert_eq!(&record[19..], format!(" Apprise[{}]: Title: Body", std::process::id()));
  }

  #[tokio::test]
  async fn test_send_without_log_socket_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    let svc = from_url("syslog://").unwrap();
    let ok = crate::notify::with_test_base(path.to_str().unwrap(), svc.send(&ctx("", "Body", NotifyType::Info))).await.unwrap();
    assert!(!ok);
  }

  #[test]
  fn test_static_details() {
    let details = Syslog::static_details();
    assert_eq!(details.service_name, "Syslog");
    assert_eq!(details.protocols, vec!["syslog"]);
    assert!(!details.attachment_support);
  }
}

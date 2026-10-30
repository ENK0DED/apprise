use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool_with_default, split_path};
use async_trait::async_trait;
use std::time::Duration;

/// Default remote syslog port.
const DEFAULT_PORT: u16 = 514;

/// Syslog facilities by name, in upstream `SYSLOG_FACILITY_MAP` order (a
/// short name matches the first facility it is a prefix of). The values are
/// the facility codes already multiplied by 8, as in `<syslog.h>`.
pub(crate) const FACILITIES: [(&str, u8); 18] = [
  ("kern", 0),
  ("user", 8),
  ("mail", 16),
  ("daemon", 24),
  ("auth", 32),
  ("syslog", 40),
  ("lpr", 48),
  ("news", 56),
  ("uucp", 64),
  ("cron", 72),
  ("local0", 128),
  ("local1", 136),
  ("local2", 144),
  ("local3", 152),
  ("local4", 160),
  ("local5", 168),
  ("local6", 176),
  ("local7", 184),
];

/// Default facility (upstream `SyslogFacility.USER`).
pub(crate) const DEFAULT_FACILITY: u8 = 8;

/// Resolve the facility of a syslog URL: the last path entry (or, for
/// `syslog://`, the host), overridden by a non-empty `?facility=`. A name
/// that is not exact matches the first facility it is a prefix of, so `d`
/// is `daemon`. `Err` means the name matches nothing.
pub(crate) fn resolve_facility(tokens: &[String], qsd_facility: Option<&str>) -> Result<u8, String> {
  let mut facility = tokens.last().map(|t| t.to_lowercase());
  if let Some(f) = qsd_facility.filter(|f| !f.is_empty()) {
    facility = Some(f.to_lowercase());
  }
  let Some(name) = facility.filter(|f| !f.is_empty()) else {
    return Ok(DEFAULT_FACILITY);
  };
  FACILITIES.iter().find(|(n, _)| *n == name).or_else(|| FACILITIES.iter().find(|(n, _)| n.starts_with(&name))).map(|(_, v)| *v).ok_or(name)
}

/// The facility name for a facility code (for logging).
pub(crate) fn facility_name(value: u8) -> &'static str {
  FACILITIES.iter().find(|(_, v)| *v == value).map(|(n, _)| *n).unwrap_or("user")
}

/// Syslog severity for each notification type (upstream `SYSLOG_PUBLISH_MAP`).
pub(crate) fn severity(notify_type: &NotifyType) -> u8 {
  match notify_type {
    NotifyType::Info => 6,    // LOG_INFO
    NotifyType::Success => 5, // LOG_NOTICE
    NotifyType::Warning => 4, // LOG_WARNING
    NotifyType::Failure => 2, // LOG_CRIT
  }
}

pub struct RSyslog {
  host: String,
  port: u16,
  facility: u8,
  log_pid: bool,
  connect_timeout: Duration,
  tags: Vec<String>,
}

impl RSyslog {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parses this URL without host verification, so a host such
    // as `localhost:garbage` loads and only fails when sending.
    let host = url.host.clone().unwrap_or_default();
    let port = match url.get("port") {
      Some(p) => p.trim().parse::<u16>().ok().filter(|p| *p > 0)?,
      None => url.port.unwrap_or(DEFAULT_PORT),
    };

    let facility = match resolve_facility(&split_path(&url.path_parts), url.get("facility")) {
      Ok(f) => f,
      Err(name) => {
        tracing::warn!("An invalid syslog facility ({}) was specified.", name);
        return None;
      }
    };

    // A value that is not a recognised boolean reads as false, as upstream
    // `parse_bool()` does; only a missing option takes the default.
    let log_pid = url.get("logpid").map(|v| parse_bool_with_default(v, false)).unwrap_or(true);
    let connect_timeout = crate::utils::socket::url_timeouts(url).connect_timeout().unwrap_or(Duration::from_secs(4));

    Some(Self { host, port, facility, log_pid, connect_timeout, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Remote Syslog",
      service_url: Some("https://tools.ietf.org/html/rfc5424"),
      setup_url: Some("https://appriseit.com/services/rsyslog/"),
      protocols: vec!["rsyslog"],
      description: "Send notifications to a remote syslog server over UDP.",
      attachment_support: false,
    }
  }

  /// The datagram for one notification: `<PRI>- PID MESSAGE`, where PRI is
  /// the facility code (already a multiple of 8) plus the severity.
  fn payload(&self, ctx: &NotifyContext, pid: u32) -> String {
    let body = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
    let priority = self.facility as u16 + severity(&ctx.notify_type) as u16;
    if self.log_pid { format!("<{}>- {} {}", priority, pid, body) } else { format!("<{}>- {}", priority, body) }
  }

  async fn send_datagram(&self, payload: &[u8]) -> Result<usize, String> {
    // Upstream opens an AF_INET socket, so prefer an IPv4 address.
    let mut addrs: Vec<std::net::SocketAddr> =
      tokio::net::lookup_host((self.host.as_str(), self.port)).await.map_err(|e| format!("could not resolve {}: {}", self.host, e))?.collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    let addr = addrs.first().ok_or_else(|| format!("no address for {}", self.host))?;
    let bind: std::net::SocketAddr = if addr.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let socket = tokio::net::UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    tokio::time::timeout(self.connect_timeout, socket.send_to(payload, addr)).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())
  }
}

#[async_trait]
impl Notify for RSyslog {
  fn schemas(&self) -> &[&str] {
    &["rsyslog"]
  }
  fn service_name(&self) -> &str {
    "Remote Syslog"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let payload = self.payload(ctx, std::process::id());
    tracing::debug!("RSyslog Host: {}:{}/{}", self.host, self.port, facility_name(self.facility));
    tracing::debug!("RSyslog Payload: {}", payload);

    match self.send_datagram(payload.as_bytes()).await {
      Ok(sent) if sent < payload.len() => {
        tracing::warn!("RSyslog sent {} byte(s) but intended to send {} byte(s)", sent, payload.len());
        Ok(false)
      }
      Ok(_) => {
        tracing::info!("Sent RSyslog notification.");
        Ok(true)
      }
      Err(e) => {
        tracing::warn!("A connection error occurred sending RSyslog notification to {}:{}/{}", self.host, self.port, facility_name(self.facility));
        tracing::debug!("Socket Exception: {}", e);
        Ok(false)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  fn parse(url: &str) -> Option<RSyslog> {
    ParsedUrl::parse(url).and_then(|p| RSyslog::from_url(&p))
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "rsyslog://",
      "rsyslog://localhost",
      "rsyslog://localhost/?facility=local5",
      "rsyslog://localhost/?facility=d",
      "rsyslog://localhost:518",
      "rsyslog://localhost:514",
      "rsyslog://localhost/kern",
      "rsyslog://localhost:514/d",
      "rsyslog://localhost:9000/d?logpid=no",
      "rsyslog://localhost/?facility=",
      "rsyslog://192.168.1.1",
      "rsyslog://syslog.example.com:1514",
      "rsyslog://localhost:garbage",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["rsyslog://localhost/?facility=invalid", "rsyslog://localhost/invalid"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_facility_resolution() {
    let facility = |url: &str| parse(url).unwrap().facility;
    assert_eq!(facility("rsyslog://localhost"), 8);
    assert_eq!(facility("rsyslog://localhost/?facility=local5"), 168);
    // A prefix matches the first facility in upstream order
    assert_eq!(facility("rsyslog://localhost/?facility=d"), 24);
    assert_eq!(facility("rsyslog://localhost/a/b/local"), 128);
    assert_eq!(facility("rsyslog://localhost/LOCAL1"), 136);
    // ?facility= wins over the path
    assert_eq!(facility("rsyslog://localhost/kern?facility=mail"), 16);
  }

  #[test]
  fn test_from_url_fields() {
    let rs = parse("rsyslog://localhost:9000/d?logpid=no").unwrap();
    assert_eq!(rs.host, "localhost");
    assert_eq!(rs.port, 9000);
    assert_eq!(rs.facility, 24);
    assert!(!rs.log_pid);

    let rs = parse("rsyslog://localhost").unwrap();
    assert_eq!(rs.port, 514);
    assert!(rs.log_pid);
    assert_eq!(rs.connect_timeout, Duration::from_secs(4));

    // An unrecognised logpid value reads as false
    assert!(!parse("rsyslog://localhost?logpid=maybe").unwrap().log_pid);
    // A garbage port is kept as part of the host
    assert_eq!(parse("rsyslog://localhost:garbage").unwrap().host, "localhost:garbage");
    assert_eq!(parse("rsyslog://localhost?port=600").unwrap().port, 600);
    assert_eq!(parse("rsyslog://localhost?cto=1.5").unwrap().connect_timeout, Duration::from_secs_f64(1.5));
  }

  #[test]
  fn test_priority_values() {
    // PRI = (facility code * 8) + severity (RFC 5424, section 6.2.1); the
    // facility table already holds the multiplied value.
    for (facility, notify_type, priority) in [
      ("kern", NotifyType::Success, 5),
      ("user", NotifyType::Info, 14),
      ("mail", NotifyType::Failure, 18),
      ("daemon", NotifyType::Warning, 28),
      ("local0", NotifyType::Info, 134),
      ("local7", NotifyType::Failure, 186),
    ] {
      let rs = parse(&format!("rsyslog://localhost/{}?logpid=no", facility)).unwrap();
      assert_eq!(rs.payload(&ctx("", "test", notify_type), 1), format!("<{}>- test", priority));
    }
  }

  #[test]
  fn test_payload_format() {
    let rs = parse("rsyslog://localhost").unwrap();
    assert_eq!(rs.payload(&ctx("", "test", NotifyType::Info), 123), "<14>- 123 test");
    assert_eq!(rs.payload(&ctx("Testing a title entry", "test", NotifyType::Info), 123), "<14>- 123 Testing a title entry: test");
  }

  #[tokio::test]
  async fn test_send_datagram_bytes() {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();

    let svc = from_url(&format!("rsyslog://127.0.0.1:{}/local7", port)).unwrap();
    assert!(svc.send(&ctx("Title", "Body", NotifyType::Failure)).await.unwrap());

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), server.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], format!("<186>- {} Title: Body", std::process::id()).as_bytes());

    let svc = from_url(&format!("rsyslog://127.0.0.1:{}/?facility=d&logpid=no", port)).unwrap();
    assert!(svc.send(&ctx("", "multi\nline", NotifyType::Warning)).await.unwrap());
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), server.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"<28>- multi\nline");
  }

  #[tokio::test]
  async fn test_send_unresolvable_host_fails() {
    let svc = from_url("rsyslog://host.invalid").unwrap();
    assert!(!svc.send(&ctx("", "test", NotifyType::Info)).await.unwrap());
    let svc = from_url("rsyslog://").unwrap();
    assert!(!svc.send(&ctx("", "test", NotifyType::Info)).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = RSyslog::static_details();
    assert_eq!(details.service_name, "Remote Syslog");
    assert_eq!(details.protocols, vec!["rsyslog"]);
    assert!(!details.attachment_support);
  }
}

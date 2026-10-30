use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;
use crate::utils::url_args::{has_invalid_port, parse_bool_with_default, url_credentials};
use async_trait::async_trait;
use md5::{Digest, Md5};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEFAULT_PORT: u16 = 23053;

/// The single notification type Apprise registers.
const NOTIFICATION_TYPE: &str = "New Messages";

/// Socket timeout of the GNTP client upstream uses (`gntp.notifier`).
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);

/// Upstream `GROWL_PRIORITY_MAP`: a priority matches the first key it
/// starts with.
const PRIORITY_MAP: [(&str, i8); 10] = [("l", -2), ("m", -1), ("n", 0), ("h", 1), ("e", 2), ("-2", -2), ("-1", -1), ("0", 0), ("1", 1), ("2", 2)];

/// Upstream `AppriseAsset.image_url_mask`.
const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

pub struct Growl {
  host: String,
  port: u16,
  password: Option<String>,
  priority: i8,
  version: i64,
  include_image: bool,
  sticky: bool,
  /// Registration happens once, before the first notification.
  registered: AtomicBool,
  tags: Vec<String>,
}

impl Growl {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    if has_invalid_port(&url.raw) {
      return None;
    }
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let port = match url.get("port") {
      Some(p) => p.trim().parse::<u16>().ok().filter(|p| *p > 0)?,
      None => url.port.unwrap_or(DEFAULT_PORT),
    };

    // growl://password@host: without a password, the user field holds it.
    let (user, password) = url_credentials(url);
    let password = password.or(user);

    let priority = url
      .get("priority")
      .filter(|p| !p.is_empty())
      .map(|p| {
        let p = p.to_lowercase();
        PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v).unwrap_or(0)
      })
      .unwrap_or(0);

    // Only the major number counts; an unusable value keeps version 2.
    let version = match url.get("version").filter(|v| !v.is_empty()) {
      Some(v) => match v.trim().split('.').next().unwrap_or("").parse::<i64>() {
        Ok(n) if n != 0 => n,
        Ok(_) => 2,
        Err(_) => {
          tracing::warn!("An invalid Growl version of \"{}\" was specified and will be ignored.", v);
          2
        }
      },
      None => 2,
    };

    // An unrecognised value reads as false, as upstream `parse_bool()` does
    let include_image = url.get("image").map(|v| parse_bool_with_default(v, false)).unwrap_or(true);
    let sticky = url.get("sticky").map(|v| parse_bool_with_default(v, false)).unwrap_or(true);

    Some(Self { host, port, password, priority, version, include_image, sticky, registered: AtomicBool::new(false), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Growl",
      service_url: Some("http://growl.info/"),
      setup_url: Some("https://appriseit.com/services/growl/"),
      protocols: vec!["growl"],
      description: "Send notifications to Growl over GNTP.",
      attachment_support: false,
    }
  }

  /// The GNTP information line. With a password the key hash follows:
  /// `MD5:<hex(MD5(MD5(password + salt)))>.<hex(salt)>`, as `gntp` builds it.
  fn info_line(&self, message_type: &str) -> String {
    match self.password.as_deref().filter(|p| !p.is_empty()) {
      None => format!("GNTP/1.0 {} NONE", message_type),
      Some(password) => {
        let salt: [u8; 16] = rand::random();
        let mut basis = password.as_bytes().to_vec();
        basis.extend_from_slice(&salt);
        let key = Md5::digest(&basis);
        let key_hash = Md5::digest(key);
        format!("GNTP/1.0 {} NONE MD5:{}.{}", message_type, hex::encode_upper(key_hash), hex::encode_upper(salt))
      }
    }
  }

  fn register_packet(&self, app_id: &str) -> String {
    let mut packet = format!("{}\r\n", self.info_line("REGISTER"));
    push_header(&mut packet, "Application-Name", app_id);
    push_header(&mut packet, "Notifications-Count", "1");
    push_origin_headers(&mut packet);
    packet.push_str("\r\n");
    push_header(&mut packet, "Notification-Name", NOTIFICATION_TYPE);
    push_header(&mut packet, "Notification-Enabled", "True");
    packet.push_str("\r\n");
    packet
  }

  fn notify_packet(&self, ctx: &NotifyContext) -> String {
    let mut packet = format!("{}\r\n", self.info_line("NOTIFY"));
    push_header(&mut packet, "Application-Name", &ctx.asset.app_id);
    push_header(&mut packet, "Notification-Name", NOTIFICATION_TYPE);
    push_header(&mut packet, "Notification-Title", &ctx.title);
    if self.sticky {
      push_header(&mut packet, "Notification-Sticky", "True");
    }
    if self.priority != 0 {
      push_header(&mut packet, "Notification-Priority", &self.priority.to_string());
    }
    if let Some(icon) = self.icon_url(ctx) {
      push_header(&mut packet, "Notification-Icon", &icon);
    }
    if !ctx.body.is_empty() {
      push_header(&mut packet, "Notification-Text", &ctx.body);
    }
    push_origin_headers(&mut packet);
    packet.push_str("\r\n");
    packet
  }

  /// Version 2 sends the 72x72 image as a URL. Version 1 would embed the
  /// raw image, which this port does not ship, so it sends none.
  fn icon_url(&self, ctx: &NotifyContext) -> Option<String> {
    if !self.include_image || self.version < 2 {
      return None;
    }
    let mask = ctx.asset.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
    let notify_type = match ctx.notify_type {
      NotifyType::Info => "info",
      NotifyType::Success => "success",
      NotifyType::Warning => "warning",
      NotifyType::Failure => "failure",
    };
    Some(mask.replace("{THEME}", &ctx.asset.theme).replace("{TYPE}", notify_type).replace("{XY}", "72x72").replace("{EXTENSION}", ".png"))
  }

  /// Send one GNTP message on its own connection and read the reply, which
  /// ends with a blank line.
  async fn exchange(&self, packet: &str) -> Result<String, String> {
    let addr = format!("{}:{}", self.host, self.port);
    let io = async {
      let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
      stream.write_all(packet.as_bytes()).await.map_err(|e| e.to_string())?;
      let mut response = Vec::new();
      let mut buf = [0u8; 1024];
      while !response.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
          break;
        }
        response.extend_from_slice(&buf[..n]);
      }
      Ok::<_, String>(String::from_utf8_lossy(&response).into_owned())
    };
    tokio::time::timeout(SOCKET_TIMEOUT, io).await.map_err(|_| "timed out".to_string())?
  }

  /// Send a message and check the reply: `-OK` is success, `-ERROR` is a
  /// failure, and anything that is not GNTP is a parse error.
  async fn transact(&self, kind: &str, packet: &str) -> bool {
    let response = match self.exchange(packet).await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A network error occurred sending the Growl {} to {}.", kind, self.host);
        tracing::debug!("Growl Exception: {}", e);
        return false;
      }
    };
    match response_type(&response) {
      Some(t) if t.eq_ignore_ascii_case("-OK") => true,
      Some(t) if t.eq_ignore_ascii_case("-ERROR") => {
        let header = |name: &str| response.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == name).map(|(_, v)| v.trim().to_string()));
        tracing::warn!("Growl {} failed with response: {} {}", kind, header("Error-Code").unwrap_or_default(), header("Error-Description").unwrap_or_default());
        false
      }
      _ => {
        tracing::warn!("A parsing error occurred sending the Growl {} to {}.", kind, self.host);
        false
      }
    }
  }
}

/// Write one GNTP header. Header lines end with CRLF, so line breaks inside
/// a value are sent as a bare LF.
fn push_header(packet: &mut String, key: &str, value: &str) {
  let value = value.replace("\r\n", "\n").replace('\r', "\n");
  packet.push_str(&format!("{}: {}\r\n", key, value));
}

/// The optional `Origin-*` headers the GNTP client adds to every message.
fn push_origin_headers(packet: &mut String) {
  let read = |path: &str| std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
  let hostname = read("/proc/sys/kernel/hostname").or_else(|| read("/etc/hostname")).or_else(|| std::env::var("COMPUTERNAME").ok());
  let system = match std::env::consts::OS {
    "linux" => "Linux",
    "macos" => "Darwin",
    "windows" => "Windows",
    other => other,
  };
  let release = read("/proc/sys/kernel/osrelease");
  if let Some(hostname) = hostname {
    push_header(packet, "Origin-Machine-Name", &hostname);
  }
  push_header(packet, "Origin-Software-Name", "Apprise");
  push_header(packet, "Origin-Software-Version", env!("CARGO_PKG_VERSION"));
  push_header(packet, "Origin-Platform-Name", system);
  let version = match release {
    Some(r) => format!("{}-{}-{}", system, r, std::env::consts::ARCH),
    None => format!("{}-{}", system, std::env::consts::ARCH),
  };
  push_header(packet, "Origin-Platform-Version", &version);
}

/// The message type of a GNTP reply (`GNTP/1.0 -OK ...`), if it is one.
fn response_type(response: &str) -> Option<&str> {
  let rest = response.strip_prefix("GNTP/")?;
  let (version, rest) = rest.split_once(' ')?;
  let (major, minor) = version.split_once('.')?;
  if major.is_empty() || minor.is_empty() || !major.chars().chain(minor.chars()).all(|c| c.is_ascii_digit()) {
    return None;
  }
  let kind = rest.split([' ', '\r', '\n']).next()?;
  ["REGISTER", "NOTIFY", "SUBSCRIBE", "-OK", "-ERROR"].iter().any(|k| k.eq_ignore_ascii_case(kind)).then_some(kind)
}

#[async_trait]
impl Notify for Growl {
  fn schemas(&self) -> &[&str] {
    &["growl"]
  }
  fn service_name(&self) -> &str {
    "Growl"
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

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if !self.registered.load(Ordering::SeqCst) {
      if !self.transact("registration", &self.register_packet(&ctx.asset.app_id)).await {
        return Ok(false);
      }
      tracing::debug!("Growl server registration completed successfully.");
      self.registered.store(true, Ordering::SeqCst);
    }

    if !self.transact("notification", &self.notify_packet(ctx)).await {
      return Ok(false);
    }
    tracing::info!("Sent Growl notification.");
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  fn parse(url: &str) -> Option<Growl> {
    ParsedUrl::parse(url).and_then(|p| Growl::from_url(&p))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "growl://pass@growl.server",
      "growl://ignored:pass@growl.server",
      "growl://growl.server",
      "growl://growl.server?version=1",
      "growl://growl.server?sticky=yes",
      "growl://growl.server?sticky=no",
      "growl://growl.server?version=2",
      "growl://pass@growl.server?priority=low",
      "growl://pass@growl.server?priority=moderate",
      "growl://pass@growl.server?priority=normal",
      "growl://pass@growl.server?priority=high",
      "growl://pass@growl.server?priority=emergency",
      "growl://pass@growl.server?priority=invalid",
      "growl://pass@growl.server?priority=",
      "growl://growl.server?version=",
      "growl://growl.server?version=crap",
      "growl://growl.changeport:2000",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    // An unusable port fails to parse upstream, too
    for url in ["growl://", "growl://:@/", "growl://growl.garbageport:garbage", "growl://growl.colon:"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let g = parse("growl://growl.server").unwrap();
    assert_eq!((g.host.as_str(), g.port), ("growl.server", 23053));
    assert_eq!(g.password, None);
    assert_eq!(g.priority, 0);
    assert_eq!(g.version, 2);
    assert!(g.include_image);
    // Upstream's URL default for sticky is on
    assert!(g.sticky);

    // The password goes where the user would
    assert_eq!(parse("growl://pass@growl.server").unwrap().password.as_deref(), Some("pass"));
    assert_eq!(parse("growl://ignored:pass@growl.server").unwrap().password.as_deref(), Some("pass"));
    assert_eq!(parse("growl://growl.changeport:2000").unwrap().port, 2000);

    for (value, priority) in
      [("low", -2), ("moderate", -1), ("normal", 0), ("high", 1), ("emergency", 2), ("e", 2), ("-2", -2), ("10", 1), ("invalid", 0), ("", 0)]
    {
      assert_eq!(parse(&format!("growl://growl.server?priority={}", value)).unwrap().priority, priority, "priority={}", value);
    }
    for (value, version) in [("1", 1), ("2.5", 2), ("3", 3), ("0", 2), ("crap", 2), ("", 2)] {
      assert_eq!(parse(&format!("growl://growl.server?version={}", value)).unwrap().version, version, "version={}", value);
    }
    let g = parse("growl://growl.server?sticky=no&image=maybe").unwrap();
    assert!(!g.sticky);
    assert!(!g.include_image);
  }

  #[test]
  fn test_info_line_password_hash() {
    assert_eq!(parse("growl://growl.server").unwrap().info_line("NOTIFY"), "GNTP/1.0 NOTIFY NONE");
    let g = parse("growl://secret@growl.server").unwrap();
    let line = g.info_line("REGISTER");
    let hash = line.strip_prefix("GNTP/1.0 REGISTER NONE MD5:").unwrap();
    let (key_hash, salt) = hash.split_once('.').unwrap();
    // A receiver recomputes MD5(MD5(password + salt)) from the salt
    let mut basis = b"secret".to_vec();
    basis.extend_from_slice(&hex::decode(salt).unwrap());
    assert_eq!(key_hash, hex::encode_upper(Md5::digest(Md5::digest(&basis))));
    assert_eq!(salt, salt.to_uppercase());
  }

  #[test]
  fn test_packets() {
    let g = parse("growl://growl.server?priority=high").unwrap();
    let register = g.register_packet("Apprise");
    assert!(register.starts_with("GNTP/1.0 REGISTER NONE\r\nApplication-Name: Apprise\r\nNotifications-Count: 1\r\n"), "{}", register);
    assert!(register.ends_with("\r\n\r\nNotification-Name: New Messages\r\nNotification-Enabled: True\r\n\r\n"), "{}", register);

    let ctx = NotifyContext { title: "Title".into(), body: "line one\r\nline two".into(), notify_type: NotifyType::Warning, ..Default::default() };
    let notify = g.notify_packet(&ctx);
    assert!(
      notify.starts_with(
        "GNTP/1.0 NOTIFY NONE\r\n\
         Application-Name: Apprise\r\n\
         Notification-Name: New Messages\r\n\
         Notification-Title: Title\r\n\
         Notification-Sticky: True\r\n\
         Notification-Priority: 1\r\n\
         Notification-Icon: https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-72x72.png\r\n\
         Notification-Text: line one\nline two\r\n\
         Origin-"
      ),
      "{}",
      notify
    );
    assert!(notify.ends_with("\r\n\r\n"));

    // No icon for version 1 or image=no, no sticky/priority headers when off
    let g = parse("growl://growl.server?version=1&sticky=no").unwrap();
    let notify = g.notify_packet(&NotifyContext { title: "T".into(), body: "".into(), ..Default::default() });
    assert!(
      notify.starts_with("GNTP/1.0 NOTIFY NONE\r\nApplication-Name: Apprise\r\nNotification-Name: New Messages\r\nNotification-Title: T\r\nOrigin-"),
      "{}",
      notify
    );
    let g = parse("growl://growl.server?image=no").unwrap();
    assert!(!g.notify_packet(&ctx).contains("Notification-Icon"));
  }

  #[test]
  fn test_response_type() {
    assert_eq!(response_type("GNTP/1.0 -OK NONE\r\nResponse-Action: REGISTER\r\n\r\n"), Some("-OK"));
    assert_eq!(response_type("GNTP/1.0 -ERROR NONE\r\nError-Code: 400\r\n\r\n"), Some("-ERROR"));
    assert_eq!(response_type("HTTP/1.1 200 OK\r\n\r\n"), None);
    assert_eq!(response_type("GNTP/x -OK\r\n\r\n"), None);
  }

  /// A GNTP server that answers each connection with the next reply and
  /// records what it received. Connections that do not speak GNTP (such as
  /// a stray local port probe) are dropped without using up a reply.
  async fn gntp_server(replies: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
      let mut received = Vec::new();
      let mut replies = replies.into_iter().peekable();
      while let Some(&reply) = replies.peek() {
        let Ok((mut stream, _)) = listener.accept().await else { break };
        // A message ends with a blank line after its last block; wait
        // until nothing more arrives.
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(150), stream.read(&mut buf)).await {
          if n == 0 {
            break;
          }
          data.extend_from_slice(&buf[..n]);
        }
        if !data.starts_with(b"GNTP/") {
          continue;
        }
        replies.next();
        received.push(String::from_utf8_lossy(&data).into_owned());
        stream.write_all(reply.as_bytes()).await.unwrap();
      }
      received
    });
    (port, handle)
  }

  #[tokio::test]
  async fn test_send_registers_then_notifies() {
    let ok = "GNTP/1.0 -OK NONE\r\nResponse-Action: REGISTER\r\n\r\n";
    let (port, server) = gntp_server(vec![ok, ok, ok]).await;
    let svc = from_url(&format!("growl://127.0.0.1:{}?sticky=no&image=no", port)).unwrap();
    let ctx = NotifyContext { title: "Title".into(), body: "Body".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
    // Registration is only done once
    assert!(svc.send(&ctx).await.unwrap());
    drop(svc);

    let received = server.await.unwrap();
    assert_eq!(received.len(), 3);
    assert!(received[0].starts_with("GNTP/1.0 REGISTER NONE\r\n"));
    assert!(received[1].starts_with(
      "GNTP/1.0 NOTIFY NONE\r\nApplication-Name: Apprise\r\nNotification-Name: New Messages\r\nNotification-Title: Title\r\nNotification-Text: Body\r\n"
    ));
    assert!(received[2].starts_with("GNTP/1.0 NOTIFY NONE\r\n"));
  }

  #[tokio::test]
  async fn test_send_error_responses_fail() {
    let ok = "GNTP/1.0 -OK NONE\r\n\r\n";
    let error = "GNTP/1.0 -ERROR NONE\r\nError-Code: 400\r\nError-Description: Not authorized\r\n\r\n";

    // A rejected registration is retried on the next send
    let (port, server) = gntp_server(vec![error, ok, ok]).await;
    let svc = from_url(&format!("growl://secret@127.0.0.1:{}", port)).unwrap();
    let ctx = NotifyContext { title: "Title".into(), body: "Body".into(), ..Default::default() };
    assert!(!svc.send(&ctx).await.unwrap());
    assert!(svc.send(&ctx).await.unwrap());
    let received = server.await.unwrap();
    assert!(received[0].starts_with("GNTP/1.0 REGISTER NONE MD5:"));
    assert!(received[1].starts_with("GNTP/1.0 REGISTER NONE MD5:"));
    assert!(received[2].starts_with("GNTP/1.0 NOTIFY NONE MD5:"));

    // A rejected or unparseable notification fails
    for reply in [error, "garbage\r\n\r\n"] {
      let (port, _server) = gntp_server(vec![ok, reply]).await;
      let svc = from_url(&format!("growl://127.0.0.1:{}", port)).unwrap();
      assert!(!svc.send(&ctx).await.unwrap(), "reply {:?}", reply);
    }
  }

  #[tokio::test]
  async fn test_send_connection_refused_fails() {
    // Port 1 is privileged, so no test can be listening on it; a port
    // freed by dropping a listener may be taken by another test meanwhile.
    let port = 1;
    let svc = from_url(&format!("growl://127.0.0.1:{}", port)).unwrap();
    assert!(!svc.send(&NotifyContext { body: "Body".into(), ..Default::default() }).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = Growl::static_details();
    assert_eq!(details.service_name, "Growl");
    assert_eq!(details.protocols, vec!["growl"]);
    assert!(!details.attachment_support);
  }
}

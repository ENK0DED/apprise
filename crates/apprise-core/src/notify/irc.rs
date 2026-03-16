use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq)]
enum IrcAuthMode {
  None,
  Server,   // PASS during registration
  NickServ, // IDENTIFY via NickServ after registration
}

pub struct Irc {
  host: String,
  port: u16,
  password: Option<String>,
  nick: String,
  user: String,
  realname: String,
  channels: Vec<String>,
  users: Vec<String>,
  secure: bool,
  auth_mode: IrcAuthMode,
  tags: Vec<String>,
}

impl Irc {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let secure = url.schema == "ircs";
    let port = url.port.unwrap_or(if secure { 6697 } else { 6667 });
    let nick = url.get("nick").map(|s| s.to_string()).or_else(|| url.user.clone()).unwrap_or_else(|| "Apprise".to_string());
    let user = url.user.clone().unwrap_or_else(|| nick.clone());
    let realname = url.get("name").unwrap_or("Apprise Notification").to_string();
    let auth_mode_str = url.get("mode").unwrap_or("server").to_string();

    let auth_mode = match auth_mode_str.to_lowercase().as_str() {
      "none" => IrcAuthMode::None,
      "nickserv" => IrcAuthMode::NickServ,
      _ => IrcAuthMode::Server,
    };

    let mut channels = Vec::new();
    let mut users = Vec::new();
    for part in &url.path_parts {
      if part.starts_with('#') || part.starts_with('&') {
        channels.push(part.clone());
      } else if let Some(stripped) = part.strip_prefix('@') {
        users.push(stripped.to_string());
      } else {
        channels.push(format!("#{}", part));
      }
    }
    if let Some(to) = url.get("to") {
      for t in to.split(',').map(|s| s.trim()) {
        if t.starts_with('#') || t.starts_with('&') {
          channels.push(t.to_string());
        } else if let Some(stripped) = t.strip_prefix('@') {
          users.push(stripped.to_string());
        } else if !t.is_empty() {
          channels.push(format!("#{}", t));
        }
      }
    }
    if channels.is_empty() && users.is_empty() {
      return None;
    }

    Some(Self { host, port, password: url.password.clone(), nick, user, realname, channels, users, secure, auth_mode, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "IRC",
      service_url: None,
      setup_url: None,
      protocols: vec!["irc", "ircs"],
      description: "Send messages via IRC.",
      attachment_support: false,
    }
  }
}

async fn irc_send(writer: &mut (impl AsyncWriteExt + Unpin), line: &str) -> Result<(), NotifyError> {
  writer.write_all(format!("{}\r\n", line).as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))
}

async fn irc_wait_for(reader: &mut (impl AsyncBufReadExt + Unpin), writer: &mut (impl AsyncWriteExt + Unpin), code: &str) -> Result<(), NotifyError> {
  let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
  let mut buf = String::new();
  loop {
    buf.clear();
    match tokio::time::timeout_at(deadline, reader.read_line(&mut buf)).await {
      Ok(Ok(0)) => return Err(NotifyError::Other("IRC connection closed".into())),
      Ok(Ok(_)) => {
        if buf.starts_with("PING") {
          let pong = buf.replace("PING", "PONG");
          let _ = irc_send(writer, pong.trim()).await;
        }
        if buf.contains(code) {
          return Ok(());
        }
        if buf.contains("ERROR") || buf.contains(" 433 ") || buf.contains(" 462 ") {
          return Err(NotifyError::Other(format!("IRC error: {}", buf.trim())));
        }
      }
      Ok(Err(e)) => return Err(NotifyError::Other(e.to_string())),
      Err(_) => return Err(NotifyError::Other("IRC timeout".into())),
    }
  }
}

#[async_trait]
impl Notify for Irc {
  fn schemas(&self) -> &[&str] {
    &["irc", "ircs"]
  }
  fn service_name(&self) -> &str {
    "IRC"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

    let tcp = TcpStream::connect(format!("{}:{}", self.host, self.port)).await.map_err(|e| NotifyError::Other(format!("IRC connect failed: {}", e)))?;

    if self.secure {
      let mut root_store = rustls::RootCertStore::empty();
      root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
      let config = rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
      let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
      let domain = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| NotifyError::Other(format!("Invalid hostname for TLS: {}", e)))?;
      let tls_stream = connector.connect(domain, tcp).await.map_err(|e| NotifyError::Other(format!("IRC TLS handshake failed: {}", e)))?;
      let (reader, mut writer) = tokio::io::split(tls_stream);
      let mut reader = BufReader::new(reader);
      self.irc_session(&mut reader, &mut writer, &msg).await
    } else {
      let (reader, mut writer) = tokio::io::split(tcp);
      let mut reader = BufReader::new(reader);
      self.irc_session(&mut reader, &mut writer, &msg).await
    }
  }
}

impl Irc {
  async fn irc_session(&self, reader: &mut (impl AsyncBufReadExt + Unpin), writer: &mut (impl AsyncWriteExt + Unpin), msg: &str) -> Result<bool, NotifyError> {
    if self.auth_mode == IrcAuthMode::Server {
      if let Some(ref pass) = self.password {
        irc_send(writer, &format!("PASS {}", pass)).await?;
      }
    }

    irc_send(writer, &format!("NICK {}", self.nick)).await?;
    irc_send(writer, &format!("USER {} 0 * :{}", self.user, self.realname)).await?;
    irc_wait_for(reader, writer, "001").await?;

    if self.auth_mode == IrcAuthMode::NickServ {
      if let Some(ref pass) = self.password {
        irc_send(writer, &format!("PRIVMSG NickServ :IDENTIFY {}", pass)).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
      }
    }

    for channel in &self.channels {
      irc_send(writer, &format!("JOIN {}", channel)).await?;
      for chunk in msg.as_bytes().chunks(380) {
        let chunk_str = String::from_utf8_lossy(chunk);
        irc_send(writer, &format!("PRIVMSG {} :{}", channel, chunk_str)).await?;
      }
    }

    for user in &self.users {
      for chunk in msg.as_bytes().chunks(380) {
        let chunk_str = String::from_utf8_lossy(chunk);
        irc_send(writer, &format!("PRIVMSG {} :{}", user, chunk_str)).await?;
      }
    }

    irc_send(writer, "QUIT :Apprise notification sent").await?;
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_valid_urls() {
    let valid_urls = vec!["irc://irc.freenode.net/channel", "ircs://irc.freenode.net/channel", "irc://irc.freenode.net:6667/channel"];
    for url in &valid_urls {
      let parsed = ParsedUrl::parse(url);
      assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
      let parsed = parsed.unwrap();
      assert!(Irc::from_url(&parsed).is_some(), "Irc::from_url returned None for valid URL: {}", url,);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let invalid_urls = vec![
      "irc://",
      // No channels or users
      "irc://irc.example.com",
    ];
    for url in &invalid_urls {
      let result = ParsedUrl::parse(url).and_then(|p| Irc::from_url(&p));
      assert!(result.is_none(), "Irc::from_url should return None for: {}", url,);
    }
  }

  #[test]
  fn test_irc_default_port() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.host, "irc.freenode.net");
    assert_eq!(irc.port, 6667);
    assert!(!irc.secure);
  }

  #[test]
  fn test_ircs_default_port() {
    let parsed = ParsedUrl::parse("ircs://irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.port, 6697);
    assert!(irc.secure);
  }

  #[test]
  fn test_irc_custom_port() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net:7000/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.port, 7000);
  }

  #[test]
  fn test_irc_channel_hash_prefix() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert!(irc.channels.contains(&"#channel".to_string()));
  }

  #[test]
  fn test_irc_channel_already_prefixed() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/%23mychannel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert!(irc.channels.iter().any(|c| c.contains("mychannel")));
  }

  #[test]
  fn test_irc_user_target() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/@bob").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert!(irc.users.contains(&"bob".to_string()));
  }

  #[test]
  fn test_irc_nick_from_url() {
    let parsed = ParsedUrl::parse("irc://mynick@irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.nick, "mynick");
    assert_eq!(irc.user, "mynick");
  }

  #[test]
  fn test_irc_nick_from_query() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/channel?nick=CustomNick").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.nick, "CustomNick");
  }

  #[test]
  fn test_irc_default_nick() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.nick, "Apprise");
  }

  #[test]
  fn test_irc_password() {
    let parsed = ParsedUrl::parse("irc://user:mypass@irc.freenode.net/channel").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.password.as_deref(), Some("mypass"));
  }

  #[test]
  fn test_irc_to_query_targets() {
    let parsed = ParsedUrl::parse("irc://irc.freenode.net/chan1?to=%23chan2,@alice").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert!(irc.channels.len() >= 2);
    assert!(irc.users.contains(&"alice".to_string()));
  }

  #[test]
  fn test_irc_auth_mode_nickserv() {
    let parsed = ParsedUrl::parse("irc://user:pass@irc.freenode.net/channel?mode=nickserv").unwrap();
    let irc = Irc::from_url(&parsed).unwrap();
    assert_eq!(irc.auth_mode, IrcAuthMode::NickServ);
  }

  #[test]
  fn test_irc_static_details() {
    let details = Irc::static_details();
    assert_eq!(details.service_name, "IRC");
    assert_eq!(details.protocols, vec!["irc", "ircs"]);
    assert!(!details.attachment_support);
  }
}

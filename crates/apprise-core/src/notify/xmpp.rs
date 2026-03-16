use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq)]
enum XmppSecureMode {
  None,
  StartTls,
  Tls,
}

pub struct Xmpp {
  host: String,
  port: u16,
  jid: String,
  password: String,
  targets: Vec<String>,
  secure_mode: XmppSecureMode,
  tags: Vec<String>,
}

impl Xmpp {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone()?;
    let user = url.user.clone()?;
    let password = url.password.clone()?;

    let mode_str = url.get("mode").unwrap_or("").to_string();
    let secure_mode = if url.schema == "xmpps" {
      match mode_str.to_lowercase().as_str() {
        "none" => XmppSecureMode::None,
        "starttls" => XmppSecureMode::StartTls,
        _ => XmppSecureMode::Tls,
      }
    } else {
      match mode_str.to_lowercase().as_str() {
        "tls" => XmppSecureMode::Tls,
        "none" => XmppSecureMode::None,
        _ => XmppSecureMode::StartTls,
      }
    };

    let port = url.port.unwrap_or(match secure_mode {
      XmppSecureMode::Tls => 5223,
      _ => 5222,
    });

    let jid = if user.contains('@') { user } else { format!("{}@{}", user, host) };
    let targets: Vec<String> = url.path_parts.iter().map(|t| if t.contains('@') { t.clone() } else { format!("{}@{}", t, host) }).collect();
    // Also check ?to= for additional targets
    let mut all_targets = targets;
    if let Some(to) = url.get("to") {
      for t in to.split(',').map(|s| s.trim()) {
        if !t.is_empty() {
          all_targets.push(if t.contains('@') { t.to_string() } else { format!("{}@{}", t, host) });
        }
      }
    }
    if all_targets.is_empty() {
      return None;
    }
    Some(Self { host, port, jid, password, targets: all_targets, secure_mode, tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "XMPP",
      service_url: Some("https://xmpp.org"),
      setup_url: None,
      protocols: vec!["xmpp", "xmpps"],
      description: "Send messages via XMPP/Jabber.",
      attachment_support: false,
    }
  }

  fn make_tls_connector(&self) -> Result<tokio_rustls::TlsConnector, NotifyError> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)))
  }

  fn server_name(&self) -> Result<rustls::pki_types::ServerName<'static>, NotifyError> {
    rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| NotifyError::Other(format!("Invalid hostname for TLS: {}", e)))
  }
}

async fn xmpp_write(w: &mut (impl AsyncWriteExt + Unpin), data: &str) -> Result<(), NotifyError> {
  w.write_all(data.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))
}

async fn xmpp_read(r: &mut (impl AsyncReadExt + Unpin)) -> Result<String, NotifyError> {
  let mut buf = vec![0u8; 8192];
  let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
  match tokio::time::timeout_at(deadline, r.read(&mut buf)).await {
    Ok(Ok(n)) if n > 0 => Ok(String::from_utf8_lossy(&buf[..n]).to_string()),
    Ok(Ok(_)) => Err(NotifyError::Other("XMPP connection closed".into())),
    Ok(Err(e)) => Err(NotifyError::Other(e.to_string())),
    Err(_) => Err(NotifyError::Other("XMPP timeout".into())),
  }
}

/// Run the XMPP session (auth + send messages) over any async reader/writer
async fn xmpp_session(
  reader: &mut (impl AsyncReadExt + Unpin),
  writer: &mut (impl AsyncWriteExt + Unpin),
  host: &str,
  jid: &str,
  password: &str,
  targets: &[String],
  msg: &str,
) -> Result<bool, NotifyError> {
  let stream_open =
    format!("<?xml version='1.0'?><stream:stream to='{}' xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' version='1.0'>", host);
  xmpp_write(writer, &stream_open).await?;
  let resp = xmpp_read(reader).await?;
  if !resp.contains("stream:features") {
    let _ = xmpp_read(reader).await;
  }

  // SASL PLAIN auth
  let user_part = jid.split('@').next().unwrap_or(jid);
  let auth_str = format!("\x00{}\x00{}", user_part, password);
  let auth_b64 = base64::engine::general_purpose::STANDARD.encode(auth_str.as_bytes());
  xmpp_write(writer, &format!("<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>{}</auth>", auth_b64)).await?;
  let resp = xmpp_read(reader).await?;
  if resp.contains("not-authorized") || resp.contains("failure") {
    return Err(NotifyError::Auth(format!("XMPP authentication failed for {}", jid)));
  }

  // Restart stream after auth
  xmpp_write(writer, &stream_open).await?;
  let _ = xmpp_read(reader).await;

  // Bind resource
  xmpp_write(writer, "<iq type='set' id='bind1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><resource>apprise</resource></bind></iq>").await?;
  let _ = xmpp_read(reader).await;

  // Send messages
  let mut all_ok = true;
  let msg_escaped = msg.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
  for target in targets {
    let stanza = format!("<message to='{}' type='chat'><body>{}</body></message>", target, msg_escaped);
    if let Err(e) = xmpp_write(writer, &stanza).await {
      tracing::warn!("XMPP send to {} failed: {}", target, e);
      all_ok = false;
    }
  }

  let _ = xmpp_write(writer, "</stream:stream>").await;
  Ok(all_ok)
}

#[async_trait]
impl Notify for Xmpp {
  fn schemas(&self) -> &[&str] {
    &["xmpp", "xmpps"]
  }
  fn service_name(&self) -> &str {
    "XMPP"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

    let tcp = TcpStream::connect(format!("{}:{}", self.host, self.port))
      .await
      .map_err(|e| NotifyError::Other(format!("XMPP connect to {}:{} failed: {}", self.host, self.port, e)))?;

    match self.secure_mode {
      XmppSecureMode::Tls => {
        // Direct TLS connection (port 5223)
        let connector = self.make_tls_connector()?;
        let domain = self.server_name()?;
        let tls_stream = connector.connect(domain, tcp).await.map_err(|e| NotifyError::Other(format!("XMPP TLS handshake failed: {}", e)))?;
        let (mut reader, mut writer) = tokio::io::split(tls_stream);
        xmpp_session(&mut reader, &mut writer, &self.host, &self.jid, &self.password, &self.targets, &msg).await
      }
      XmppSecureMode::None => {
        // Plain text connection
        let (mut reader, mut writer) = tokio::io::split(tcp);
        xmpp_session(&mut reader, &mut writer, &self.host, &self.jid, &self.password, &self.targets, &msg).await
      }
      XmppSecureMode::StartTls => {
        // Start plain, upgrade to TLS after STARTTLS
        // Use the raw stream (not split) for the STARTTLS handshake
        let mut tcp = tcp;
        let stream_open = format!(
          "<?xml version='1.0'?><stream:stream to='{}' xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' version='1.0'>",
          self.host
        );
        tcp.write_all(stream_open.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(tokio::time::Duration::from_secs(10), tcp.read(&mut buf))
          .await
          .map_err(|_| NotifyError::Other("XMPP timeout".into()))?
          .map_err(|e| NotifyError::Other(e.to_string()))?;
        let resp = String::from_utf8_lossy(&buf[..n]).to_string();
        if !resp.contains("stream:features") {
          let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), tcp.read(&mut buf)).await;
        }

        // Send STARTTLS
        tcp.write_all(b"<starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>").await.map_err(|e| NotifyError::Other(e.to_string()))?;
        let n = tokio::time::timeout(tokio::time::Duration::from_secs(10), tcp.read(&mut buf))
          .await
          .map_err(|_| NotifyError::Other("XMPP timeout".into()))?
          .map_err(|e| NotifyError::Other(e.to_string()))?;
        let resp = String::from_utf8_lossy(&buf[..n]).to_string();
        if !resp.contains("<proceed") {
          return Err(NotifyError::Other("XMPP STARTTLS not supported by server".into()));
        }

        // Upgrade to TLS
        let connector = self.make_tls_connector()?;
        let domain = self.server_name()?;
        let tls_stream = connector.connect(domain, tcp).await.map_err(|e| NotifyError::Other(format!("XMPP STARTTLS upgrade failed: {}", e)))?;
        let (mut reader, mut writer) = tokio::io::split(tls_stream);
        xmpp_session(&mut reader, &mut writer, &self.host, &self.jid, &self.password, &self.targets, &msg).await
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils::parse::ParsedUrl;

  #[test]
  fn test_valid_urls() {
    let valid_urls = vec!["xmpp://user:pass@localhost/target@example.com", "xmpps://user:pass@localhost/target@example.com"];
    for url in &valid_urls {
      let parsed = ParsedUrl::parse(url);
      assert!(parsed.is_some(), "ParsedUrl::parse failed for: {}", url);
      let parsed = parsed.unwrap();
      assert!(Xmpp::from_url(&parsed).is_some(), "Xmpp::from_url returned None for valid URL: {}", url,);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let invalid_urls = vec![
      "xmpp://",
      "xmpp://localhost/target",
      // No password
      "xmpp://user@localhost/target",
    ];
    for url in &invalid_urls {
      let result = ParsedUrl::parse(url).and_then(|p| Xmpp::from_url(&p));
      assert!(result.is_none(), "Xmpp::from_url should return None for: {}", url,);
    }
  }

  #[test]
  fn test_xmpp_struct_fields() {
    let parsed = ParsedUrl::parse("xmpp://user:pass@jabber.org/target@example.com").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.host, "jabber.org");
    assert_eq!(x.jid, "user@jabber.org");
    assert_eq!(x.password, "pass");
    assert!(x.targets.contains(&"target@example.com".to_string()));
  }

  #[test]
  fn test_xmpp_jid_with_at_sign() {
    let parsed = ParsedUrl::parse("xmpp://user%40jabber.org:pass@jabber.org/target@example.com").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert!(x.jid.contains("jabber.org"));
  }

  #[test]
  fn test_xmpps_default_port() {
    let parsed = ParsedUrl::parse("xmpps://user:pass@jabber.org/target@example.com").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.port, 5223);
    assert_eq!(x.secure_mode, XmppSecureMode::Tls);
  }

  #[test]
  fn test_xmpp_default_port() {
    let parsed = ParsedUrl::parse("xmpp://user:pass@jabber.org/target@example.com").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.port, 5222);
    assert_eq!(x.secure_mode, XmppSecureMode::StartTls);
  }

  #[test]
  fn test_xmpp_target_auto_domain() {
    // When target doesn't contain @, host is appended
    let parsed = ParsedUrl::parse("xmpp://user:pass@jabber.org/target").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert!(x.targets.contains(&"target@jabber.org".to_string()));
  }

  #[test]
  fn test_xmpp_to_query_param() {
    let parsed = ParsedUrl::parse("xmpp://user:pass@jabber.org/t1@example.com?to=t2@example.com,t3").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.targets.len(), 3);
    assert!(x.targets.contains(&"t2@example.com".to_string()));
    assert!(x.targets.contains(&"t3@jabber.org".to_string()));
  }

  #[test]
  fn test_xmpp_no_targets_fails() {
    let result = ParsedUrl::parse("xmpp://user:pass@jabber.org").and_then(|p| Xmpp::from_url(&p));
    assert!(result.is_none(), "XMPP without targets should fail");
  }

  #[test]
  fn test_xmpp_mode_none() {
    let parsed = ParsedUrl::parse("xmpp://user:pass@jabber.org/target?mode=none").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.secure_mode, XmppSecureMode::None);
  }

  #[test]
  fn test_xmpps_mode_starttls() {
    let parsed = ParsedUrl::parse("xmpps://user:pass@jabber.org/target?mode=starttls").unwrap();
    let x = Xmpp::from_url(&parsed).unwrap();
    assert_eq!(x.secure_mode, XmppSecureMode::StartTls);
  }

  #[test]
  fn test_xmpp_static_details() {
    let details = Xmpp::static_details();
    assert_eq!(details.service_name, "XMPP");
    assert_eq!(details.protocols, vec!["xmpp", "xmpps"]);
    assert!(!details.attachment_support);
  }
}

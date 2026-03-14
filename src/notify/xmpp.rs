use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Xmpp {
    host: String,
    port: u16,
    jid: String,
    password: String,
    targets: Vec<String>,
    secure: bool,
    tags: Vec<String>,
}

impl Xmpp {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let user = url.user.clone()?;
        let password = url.password.clone()?;
        let secure = url.schema == "xmpps";
        let port = url.port.unwrap_or(if secure { 5223 } else { 5222 });
        let jid = if user.contains('@') { user } else { format!("{}@{}", user, host) };
        let targets: Vec<String> = url.path_parts.iter().map(|t| {
            if t.contains('@') { t.clone() } else { format!("{}@{}", t, host) }
        }).collect();
        if targets.is_empty() { return None; }
        Some(Self { host, port, jid, password, targets, secure, tags: url.tags() })
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

#[async_trait]
impl Notify for Xmpp {
    fn schemas(&self) -> &[&str] { &["xmpp", "xmpps"] }
    fn service_name(&self) -> &str { "XMPP" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

        let stream = TcpStream::connect(format!("{}:{}", self.host, self.port))
            .await
            .map_err(|e| NotifyError::Other(format!("XMPP connect to {}:{} failed: {}", self.host, self.port, e)))?;

        let (mut reader, mut writer) = tokio::io::split(stream);

        // Open stream
        let stream_open = format!(
            "<?xml version='1.0'?><stream:stream to='{}' xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' version='1.0'>",
            self.host
        );
        xmpp_write(&mut writer, &stream_open).await?;
        let resp = xmpp_read(&mut reader).await?;

        if !resp.contains("stream:features") {
            // Read again for features
            let _ = xmpp_read(&mut reader).await;
        }

        // SASL PLAIN auth: base64(NUL + username + NUL + password)
        let user_part = self.jid.split('@').next().unwrap_or(&self.jid);
        let auth_str = format!("\x00{}\x00{}", user_part, self.password);
        let auth_b64 = base64::engine::general_purpose::STANDARD.encode(auth_str.as_bytes());
        let auth_xml = format!(
            "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>{}</auth>",
            auth_b64
        );
        xmpp_write(&mut writer, &auth_xml).await?;
        let resp = xmpp_read(&mut reader).await?;

        if resp.contains("not-authorized") || resp.contains("failure") {
            return Err(NotifyError::Auth(format!("XMPP authentication failed for {}", self.jid)));
        }

        // Restart stream after auth
        xmpp_write(&mut writer, &stream_open).await?;
        let _ = xmpp_read(&mut reader).await;

        // Bind resource
        let bind = "<iq type='set' id='bind1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><resource>apprise</resource></bind></iq>";
        xmpp_write(&mut writer, bind).await?;
        let _ = xmpp_read(&mut reader).await;

        // Send messages
        let mut all_ok = true;
        for target in &self.targets {
            let msg_escaped = msg
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let stanza = format!(
                "<message to='{}' type='chat'><body>{}</body></message>",
                target, msg_escaped
            );
            if let Err(e) = xmpp_write(&mut writer, &stanza).await {
                tracing::warn!("XMPP send to {} failed: {}", target, e);
                all_ok = false;
            }
        }

        // Close stream
        let _ = xmpp_write(&mut writer, "</stream:stream>").await;
        Ok(all_ok)
    }
}

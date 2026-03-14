use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

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
    tags: Vec<String>,
}

impl Irc {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema == "ircs";
        let port = url.port.unwrap_or(if secure { 6697 } else { 6667 });
        let nick = url.user.clone().unwrap_or_else(|| "Apprise".to_string());
        let user = nick.clone();
        let realname = url.get("name").unwrap_or("Apprise Notification").to_string();

        let mut channels = Vec::new();
        let mut users = Vec::new();
        for part in &url.path_parts {
            if part.starts_with('#') || part.starts_with('&') {
                channels.push(part.clone());
            } else if part.starts_with('@') {
                users.push(part[1..].to_string());
            } else {
                // Treat as channel by default
                channels.push(format!("#{}", part));
            }
        }
        if channels.is_empty() && users.is_empty() { return None; }

        Some(Self {
            host, port,
            password: url.password.clone(),
            nick, user, realname,
            channels, users, secure,
            tags: url.tags(),
        })
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

async fn irc_send_line(writer: &mut (impl AsyncWriteExt + Unpin), line: &str) -> Result<(), NotifyError> {
    let data = format!("{}\r\n", line);
    writer.write_all(data.as_bytes()).await.map_err(|e| NotifyError::Other(e.to_string()))
}

async fn irc_wait_for(reader: &mut (impl AsyncBufReadExt + Unpin), code: &str) -> Result<(), NotifyError> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    loop {
        buf.clear();
        let read_fut = reader.read_line(&mut buf);
        match tokio::time::timeout_at(deadline, read_fut).await {
            Ok(Ok(0)) => return Err(NotifyError::Other("IRC connection closed".into())),
            Ok(Ok(_)) => {
                // Respond to PING
                if buf.starts_with("PING") {
                    // Can't write back here, but we'll handle PING in the main loop
                }
                if buf.contains(code) { return Ok(()); }
                // Check for errors
                if buf.contains("ERROR") || buf.contains("433") || buf.contains("462") {
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
    fn schemas(&self) -> &[&str] { &["irc", "ircs"] }
    fn service_name(&self) -> &str { "IRC" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

        let stream = TcpStream::connect(format!("{}:{}", self.host, self.port))
            .await
            .map_err(|e| NotifyError::Other(format!("IRC connect failed: {}", e)))?;

        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // Send PASS if we have a password
        if let Some(ref pass) = self.password {
            irc_send_line(&mut writer, &format!("PASS {}", pass)).await?;
        }

        // Register
        irc_send_line(&mut writer, &format!("NICK {}", self.nick)).await?;
        irc_send_line(&mut writer, &format!("USER {} 0 * :{}", self.user, self.realname)).await?;

        // Wait for welcome (001) or MOTD end (376/422)
        irc_wait_for(&mut reader, "001").await?;

        // Join channels and send messages
        for channel in &self.channels {
            irc_send_line(&mut writer, &format!("JOIN {}", channel)).await?;
            // Send message in chunks of 380 bytes (IRC line limit)
            for chunk in msg.as_bytes().chunks(380) {
                let chunk_str = String::from_utf8_lossy(chunk);
                irc_send_line(&mut writer, &format!("PRIVMSG {} :{}", channel, chunk_str)).await?;
            }
        }

        // Send to users
        for user in &self.users {
            for chunk in msg.as_bytes().chunks(380) {
                let chunk_str = String::from_utf8_lossy(chunk);
                irc_send_line(&mut writer, &format!("PRIVMSG {} :{}", user, chunk_str)).await?;
            }
        }

        // Quit
        irc_send_line(&mut writer, "QUIT :Apprise notification sent").await?;
        Ok(true)
    }
}

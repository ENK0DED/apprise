use async_trait::async_trait;
use lettre::{
    message::{header::ContentType, Mailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Email {
    smtp_host: String,
    smtp_port: u16,
    secure: SecureMode,
    from: String,
    from_name: Option<String>,
    to: Vec<String>,
    user: Option<String>,
    password: Option<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum SecureMode {
    Plain,
    Ssl,
    StartTls,
}

impl Email {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // mailto://user:password@host/target1/target2?from=from@example.com
        // mailtos://... (SMTPS/SSL)
        let host = url.host.clone()?;

        let secure = match url.schema.as_str() {
            "mailtos" => {
                let mode = url.get("mode").unwrap_or("");
                match mode {
                    "ssl" => SecureMode::Ssl,
                    "insecure" | "plain" => SecureMode::Plain,
                    _ => SecureMode::StartTls,
                }
            }
            _ => {
                let mode = url.get("mode").unwrap_or("");
                match mode {
                    "ssl" => SecureMode::Ssl,
                    "starttls" => SecureMode::StartTls,
                    _ => SecureMode::Plain,
                }
            }
        };

        let default_port = match &secure {
            SecureMode::Ssl => 465,
            SecureMode::StartTls => 587,
            SecureMode::Plain => 25,
        };
        let smtp_port = url.port.unwrap_or(default_port);

        let user = url.user.clone();
        let password = url.password.clone();

        // Determine from address
        let from = url
            .get("from")
            .map(|s| s.to_string())
            .or_else(|| user.as_ref().map(|u| {
                if u.contains('@') {
                    u.clone()
                } else {
                    format!("{}@{}", u, host)
                }
            }))
            .unwrap_or_else(|| format!("noreply@{}", host));

        let from_name = url.get("name").map(|s| s.to_string());

        // Collect targets from path + "to" param
        let mut to: Vec<String> = url.path_parts.clone();
        if let Some(t) = url.get("to") {
            to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        if to.is_empty() {
            to.push(from.clone());
        }

        Some(Self {
            smtp_host: host,
            smtp_port,
            secure,
            from,
            from_name,
            to,
            user,
            password,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Email (SMTP)",
            service_url: None,
            setup_url: Some("https://github.com/caronc/apprise/wiki/Notify_email"),
            protocols: vec!["mailto", "mailtos"],
            description: "Send notifications via SMTP email.",
            attachment_support: true,
        }
    }
}

#[async_trait]
impl Notify for Email {
    fn schemas(&self) -> &[&str] { &["mailto", "mailtos"] }
    fn service_name(&self) -> &str { "Email (SMTP)" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    fn attachment_support(&self) -> bool { true }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let from_mailbox: Mailbox = if let Some(ref name) = self.from_name {
            format!("{} <{}>", name, self.from)
                .parse()
                .map_err(|e| NotifyError::Email(format!("Invalid from address: {}", e)))?
        } else {
            self.from
                .parse()
                .map_err(|e| NotifyError::Email(format!("Invalid from address: {}", e)))?
        };

        let subject = if ctx.title.is_empty() {
            "Apprise Notification".to_string()
        } else {
            ctx.title.clone()
        };

        let mut all_ok = true;

        for to_addr in &self.to {
            let to_mailbox: Mailbox = to_addr
                .parse()
                .map_err(|e| NotifyError::Email(format!("Invalid to address {}: {}", to_addr, e)))?;

            let email = Message::builder()
                .from(from_mailbox.clone())
                .to(to_mailbox)
                .subject(&subject)
                .body(ctx.body.clone())
                .map_err(|e| NotifyError::Email(e.to_string()))?;

            let result = match &self.secure {
                SecureMode::Ssl => {
                    let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&self.smtp_host)
                        .map_err(|e| NotifyError::Email(e.to_string()))?
                        .port(self.smtp_port)
                        .credentials(self.make_creds())
                        .build();
                    transport.send(email).await
                }
                SecureMode::StartTls => {
                    let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.smtp_host)
                        .map_err(|e| NotifyError::Email(e.to_string()))?
                        .port(self.smtp_port)
                        .credentials(self.make_creds())
                        .build();
                    transport.send(email).await
                }
                SecureMode::Plain => {
                    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.smtp_host)
                        .port(self.smtp_port)
                        .credentials(self.make_creds())
                        .build();
                    transport.send(email).await
                }
            };

            match result {
                Ok(_) => tracing::info!("Email sent to {}", to_addr),
                Err(e) => {
                    tracing::warn!("Email to {} failed: {}", to_addr, e);
                    all_ok = false;
                }
            }
        }
        Ok(all_ok)
    }
}

impl Email {
    fn make_creds(&self) -> Credentials {
        match (&self.user, &self.password) {
            (Some(u), Some(p)) => Credentials::new(u.clone(), p.clone()),
            (Some(u), None) => Credentials::new(u.clone(), String::new()),
            _ => Credentials::new(String::new(), String::new()),
        }
    }
}

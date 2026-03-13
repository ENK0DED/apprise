use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Ntfy {
    host: Option<String>,
    port: Option<u16>,
    topics: Vec<String>,
    secure: bool,
    priority: &'static str,
    auth: Option<NtfyAuth>,
    verify_certificate: bool,
    tags: Vec<String>,
}

enum NtfyAuth {
    Basic { user: String, pass: String },
    Token(String),
}

impl Ntfy {
    const CLOUD_HOST: &'static str = "ntfy.sh";

    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // ntfy://topic  (cloud mode)
        // ntfy://host/topic  or  ntfys://host/topics
        // ntfy://user:pass@host/topic
        // ntfy://token@host/topic  (if user starts with "tk_")

        let secure = url.schema == "ntfys";

        // Determine host and topics
        let (host, topics): (Option<String>, Vec<String>) = match &url.host {
            None => (None, vec![]),
            Some(h) if url.path_parts.is_empty() => {
                // ntfy://topic  — host IS the topic, use cloud
                (None, vec![h.clone()])
            }
            Some(h) => {
                // ntfy://host/topic1/topic2
                (Some(h.clone()), url.path_parts.clone())
            }
        };

        if topics.is_empty() {
            return None;
        }

        let auth = match (&url.user, &url.password) {
            (Some(u), _) if u.starts_with("tk_") => {
                Some(NtfyAuth::Token(u.clone()))
            }
            (Some(u), Some(p)) => {
                Some(NtfyAuth::Basic { user: u.clone(), pass: p.clone() })
            }
            _ => None,
        };

        let priority = url.get("priority").map(|p| match p.to_lowercase().as_str() {
            "min" | "1" => "min",
            "low" | "2" => "low",
            "high" | "4" => "high",
            "max" | "urgent" | "5" => "max",
            _ => "default",
        }).unwrap_or("default");

        Some(Self {
            host,
            port: url.port,
            topics,
            secure,
            priority,
            auth,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Ntfy",
            service_url: Some("https://ntfy.sh"),
            setup_url: Some("https://docs.ntfy.sh/publish/"),
            protocols: vec!["ntfy", "ntfys"],
            description: "Send notifications via ntfy.sh (self-hosted or cloud).",
            attachment_support: false,
        }
    }

    fn base_url(&self) -> String {
        let schema = if self.secure { "https" } else { "http" };
        match (&self.host, self.port) {
            (Some(h), Some(p)) => format!("{}://{}:{}", schema, h, p),
            (Some(h), None) => format!("{}://{}", schema, h),
            _ => format!("https://{}", Self::CLOUD_HOST),
        }
    }
}

#[async_trait]
impl Notify for Ntfy {
    fn schemas(&self) -> &[&str] { &["ntfy", "ntfys"] }
    fn service_name(&self) -> &str { "Ntfy" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let base = self.base_url();
        let mut all_ok = true;

        for topic in &self.topics {
            let url = format!("{}/{}", base, topic);
            let mut req = client
                .post(&url)
                .header("User-Agent", APP_ID)
                .header("X-Priority", self.priority)
                .header("X-Markdown", "yes");

            if !ctx.title.is_empty() {
                req = req.header("X-Title", &ctx.title);
            }

            req = match &self.auth {
                Some(NtfyAuth::Basic { user, pass }) => {
                    req.basic_auth(user, Some(pass))
                }
                Some(NtfyAuth::Token(t)) => {
                    req.header("Authorization", format!("Bearer {}", t))
                }
                None => req,
            };

            let resp = req.body(ctx.body.clone()).send().await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!("Ntfy send to {} failed: {}", topic, body);
                all_ok = false;
            }
        }
        Ok(all_ok)
    }
}

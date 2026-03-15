use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Gotify {
    host: String,
    port: Option<u16>,
    token: String,
    path: String,
    secure: bool,
    priority: i32,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Gotify {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // gotify://host/token  or  gotifys://host/token
        // gotify://host:port/path/token
        let host = url.host.clone()?;
        let token = url.path_parts.last()?.clone();
        if token.is_empty() { return None; }

        // Path prefix is everything except the last component
        let path = if url.path_parts.len() > 1 {
            format!("/{}/", url.path_parts[..url.path_parts.len()-1].join("/"))
        } else {
            "/".to_string()
        };

        let priority = url.get("priority").and_then(|p| {
            match p.to_lowercase().as_str() {
                "l" | "low" | "1" => Some(1),
                "m" | "moderate" | "3" => Some(3),
                "n" | "normal" | "5" => Some(5),
                "h" | "high" | "8" => Some(8),
                "e" | "emergency" | "10" => Some(10),
                n => n.parse().ok(),
            }
        }).unwrap_or(5);

        Some(Self {
            host,
            port: url.port,
            token,
            path,
            secure: url.schema.ends_with('s'),
            priority,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Gotify",
            service_url: Some("https://gotify.net"),
            setup_url: Some("https://gotify.net/docs/pushmsg"),
            protocols: vec!["gotify", "gotifys"],
            description: "Send notifications via Gotify self-hosted push server.",
            attachment_support: false,
        }
    }
}

#[async_trait]
impl Notify for Gotify {
    fn schemas(&self) -> &[&str] { &["gotify", "gotifys"] }
    fn service_name(&self) -> &str { "Gotify" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let api_url = format!("{}://{}{}{}message", schema, self.host, port_str, self.path);

        let mut payload = json!({
            "title": ctx.title,
            "message": ctx.body,
            "priority": self.priority,
        });

        // Include markdown extras only when format is Markdown (matching Python)
        if ctx.body_format == crate::types::NotifyFormat::Markdown {
            payload["extras"] = json!({
                "client::display": { "contentType": "text/markdown" }
            });
        }

        let client = build_client(self.verify_certificate)?;
        let resp = client
            .post(&api_url)
            .header("User-Agent", APP_ID)
            .header("X-Gotify-Key", &self.token)
            .json(&payload)
            .send()
            .await?;

        if resp.status().is_success() {
            tracing::info!("Gotify notification sent");
            Ok(true)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(NotifyError::ServiceError { status, body })
        }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "gotify://",
            "gotify://hostname",
            "gotify://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

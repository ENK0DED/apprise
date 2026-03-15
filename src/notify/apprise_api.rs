use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct AppriseApi {
    host: String,
    port: Option<u16>,
    token: String,
    secure: bool,
    user: Option<String>,
    password: Option<String>,
    tags: Vec<String>,
    verify_certificate: bool,
}

impl AppriseApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        // Validate token — reject special chars
        if !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        // Validate method if provided
        if let Some(method) = url.get("method") {
            match method.to_lowercase().as_str() {
                "form" | "json" | "" => {}
                _ => return None,
            }
        }
        Some(Self {
            host, port: url.port, token,
            secure: url.schema == "apprises",
            user: url.user.clone(), password: url.password.clone(),
            tags: url.tags(), verify_certificate: url.verify_certificate(),
        })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Apprise API", service_url: Some("https://github.com/caronc/apprise-api"), setup_url: None, protocols: vec!["apprise", "apprises"], description: "Send via the Apprise REST API.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for AppriseApi {
    fn schemas(&self) -> &[&str] { &["apprise", "apprises"] }
    fn service_name(&self) -> &str { "Apprise API" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let url = format!("{}://{}{}/notify/{}", schema, self.host, port_str, self.token);
        let mut payload = json!({ "title": ctx.title, "body": ctx.body, "type": ctx.notify_type.as_str() });
        if !ctx.attachments.is_empty() {
            payload["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                "filename": att.name,
                "base64": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "mimetype": att.mime_type,
            })).collect::<Vec<_>>());
        }
        let client = build_client(self.verify_certificate)?;
        let mut req = client.post(&url).header("User-Agent", APP_ID).json(&payload);
        if let (Some(u), Some(p)) = (&self.user, &self.password) { req = req.basic_auth(u, Some(p)); }
        let resp = req.send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "apprise://user@localhost/mytoken0/?format=markdown",
            "apprise://user@localhost/mytoken1/",
            "apprise://localhost:8080/mytoken/",
            "apprise://user:pass@localhost:8080/mytoken2/",
            "apprises://localhost/mytoken/",
            "apprises://user:pass@localhost/mytoken3/",
            "apprises://localhost:8080/mytoken4/",
            "apprises://localhost:8080/abc123/?method=json",
            "apprises://localhost:8080/abc123/?method=form",
            "apprises://user:password@localhost:8080/mytoken5/",
            "apprises://localhost:8080/path?+HeaderKey=HeaderValue",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "apprise://",
            "apprise://:@/",
            "apprise://localhost",
            "apprise://localhost/!",
            "apprise://localhost/%%20",
            "apprises://localhost:8080/abc123/?method=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

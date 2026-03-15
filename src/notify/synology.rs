use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Synology {
    host: String, port: u16, token: String, secure: bool,
    user: Option<String>, password: Option<String>,
    verify_certificate: bool, tags: Vec<String>,
}

impl Synology {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        if host.is_empty() { return None; }
        // Token from first path part or ?token= query param
        let token = url.path_parts.first().cloned()
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.trim().is_empty() { return None; }
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 5001 } else { 5000 });
        Some(Self {
            host, port, token, secure,
            user: url.user.clone(), password: url.password.clone(),
            verify_certificate: url.verify_certificate(), tags: url.tags(),
        })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Synology Chat", service_url: Some("https://www.synology.com"), setup_url: None, protocols: vec!["synology", "synologys"], description: "Send via Synology Chat.", attachment_support: false } }
}

#[async_trait]
impl Notify for Synology {
    fn schemas(&self) -> &[&str] { &["synology", "synologys"] }
    fn service_name(&self) -> &str { "Synology Chat" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };

        let payload_str = serde_json::to_string(&json!({ "text": text })).unwrap();
        let params = [
            ("api", "SYNO.Chat.External"),
            ("method", "incoming"),
            ("version", "2"),
            ("token", self.token.as_str()),
        ];
        let url = format!("{}://{}:{}/webapi/entry.cgi", schema, self.host, self.port);

        let mut req = client.post(&url)
            .header("User-Agent", APP_ID)
            .query(&params)
            .body(format!("payload={}", urlencoding::encode(&payload_str)));

        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            req = req.basic_auth(u, Some(p));
        }

        let resp = req.send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "synology://localhost/token",
            "synology://localhost/token?file_url=http://reddit.com/test.jpg",
            "synology://user:pass@localhost/token",
            "synology://user@localhost/token",
            "synology://localhost:8080/token",
            "synology://user:pass@localhost:8080/token",
            "synologys://localhost/token",
            "synologys://localhost/?token=mytoken",
            "synologys://user:pass@localhost/token",
            "synologys://localhost:8080/token/path/",
            "synologys://user:password@localhost:8080/token",
            "synology://localhost:8080/path?+HeaderKey=HeaderValue",
            "synology://user:pass@localhost:8083/token",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "synology://:@/",
            "synology://",
            "synologys://",
            "synology://user@localhost",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

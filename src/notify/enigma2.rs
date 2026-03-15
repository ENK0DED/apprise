use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Enigma2 { host: String, port: u16, user: Option<String>, password: Option<String>, secure: bool, verify_certificate: bool, tags: Vec<String> }
impl Enigma2 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let secure = url.schema.ends_with('s');
        let port = url.port.unwrap_or(if secure { 443 } else { 80 });
        Some(Self { host, port, user: url.user.clone(), password: url.password.clone(), secure, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Enigma2", service_url: None, setup_url: None, protocols: vec!["enigma2", "enigma2s"], description: "Send notifications to Enigma2 receivers.", attachment_support: false } }
}
#[async_trait]
impl Notify for Enigma2 {
    fn schemas(&self) -> &[&str] { &["enigma2", "enigma2s"] }
    fn service_name(&self) -> &str { "Enigma2" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let schema = if self.secure { "https" } else { "http" };
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let url = format!("{}://{}:{}/web/message?text={}&type=1", schema, self.host, self.port, urlencoding::encode(&msg));
        let mut req = client.get(&url).header("User-Agent", APP_ID);
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
            "enigma2://localhost",
            "enigma2://user@localhost",
            "enigma2://user@localhost?timeout=-1",
            "enigma2://user@localhost?timeout=-1000",
            "enigma2://user@localhost?timeout=invalid",
            "enigma2://user:pass@localhost",
            "enigma2://localhost:8080",
            "enigma2://user:pass@localhost:8080",
            "enigma2s://localhost",
            "enigma2s://user:pass@localhost",
            "enigma2s://localhost:8080/path/",
            "enigma2s://user:pass@localhost:8080",
            "enigma2://localhost:8080/path?+HeaderKey=HeaderValue",
            "enigma2://user:pass@localhost:8083",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "enigma2://:@/",
            "enigma2://",
            "enigma2s://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct PushJet { host: Option<String>, port: Option<u16>, secret: String, secure: bool, user: Option<String>, password: Option<String>, verify_certificate: bool, tags: Vec<String> }
impl PushJet {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // pjet://host/secret or pjet://user:pass@host?secret=X
        let (host, secret) = if let Some(sec) = url.get("secret") {
            (url.host.clone(), sec.to_string())
        } else if !url.path_parts.is_empty() {
            let sec = url.path_parts.last()?.clone();
            (url.host.clone(), sec)
        } else {
            return None;
        };
        if secret.is_empty() { return None; }
        // Secret must be at least 32 characters
        if secret.len() < 32 { return None; }
        Some(Self { host, port: url.port, secret, secure: url.schema == "pjets", user: url.user.clone(), password: url.password.clone(), verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushjet", service_url: Some("https://pushjet.io"), setup_url: None, protocols: vec!["pjet", "pjets"], description: "Send push notifications via Pushjet.", attachment_support: false } }
}
#[async_trait]
impl Notify for PushJet {
    fn schemas(&self) -> &[&str] { &["pjet", "pjets"] }
    fn service_name(&self) -> &str { "Pushjet" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let base = match &self.host {
            Some(h) => { let schema = if self.secure { "https" } else { "http" }; let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default(); format!("{}://{}{}", schema, h, port_str) },
            None => "https://api.pushjet.io".to_string(),
        };
        let url = format!("{}/message?secret={}&message={}&title={}", base, self.secret, urlencoding::encode(&ctx.body), urlencoding::encode(&ctx.title));
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "pjet://user:pass@localhost/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pjets://localhost/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pjet://user:pass@localhost?secret=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pjets://localhost:8080/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pjet://localhost:8081/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pjet://",
            "pjets://",
            "pjet://:@/",
            "pjet://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

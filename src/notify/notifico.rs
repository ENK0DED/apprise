use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Notifico { project_id: String, msghook: String, verify_certificate: bool, tags: Vec<String> }
impl Notifico {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // notifico://project_id/msghook
        // or https://n.tkte.ch/h/project_id/msghook
        let (project_id, msghook) = if url.schema == "https" || url.schema == "http" {
            // https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj
            // path_parts: ["h", "2144", "uJmKaBW9WFk42miB146ci3Kj"]
            let h_idx = url.path_parts.iter().position(|p| p == "h")?;
            let pid = url.path_parts.get(h_idx + 1)?.clone();
            let mh = url.path_parts.get(h_idx + 2)?.clone();
            (pid, mh)
        } else {
            let pid = url.host.clone()?;
            let mh = url.path_parts.first()?.clone();
            (pid, mh)
        };
        // Project ID must be numeric
        if project_id.is_empty() || !project_id.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(Self { project_id, msghook, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Notifico", service_url: Some("https://notico.re"), setup_url: None, protocols: vec!["notifico"], description: "Send IRC notifications via Notifico.", attachment_support: false } }
}
#[async_trait]
impl Notify for Notifico {
    fn schemas(&self) -> &[&str] { &["notifico"] }
    fn service_name(&self) -> &str { "Notifico" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let msg = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) };
        let url = format!("https://notico.re/api/{}/{}", self.project_id, self.msghook);
        let client = build_client(self.verify_certificate)?;
        let resp = client.get(&url).header("User-Agent", APP_ID).query(&[("msg", msg.as_str())]).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "notifico://1234/ckhrjW8w672m6HG",
            "notifico://1234/ckhrjW8w672m6HG?prefix=no",
            "notifico://1234/ckhrjW8w672m6HG?color=yes",
            "notifico://1234/ckhrjW8w672m6HG?color=yes",
            "notifico://1234/ckhrjW8w672m6HG?color=yes",
            "notifico://1234/ckhrjW8w672m6HG?color=yes",
            "notifico://1234/ckhrjW8w672m6HG?color=yes",
            "notifico://1234/ckhrjW8w672m6HG?color=no",
            "https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj",
            "notifico://1234/ckhrjW8w672m6HG",
            "notifico://1234/ckhrjW8w672m6HG",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "notifico://",
            "notifico://:@/",
            "notifico://1234",
            "notifico://abcd/ckhrjW8w672m6HG",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

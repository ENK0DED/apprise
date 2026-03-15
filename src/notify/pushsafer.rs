use async_trait::async_trait;
use base64::Engine;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Pushsafer { privatekey: String, verify_certificate: bool, tags: Vec<String> }
impl Pushsafer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> { Some(Self { privatekey: url.host.clone()?, verify_certificate: url.verify_certificate(), tags: url.tags() }) }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Pushsafer", service_url: Some("https://www.pushsafer.com"), setup_url: None, protocols: vec!["psafer", "psafers"], description: "Send push notifications via Pushsafer.", attachment_support: true } }
}
#[async_trait]
impl Notify for Pushsafer {
    fn schemas(&self) -> &[&str] { &["psafer", "psafers"] }
    fn service_name(&self) -> &str { "Pushsafer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let mut params: Vec<(String, String)> = vec![
            ("k".into(), self.privatekey.clone()),
            ("t".into(), ctx.title.clone()),
            ("m".into(), ctx.body.clone()),
            ("d".into(), "a".into()),
            ("s".into(), "11".into()),
            ("v".into(), "1".into()),
        ];
        // Attach up to 3 image attachments as data URLs (p, p2, p3)
        let image_attachments: Vec<_> = ctx.attachments.iter()
            .filter(|att| att.mime_type.starts_with("image/"))
            .take(3)
            .collect();
        let pic_keys = ["p", "p2", "p3"];
        for (i, att) in image_attachments.iter().enumerate() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let data_url = format!("data:{};base64,{}", att.mime_type, b64);
            params.push((pic_keys[i].into(), data_url));
        }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://www.pushsafer.com/api").header("User-Agent", APP_ID).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "psafer://:@/",
            "psafer://",
            "psafers://",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

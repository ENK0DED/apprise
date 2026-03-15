use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Msg91 {
    authkey: String,
    template: String,
    targets: Vec<String>,
    short_url: bool,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Msg91 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // msg91://template_id@authkey/phone1/phone2
        let authkey = url.host.clone()?;
        if authkey.is_empty() || authkey == "_" || authkey == "-" { return None; }
        let template = url.user.clone()?;
        if template.is_empty() { return None; }
        let mut targets = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let short_url = url.get("short_url").map(crate::utils::parse::parse_bool).unwrap_or(false);
        Some(Self { authkey, template, targets, short_url, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "MSG91", service_url: Some("https://msg91.com"), setup_url: None, protocols: vec!["msg91"], description: "Send SMS via MSG91.", attachment_support: false } }
}

#[async_trait]
impl Notify for Msg91 {
    fn schemas(&self) -> &[&str] { &["msg91"] }
    fn service_name(&self) -> &str { "MSG91" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);
        let recipients: Vec<_> = self.targets.iter().map(|t| json!({ "mobiles": t })).collect();
        let payload = json!({
            "template_id": self.template,
            "short_url": if self.short_url { 1 } else { 0 },
            "recipients": recipients,
        });
        let resp = client.post("https://control.msg91.com/api/v5/flow/")
            .header("User-Agent", APP_ID)
            .header("authkey", &self.authkey)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "msg91://tttttttttttttttttttt@aaaaaaaaaaaaaaaaaaaaaaa",
            "msg91://tttttttttttttttttttt@aaaaaaaaaaaaaaaaaaaaaaa/abcd",
            "msg91://tttttttttttttttttttt@aaaaaaaaaaaaaaaaaaaaaaa/15551232000",
            "msg91://tttttttttttttttttttt@aaaaaaaaaaaaaaaaaaaaaaa/?to=15551232000&short_url=no",
            "msg91://tttttttttttttttttttt@aaaaaaaaaaaaaaaaaaaaaaa/15551232000?short_url=yes",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "msg91://",
            "msg91://-",
            "msg91://aaaaaaaaaaaaaaaaaaaaaaa",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Office365 { tenant: String, client_id: String, client_secret: String, from: String, targets: Vec<String>, cc: Vec<String>, bcc: Vec<String>, content_type: String, verify_certificate: bool, tags: Vec<String> }
impl Office365 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let client_id = url.user.clone()?;
        let client_secret = url.password.clone()?;
        let tenant = url.host.clone()?;
        let from = url.path_parts.first().cloned()?;
        let targets: Vec<String> = url.path_parts.iter().skip(1).cloned().collect();
        if targets.is_empty() { return None; }
        let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let content_type = url.get("format").unwrap_or("text").to_string();
        Some(Self { tenant, client_id, client_secret, from, targets, cc, bcc, content_type, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Office365 Email", service_url: Some("https://office.com"), setup_url: None, protocols: vec!["o365", "azure"], description: "Send email via Office 365 / Microsoft Graph.", attachment_support: true } }
}
#[async_trait]
impl Notify for Office365 {
    fn schemas(&self) -> &[&str] { &["o365", "azure"] }
    fn service_name(&self) -> &str { "Office365 Email" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        // Get access token
        let token_url = format!("https://login.microsoftonline.com/{}/oauth2/v2.0/token", self.tenant);
        let token_params = [("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str()), ("scope", "https://graph.microsoft.com/.default"), ("grant_type", "client_credentials")];
        let token_resp = client.post(&token_url).header("User-Agent", APP_ID).form(&token_params).send().await?;
        if !token_resp.status().is_success() { return Ok(false); }
        let token_json: serde_json::Value = token_resp.json().await?;
        let access_token = token_json["access_token"].as_str().ok_or_else(|| NotifyError::Other("No access token".into()))?;
        let to_recipients: Vec<_> = self.targets.iter().map(|t| json!({ "emailAddress": { "address": t } })).collect();
        let ct = if self.content_type.to_lowercase() == "html" { "HTML" } else { "Text" };
        let mut message = json!({ "subject": ctx.title, "body": { "contentType": ct, "content": ctx.body }, "toRecipients": to_recipients });
        if !self.cc.is_empty() {
            message["ccRecipients"] = json!(self.cc.iter().map(|t| json!({ "emailAddress": { "address": t } })).collect::<Vec<_>>());
        }
        if !self.bcc.is_empty() {
            message["bccRecipients"] = json!(self.bcc.iter().map(|t| json!({ "emailAddress": { "address": t } })).collect::<Vec<_>>());
        }
        // Add attachments
        if !ctx.attachments.is_empty() {
            let attachments: Vec<_> = ctx.attachments.iter().map(|att| {
                json!({
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "name": att.name,
                    "contentBytes": base64::engine::general_purpose::STANDARD.encode(&att.data),
                    "contentType": att.mime_type,
                })
            }).collect();
            message["attachments"] = json!(attachments);
        }
        let mail_payload = json!({ "message": message, "saveToSentItems": "false" });
        let send_url = format!("https://graph.microsoft.com/v1.0/users/{}/sendMail", self.from);
        let resp = client.post(&send_url).header("User-Agent", APP_ID).bearer_auth(access_token).json(&mail_payload).send().await?;
        Ok(resp.status().is_success())
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "o365://",
            "o365://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

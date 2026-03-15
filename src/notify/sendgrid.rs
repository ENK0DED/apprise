use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct SendGrid {
    apikey: String,
    from_email: String,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl SendGrid {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // sendgrid://apikey:from_name@from_domain/to@email
        let (apikey, from_email, to) = if url.user.is_some() && url.password.is_some() {
            let apikey = url.user.clone()?;
            let from_email = format!("{}@{}", url.password.as_ref()?, url.host.as_ref()?);
            let mut to: Vec<String> = url.path_parts.clone();
            if let Some(t) = url.get("to") {
                to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            (apikey, from_email, to)
        } else {
            let apikey = url.host.clone()?;
            let from_email = url.path_parts.first()?.clone();
            let mut to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
            if let Some(t) = url.get("to") {
                to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            (apikey, from_email, to)
        };
        // Validate API key — must be alphanumeric with - and _
        if !apikey.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        Some(Self { apikey, from_email, to, cc, bcc, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "SendGrid", service_url: Some("https://sendgrid.com"), setup_url: None, protocols: vec!["sendgrid"], description: "Send email via SendGrid.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for SendGrid {
    fn schemas(&self) -> &[&str] { &["sendgrid"] }
    fn service_name(&self) -> &str { "SendGrid" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let personalizations: Vec<_> = self.to.iter().map(|t| {
            let mut p = json!({ "to": [{ "email": t }] });
            if !self.cc.is_empty() {
                p["cc"] = json!(self.cc.iter().map(|e| json!({"email": e})).collect::<Vec<_>>());
            }
            if !self.bcc.is_empty() {
                p["bcc"] = json!(self.bcc.iter().map(|e| json!({"email": e})).collect::<Vec<_>>());
            }
            p
        }).collect();
        let mut payload = json!({
            "personalizations": personalizations,
            "from": { "email": self.from_email },
            "subject": ctx.title,
            "content": [{ "type": "text/plain", "value": ctx.body }]
        });
        if !ctx.attachments.is_empty() {
            payload["attachments"] = json!(ctx.attachments.iter().map(|att| json!({
                "content": base64::engine::general_purpose::STANDARD.encode(&att.data),
                "filename": att.name,
                "type": att.mime_type,
            })).collect::<Vec<_>>());
        }
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.sendgrid.com/v3/mail/send").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.apikey)).json(&payload).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 202 { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "sendgrid://abcd:user@example.com",
            "sendgrid://abcd:user@example.com/newuser@example.com",
            "sendgrid://abcd:user@example.com/bademailaddress",
            "sendgrid://abcd:user@example.com/newuser@example.com?bcc=l2g@nuxref.com",
            "sendgrid://abcd:user@example.com/newuser@example.com?cc=l2g@nuxref.com",
            "sendgrid://abcd:user@example.com/newuser@example.com?to=l2g@nuxref.com",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "sendgrid://",
            "sendgrid://:@/",
            "sendgrid://abcd",
            "sendgrid://abcd@host",
            "sendgrid://invalid-api-key+*-d:user@example.com",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

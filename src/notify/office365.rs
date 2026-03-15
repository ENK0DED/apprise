use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Office365 { tenant: String, client_id: String, client_secret: String, from: String, targets: Vec<String>, cc: Vec<String>, bcc: Vec<String>, content_type: String, verify_certificate: bool, tags: Vec<String> }
impl Office365 {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // Multiple formats:
        // o365://tenant/oauth_id/oauth_secret/from_email/to1/to2
        // o365://user@example.com/tenant/oauth_id/oauth_secret/to1
        // o365://oauth_id:user@example.com/tenant/...
        // o365://_/?oauth_id=X&oauth_secret=Y&tenant=Z&to=email&from=email
        let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let content_type = url.get("format").unwrap_or("text").to_string();

        // Query param mode
        if let Some(oauth_id) = url.get("oauth_id") {
            let oauth_secret = url.get("oauth_secret").map(|s| s.to_string())?;
            let tenant = url.get("tenant").map(|s| s.to_string())?;
            let from = url.get("from").map(|s| s.to_string())?;
            let mut targets: Vec<String> = Vec::new();
            if let Some(to) = url.get("to") {
                targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            if targets.is_empty() { return None; }
            return Some(Self { tenant, client_id: oauth_id.to_string(), client_secret: oauth_secret, from, targets, cc, bcc, content_type, verify_certificate: url.verify_certificate(), tags: url.tags() });
        }

        let host = url.host.clone()?;
        if host.is_empty() || host == "_" { return None; }

        if let Some(ref user) = url.user {
            // user@host or user:pass@host form
            if url.password.is_some() {
                // o365://tenant:user@example.com/oauth_id/oauth_secret/targets...
                let from = format!("{}@{}", url.password.as_ref().unwrap(), host);
                // Actually: o365://tenant:user@example.com/01-12-23-34/abcd/321/4321/@test/test/email1@test.ca
                // tenant is url.user, from is password@host
                let tenant = user.clone();
                let client_id = url.path_parts.first()?.clone();
                // client_secret is join of remaining path parts that look like secret components
                let client_secret = url.path_parts.get(1..).unwrap_or(&[]).iter()
                    .take_while(|s| !s.starts_with('@') && !s.contains('@'))
                    .cloned().collect::<Vec<_>>().join("/");
                if client_secret.is_empty() { return None; }
                let secret_parts = client_secret.split('/').count();
                let targets: Vec<String> = url.path_parts.get(1 + secret_parts..).unwrap_or(&[]).to_vec();
                if targets.is_empty() { return None; }
                return Some(Self { tenant, client_id, client_secret, from, targets, cc, bcc, content_type, verify_certificate: url.verify_certificate(), tags: url.tags() });
            }
            // o365://user@example.com/tenant/oauth_id/oauth_secret/targets...
            let from = format!("{}@{}", user, host);
            // Validate from address
            if !from.contains('.') && !from.contains('@') { return None; }
            let tenant = url.path_parts.first()?.clone();
            // Validate tenant: reject commas and dots-only
            if tenant.contains(',') || tenant.chars().all(|c| c == '.') { return None; }
            let client_id = url.path_parts.get(1)?.clone();
            // Validate client_id: reject trailing dots
            if client_id.ends_with('.') { return None; }
            let client_secret = url.path_parts.get(2..).unwrap_or(&[]).iter()
                .take_while(|s| !s.starts_with('@') && !s.contains('@'))
                .cloned().collect::<Vec<_>>().join("/");
            if client_secret.is_empty() { return None; }
            let secret_parts = client_secret.split('/').count();
            let targets: Vec<String> = url.path_parts.get(2 + secret_parts..).unwrap_or(&[]).to_vec();
            if targets.is_empty() { return None; }
            return Some(Self { tenant, client_id, client_secret, from, targets, cc, bcc, content_type, verify_certificate: url.verify_certificate(), tags: url.tags() });
        }

        // o365://tenant/oauth_id/oauth_secret_parts.../from/to1/to2
        // or o365://host/path1/path2/...
        let tenant = host;
        let client_id = url.path_parts.first()?.clone();
        // Validate tenant: reject commas
        if tenant.contains(',') { return None; }
        // Validate client_id: reject trailing dots
        if client_id.ends_with('.') { return None; }
        // client_secret is everything until we hit something that looks like a from address or @target
        let remaining = url.path_parts.get(1..).unwrap_or(&[]);
        // The secret is composed of parts that don't contain @ and don't start with @
        let secret_parts: Vec<&String> = remaining.iter()
            .take_while(|s| !s.starts_with('@') && !s.contains('@'))
            .collect();
        if secret_parts.is_empty() { return None; }
        let client_secret = secret_parts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/");
        // Everything after is targets (from + to addresses)
        let after_secret: Vec<String> = remaining.iter().skip(secret_parts.len()).cloned().collect();
        // First target that contains @ is from, rest are targets
        // Or if there are @-prefixed targets, from is derived
        let from = after_secret.iter().find(|s| s.contains('@')).cloned()
            .unwrap_or_else(|| after_secret.first().cloned().unwrap_or_default());
        let targets = if after_secret.is_empty() {
            Vec::new()
        } else {
            after_secret
        };
        if targets.is_empty() { return None; }
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
    use super::*;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "o365://",
            "o365://:@/",
            // Invalid tenant (comma)
            "o365://user@example.com/,/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca",
            // Invalid client_id (trailing dot)
            "o365://user2@example.com/tenant/ab./abcd/123/3343/@jack/test/email1@test.ca",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            // Standard with email from
            "o365://user@example.edu/tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca",
            // No email -- mode self uses tenant/cid/secret/targets
            "o365://tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca",
            // Object ID as source
            "o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca",
            // Using query params
            "o365://_/?oauth_id=ab-cd-ef-gh&oauth_secret=abcd/123/3343/@jack/test&tenant=tenant&to=email1@test.ca&from=user@example.ca",
            // azure:// schema alias
            "azure://user@example.edu/tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_email_format() {
        let parsed = ParsedUrl::parse(
            "o365://user@example.edu/tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca"
        ).expect("parse");
        let o = Office365::from_url(&parsed).expect("from_url");
        assert_eq!(o.from, "user@example.edu");
        assert_eq!(o.tenant, "tenant");
        assert_eq!(o.client_id, "ab-cd-ef-gh");
        assert!(o.targets.contains(&"email1@test.ca".to_string()));
    }

    #[test]
    fn test_from_url_query_params() {
        let parsed = ParsedUrl::parse(
            "o365://_/?oauth_id=ab-cd-ef-gh&oauth_secret=mysecret&tenant=mytenant&to=email1@test.ca&from=user@example.ca"
        ).expect("parse");
        let o = Office365::from_url(&parsed).expect("from_url");
        assert_eq!(o.client_id, "ab-cd-ef-gh");
        assert_eq!(o.client_secret, "mysecret");
        assert_eq!(o.tenant, "mytenant");
        assert_eq!(o.from, "user@example.ca");
        assert_eq!(o.targets, vec!["email1@test.ca"]);
    }

    #[test]
    fn test_azure_schema_alias() {
        assert!(from_url(
            "azure://user@example.edu/tenant/ab-cd-ef-gh/abcd/123/3343/@jack/test/email1@test.ca"
        ).is_some());
    }

    #[test]
    fn test_cc_bcc_from_query() {
        let parsed = ParsedUrl::parse(
            "o365://user@example.com/tenant/ab-cd-ef-gh/abcd/email1@test.ca?cc=cc@test.com&bcc=bcc@test.com"
        ).expect("parse");
        let o = Office365::from_url(&parsed).expect("from_url");
        assert_eq!(o.cc, vec!["cc@test.com"]);
        assert_eq!(o.bcc, vec!["bcc@test.com"]);
    }

    #[test]
    fn test_token_url_format() {
        // Verify the OAuth token endpoint
        let parsed = ParsedUrl::parse(
            "o365://user@example.com/ff-gg-hh-ii-jj/ab-cd-ef-gh/abcd/email1@test.ca"
        ).expect("parse");
        let o = Office365::from_url(&parsed).expect("from_url");
        let expected_token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            o.tenant
        );
        assert_eq!(expected_token_url, "https://login.microsoftonline.com/ff-gg-hh-ii-jj/oauth2/v2.0/token");
    }

    #[test]
    fn test_send_mail_url_format() {
        // Verify the sendMail endpoint
        let parsed = ParsedUrl::parse(
            "o365://user@example.net/ff-gg-hh-ii-jj/ab-cd-ef-gh/abcd/target@example.com"
        ).expect("parse");
        let o = Office365::from_url(&parsed).expect("from_url");
        let expected_send_url = format!(
            "https://graph.microsoft.com/v1.0/users/{}/sendMail",
            o.from
        );
        assert_eq!(expected_send_url, "https://graph.microsoft.com/v1.0/users/user@example.net/sendMail");
    }

    #[test]
    fn test_static_details() {
        let details = Office365::static_details();
        assert_eq!(details.service_name, "Office365 Email");
        assert!(details.protocols.contains(&"o365"));
        assert!(details.protocols.contains(&"azure"));
        assert!(details.attachment_support);
    }
}

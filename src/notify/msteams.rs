use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct MsTeams {
    webhook_url: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl MsTeams {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // msteams://TokenA/TokenB/TokenC[/TokenD]
        // or msteams://Team@TokenA/TokenB/TokenC/TokenD  (v3 with team name)
        let token_a = url.host.clone()?;
        let team = url.user.clone();
        let parts = &url.path_parts;
        if parts.len() < 2 { return None; }
        let token_b = &parts[0];
        let token_c = &parts[1];
        let token_d = parts.get(2);

        let webhook_url = if let Some(td) = token_d {
            // v3 format with token_d
            if let Some(ref team_name) = team {
                format!(
                    "https://{}.webhook.office.com/webhookb2/{}/IncomingWebhook/{}/{}/{}",
                    team_name, token_a, token_b, token_c, td
                )
            } else {
                format!(
                    "https://outlook.office.com/webhook/{}/IncomingWebhook/{}/{}/{}",
                    token_a, token_b, token_c, td
                )
            }
        } else {
            // v1/v2 format without token_d
            format!(
                "https://outlook.office.com/webhook/{}/IncomingWebhook/{}/{}",
                token_a, token_b, token_c
            )
        };

        // Validate version if provided
        if let Some(version) = url.get("version") {
            match version.to_lowercase().as_str() {
                "1" | "2" | "3" | "4" | "" => {}
                _ => return None,
            }
        }

        Some(Self { webhook_url, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Microsoft Teams", service_url: Some("https://teams.microsoft.com"), setup_url: None, protocols: vec!["msteams"], description: "Send via Microsoft Teams incoming webhooks.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for MsTeams {
    fn schemas(&self) -> &[&str] { &["msteams"] }
    fn service_name(&self) -> &str { "Microsoft Teams" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let color = ctx.notify_type.color_hex();
        let payload = json!({
            "@type": "MessageCard",
            "@context": "https://schema.org/extensions",
            "summary": ctx.title,
            "themeColor": color.trim_start_matches('#'),
            "sections": [{
                "activityTitle": ctx.title,
                "activityText": ctx.body,
            }]
        });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&self.webhook_url).header("User-Agent", APP_ID).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "msteams://",
            "msteams://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

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
    use super::*;

    const UUID4: &str = "8b799edf-6f98-4d3a-9be7-2862fb4e5752";

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "msteams://".to_string(),
            "msteams://:@/".to_string(),
            // Only half of token_a
            format!("msteams://{}", UUID4),
            // Only 1 token (token_a@token_a but no token_b/token_c)
            format!("msteams://{}@{}/", UUID4, UUID4),
            // Only 2 tokens
            format!("msteams://{}@{}/{}", UUID4, UUID4, "a".repeat(32)),
            // Invalid version
            format!("msteams://apprise/{}@{}/{}/{}?version=999", UUID4, UUID4, "e".repeat(32), UUID4),
            format!("msteams://apprise/{}@{}/{}/{}?version=invalid", UUID4, UUID4, "e".repeat(32), UUID4),
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            // All 3 tokens -- good
            format!("msteams://{}@{}/{}/{}", UUID4, UUID4, "b".repeat(32), UUID4),
            // Legacy URL
            format!("msteams://{}@{}/{}/{}", UUID4, UUID4, "c".repeat(32), UUID4),
            // With team name (v2)
            format!("msteams://apprise/{}@{}/{}/{}", UUID4, UUID4, "e".repeat(32), UUID4),
            // team= argument
            format!("msteams://{}@{}/{}/{}?team=teamname", UUID4, UUID4, "f".repeat(32), UUID4),
            // Force v1
            format!("msteams://apprise/{}@{}/{}/{}?version=1", UUID4, UUID4, "e".repeat(32), UUID4),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_webhook_url_v1_format() {
        let url_str = format!("msteams://{}@{}/{}/{}", UUID4, UUID4, "a".repeat(32), UUID4);
        let parsed = ParsedUrl::parse(&url_str).expect("parse");
        let ms = MsTeams::from_url(&parsed).expect("from_url");
        assert!(ms.webhook_url.starts_with("https://outlook.office.com/webhook/"));
        assert!(ms.webhook_url.contains("/IncomingWebhook/"));
    }

    #[test]
    fn test_webhook_url_with_team_in_user() {
        // In Rust impl, team comes from url.user. When user is set and
        // there are 3+ path parts, token_d triggers v2 team format.
        let url_str = format!(
            "msteams://myteam@{}/{}/{}/{}",
            UUID4, "m".repeat(32), UUID4, "extra"
        );
        let parsed = ParsedUrl::parse(&url_str).expect("parse");
        let ms = MsTeams::from_url(&parsed).expect("from_url");
        assert!(ms.webhook_url.starts_with("https://myteam.webhook.office.com/webhookb2/"));
        assert!(ms.webhook_url.contains("/IncomingWebhook/"));
    }

    #[test]
    fn test_native_url_v1() {
        let url_str = format!(
            "https://outlook.office.com/webhook/{}@{}/IncomingWebhook/{}/{}",
            UUID4, UUID4, "k".repeat(32), UUID4
        );
        assert!(from_url(&url_str).is_some(), "Should parse native v1 URL");
    }

    #[test]
    fn test_native_url_v2() {
        let url_str = format!(
            "https://myteam.webhook.office.com/webhookb2/{}@{}/IncomingWebhook/{}/{}",
            UUID4, UUID4, "m".repeat(32), UUID4
        );
        assert!(from_url(&url_str).is_some(), "Should parse native v2 URL");
    }

    #[test]
    fn test_static_details() {
        let details = MsTeams::static_details();
        assert_eq!(details.service_name, "Microsoft Teams");
        assert!(details.protocols.contains(&"msteams"));
        assert!(!details.attachment_support);
    }
}

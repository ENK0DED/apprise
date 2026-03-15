use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Pushbullet { apikey: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }

impl Pushbullet {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let apikey = url.host.clone()?;
        let targets = url.path_parts.clone();
        Some(Self { apikey, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Pushbullet", service_url: Some("https://pushbullet.com"), setup_url: None, protocols: vec!["pbul"], description: "Send push notifications via Pushbullet.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Pushbullet {
    fn schemas(&self) -> &[&str] { &["pbul"] }
    fn service_name(&self) -> &str { "Pushbullet" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;

        // If no targets, send to all devices (no target field)
        let targets: Vec<Option<&String>> = if self.targets.is_empty() {
            vec![None]
        } else {
            self.targets.iter().map(Some).collect()
        };

        for target in targets {
            let mut payload = json!({ "type": "note", "title": ctx.title, "body": ctx.body });

            if let Some(t) = target {
                // Determine target type: email, channel_tag, or device_iden
                if t.contains('@') {
                    payload["email"] = json!(t);
                } else if t.starts_with('#') {
                    payload["channel_tag"] = json!(&t[1..]);
                } else {
                    payload["device_iden"] = json!(t);
                }
            }

            let resp = client.post("https://api.pushbullet.com/v2/pushes")
                .header("User-Agent", APP_ID)
                .header("Access-Token", self.apikey.as_str())
                .json(&payload)
                .send().await?;

            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/#channel/",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/?to=#channel",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/#channel1/#channel2",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/device/",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/device1/device2/",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/user@example.com/",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/user@example.com/abc@def.com/",
            "pbul://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/device/#channel/user@example.com/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "pbul://",
            "pbul://:@/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

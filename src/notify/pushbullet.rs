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

    fn set_target(payload: &mut serde_json::Value, t: &str) {
        if t.contains('@') {
            payload["email"] = json!(t);
        } else if t.starts_with('#') {
            payload["channel_tag"] = json!(&t[1..]);
        } else {
            payload["device_iden"] = json!(t);
        }
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Pushbullet", service_url: Some("https://pushbullet.com"), setup_url: None, protocols: vec!["pbul"], description: "Send push notifications via Pushbullet.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Pushbullet {
    fn schemas(&self) -> &[&str] { &["pbul"] }
    fn service_name(&self) -> &str { "Pushbullet" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    fn attachment_support(&self) -> bool { true }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let mut all_ok = true;

        // If no targets, send to all devices (no target field)
        let targets: Vec<Option<&String>> = if self.targets.is_empty() {
            vec![None]
        } else {
            self.targets.iter().map(Some).collect()
        };

        // Upload attachments first (if any)
        let mut uploaded_files: Vec<(String, String, String)> = Vec::new(); // (file_url, file_name, file_type)
        for attachment in &ctx.attachments {
            // Step 1: Request upload URL
            let upload_req = json!({
                "file_name": attachment.name,
                "file_type": attachment.mime_type,
            });
            let resp = client.post("https://api.pushbullet.com/v2/upload-request")
                .header("User-Agent", APP_ID)
                .header("Access-Token", self.apikey.as_str())
                .json(&upload_req)
                .send().await?;

            if !resp.status().is_success() {
                all_ok = false;
                continue;
            }

            let upload_resp: serde_json::Value = resp.json().await?;
            let upload_url = match upload_resp["upload_url"].as_str() {
                Some(u) => u.to_string(),
                None => { all_ok = false; continue; }
            };
            let file_url = match upload_resp["file_url"].as_str() {
                Some(u) => u.to_string(),
                None => { all_ok = false; continue; }
            };

            // Step 2: Upload file via multipart form
            let part = reqwest::multipart::Part::bytes(attachment.data.clone())
                .file_name(attachment.name.clone())
                .mime_str(&attachment.mime_type)
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(attachment.data.clone())
                        .file_name(attachment.name.clone())
                });
            let form = reqwest::multipart::Form::new().part("file", part);
            let resp = client.post(&upload_url)
                .header("User-Agent", APP_ID)
                .multipart(form)
                .send().await?;

            if !resp.status().is_success() {
                all_ok = false;
                continue;
            }

            uploaded_files.push((file_url, attachment.name.clone(), attachment.mime_type.clone()));
        }

        for target in &targets {
            if uploaded_files.is_empty() {
                // Standard note push (no attachments)
                let mut payload = json!({ "type": "note", "title": ctx.title, "body": ctx.body });
                if let Some(t) = target {
                    Self::set_target(&mut payload, t);
                }
                let resp = client.post("https://api.pushbullet.com/v2/pushes")
                    .header("User-Agent", APP_ID)
                    .header("Access-Token", self.apikey.as_str())
                    .json(&payload)
                    .send().await?;
                if !resp.status().is_success() { all_ok = false; }
            } else {
                // Step 3: Send a "file" type push for each uploaded file
                for (file_url, file_name, file_type) in &uploaded_files {
                    let mut payload = json!({
                        "type": "file",
                        "file_url": file_url,
                        "file_name": file_name,
                        "file_type": file_type,
                        "body": ctx.body,
                    });
                    if let Some(t) = target {
                        Self::set_target(&mut payload, t);
                    }
                    let resp = client.post("https://api.pushbullet.com/v2/pushes")
                        .header("User-Agent", APP_ID)
                        .header("Access-Token", self.apikey.as_str())
                        .json(&payload)
                        .send().await?;
                    if !resp.status().is_success() { all_ok = false; }
                }
            }
        }
        Ok(all_ok)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let a32 = "a".repeat(32);
        let urls = vec![
            format!("pbul://{}", a32),
            format!("pbul://{}/#channel/", a32),
            format!("pbul://{}/?to=#channel", a32),
            format!("pbul://{}/#channel1/#channel2", a32),
            format!("pbul://{}/device/", a32),
            format!("pbul://{}/device1/device2/", a32),
            format!("pbul://{}/user@example.com/", a32),
            format!("pbul://{}/user@example.com/abc@def.com/", a32),
            format!("pbul://{}/device/#channel/user@example.com/", a32),
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

    #[test]
    fn test_from_url_basic_fields() {
        let a32 = "a".repeat(32);
        let url = format!("pbul://{}", a32);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let obj = Pushbullet::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, a32);
        // No explicit targets means empty targets vec (sends to all devices)
        assert!(obj.targets.is_empty());
    }

    #[test]
    fn test_from_url_with_targets() {
        let a32 = "a".repeat(32);
        let url = format!("pbul://{}/device/#channel/user@example.com/", a32);
        let parsed = crate::utils::parse::ParsedUrl::parse(&url).unwrap();
        let obj = Pushbullet::from_url(&parsed).unwrap();
        assert_eq!(obj.apikey, a32);
        assert!(obj.targets.contains(&"device".to_string()));
        assert!(obj.targets.contains(&"#channel".to_string()));
        assert!(obj.targets.contains(&"user@example.com".to_string()));
    }

    #[test]
    fn test_set_target_email() {
        let mut payload = serde_json::json!({"type": "note"});
        Pushbullet::set_target(&mut payload, "user@example.com");
        assert_eq!(payload["email"], "user@example.com");
    }

    #[test]
    fn test_set_target_channel() {
        let mut payload = serde_json::json!({"type": "note"});
        Pushbullet::set_target(&mut payload, "#channel");
        assert_eq!(payload["channel_tag"], "channel");
    }

    #[test]
    fn test_set_target_device() {
        let mut payload = serde_json::json!({"type": "note"});
        Pushbullet::set_target(&mut payload, "device");
        assert_eq!(payload["device_iden"], "device");
    }

    #[test]
    fn test_service_details() {
        let details = Pushbullet::static_details();
        assert_eq!(details.service_name, "Pushbullet");
        assert!(details.protocols.contains(&"pbul"));
        assert_eq!(details.service_url, Some("https://pushbullet.com"));
        assert!(details.attachment_support);
    }
}

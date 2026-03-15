use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mastodon {
    host: String,
    port: Option<u16>,
    token: String,
    secure: bool,
    visibility: String,
    spoiler_text: Option<String>,
    language: Option<String>,
    sensitive: bool,
    targets: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mastodon {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let host = url.host.clone()?;
        let token = url.user.clone()
            .or_else(|| url.get("token").map(|s| s.to_string()))?;
        if token.is_empty() { return None; }
        let visibility = url.get("visibility").unwrap_or("public").to_string();
        // Validate visibility
        match visibility.to_lowercase().as_str() {
            "public" | "unlisted" | "private" | "direct" => {}
            _ => return None,
        }
        let spoiler_text = url.get("spoiler").map(|s| s.to_string());
        let language = url.get("language").or_else(|| url.get("lang")).map(|s| s.to_string());
        let sensitive = url.get("sensitive").map(crate::utils::parse::parse_bool).unwrap_or(false);
        let mut targets: Vec<String> = url.path_parts.clone();
        if let Some(to) = url.get("to") {
            targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Validate targets - each must start with @
        for t in &targets {
            if !t.starts_with('@') { return None; }
            // Must have valid content after the @
            let name = &t[1..];
            if name.is_empty() || name == "-" || name == "%" { return None; }
        }
        Some(Self {
            host, port: url.port, token,
            secure: url.schema.ends_with('s'),
            visibility, spoiler_text, language, sensitive, targets,
            verify_certificate: url.verify_certificate(), tags: url.tags(),
        })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mastodon", service_url: Some("https://mastodon.social"), setup_url: None, protocols: vec!["mastodon", "toot", "mastodons", "toots"], description: "Post a toot on Mastodon.", attachment_support: true }
    }
}

#[async_trait]
impl Notify for Mastodon {
    fn schemas(&self) -> &[&str] { &["mastodon", "toot", "mastodons", "toots"] }
    fn service_name(&self) -> &str { "Mastodon" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let schema = if self.secure { "https" } else { "http" };
        let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
        let base_url = format!("{}://{}{}", schema, self.host, port_str);
        let client = build_client(self.verify_certificate)?;

        // Upload attachments and collect media IDs (only image/video/audio supported)
        let mut media_ids: Vec<String> = Vec::new();
        for att in &ctx.attachments {
            if !(att.mime_type.starts_with("image/") || att.mime_type.starts_with("video/") || att.mime_type.starts_with("audio/")) {
                continue;
            }
            let upload_url = format!("{}/api/v1/media", base_url);
            let part = reqwest::multipart::Part::bytes(att.data.clone())
                .file_name(att.name.clone())
                .mime_str(&att.mime_type)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
            let form = reqwest::multipart::Form::new().part("file", part);
            let upload_resp = client.post(&upload_url)
                .header("User-Agent", APP_ID)
                .header("Authorization", format!("Bearer {}", self.token))
                .multipart(form)
                .send().await?;
            if upload_resp.status().is_success() {
                let media: Value = upload_resp.json().await.unwrap_or_default();
                if let Some(id) = media["id"].as_str() {
                    media_ids.push(id.to_string());
                }
            }
        }

        let status_url = format!("{}/api/v1/statuses", base_url);
        let status = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n\n{}", ctx.title, ctx.body) };

        let mut payload = json!({
            "status": status,
            "visibility": self.visibility,
            "sensitive": self.sensitive,
        });
        if let Some(ref spoiler) = self.spoiler_text { payload["spoiler_text"] = json!(spoiler); }
        if let Some(ref lang) = self.language { payload["language"] = json!(lang); }
        if !media_ids.is_empty() { payload["media_ids"] = json!(media_ids); }

        let resp = client.post(&status_url)
            .header("User-Agent", APP_ID)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&payload)
            .send().await?;

        if resp.status().is_success() { Ok(true) }
        else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;
    use super::*;
    use crate::asset::AppriseAsset;
    use crate::notify::{Notify, NotifyContext};
    use crate::types::{NotifyFormat, NotifyType};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx(title: &str, body: &str) -> NotifyContext {
        NotifyContext {
            body: body.to_string(),
            title: title.to_string(),
            notify_type: NotifyType::Info,
            body_format: NotifyFormat::Text,
            attachments: vec![],
            interpret_escapes: false,
            interpret_emojis: false,
            tags: vec![],
            asset: AppriseAsset::default(),
        }
    }

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "toot://access_token@hostname",
            "toots://access_token@hostname",
            "mastodon://access_token@hostname/@user/@user2",
            "mastodon://hostname/@user/@user2?token=abcd123",
            "mastodon://access_token@hostname?to=@user, @user2",
            "mastodon://access_token@hostname/?cache=no",
            "mastodon://access_token@hostname/?spoiler=spoiler%20text",
            "mastodon://access_token@hostname/?language=en",
            "mastodons://access_token@hostname:8443",
            "mastodon://access_token@hostname/?key=My%20Idempotency%20Key",
            "mastodon://access_token@hostname?visibility=direct",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "mastodon://",
            "mastodon://:@/",
            "mastodon://hostname",
            "mastodon://access_token@hostname/-/%/",
            "mastodon://access_token@hostname?visibility=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_fields() {
        let parsed = ParsedUrl::parse("mastodon://mytoken@nuxref.com/@user1/@user2?visibility=direct&spoiler=test&language=en&sensitive=yes").unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();
        assert_eq!(m.host, "nuxref.com");
        assert_eq!(m.token, "mytoken");
        assert_eq!(m.visibility, "direct");
        assert_eq!(m.spoiler_text, Some("test".to_string()));
        assert_eq!(m.language, Some("en".to_string()));
        assert!(m.sensitive);
        assert_eq!(m.targets, vec!["@user1", "@user2"]);
        assert!(!m.secure);
    }

    #[test]
    fn test_secure_flag() {
        let parsed = ParsedUrl::parse("mastodons://token@host:8443").unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();
        assert!(m.secure);
        assert_eq!(m.port, Some(8443));
    }

    #[test]
    fn test_token_via_query_param() {
        let parsed = ParsedUrl::parse("mastodon://hostname/@user?token=abcd123").unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();
        assert_eq!(m.token, "abcd123");
    }

    #[test]
    fn test_static_details() {
        let details = Mastodon::static_details();
        assert_eq!(details.service_name, "Mastodon");
        assert!(details.protocols.contains(&"toot"));
        assert!(details.protocols.contains(&"toots"));
        assert!(details.attachment_support);
    }

    #[tokio::test]
    async fn test_send_public_toot() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "12345"})))
            .expect(1)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!("mastodon://access_key@localhost:{}", addr.port());
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();

        let result = m.send(&ctx("test title", "test body")).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_send_status_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!("mastodon://access_key@localhost:{}", addr.port());
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();

        let result = m.send(&ctx("test", "body")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_with_attachment() {
        let server = MockServer::start().await;
        // Media upload
        Mock::given(method("POST"))
            .and(path("/api/v1/media"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "media123", "file_mime": "image/gif"})))
            .expect(1)
            .mount(&server)
            .await;
        // Status post
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "status456"})))
            .expect(1)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!("mastodon://access_key@localhost:{}", addr.port());
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();

        let mut c = ctx("title", "body");
        c.attachments.push(crate::notify::Attachment {
            name: "test.gif".to_string(),
            data: b"GIF89a".to_vec(),
            mime_type: "image/gif".to_string(),
        });

        let result = m.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_unsupported_attachment_skipped() {
        let server = MockServer::start().await;
        // Only the status post, no media upload for zip
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "1"})))
            .expect(1)
            .mount(&server)
            .await;

        let addr = server.address();
        let url_str = format!("mastodon://access_key@localhost:{}", addr.port());
        let parsed = ParsedUrl::parse(&url_str).unwrap();
        let m = Mastodon::from_url(&parsed).unwrap();

        let mut c = ctx("title", "body");
        c.attachments.push(crate::notify::Attachment {
            name: "archive.zip".to_string(),
            data: b"PK".to_vec(),
            mime_type: "application/zip".to_string(),
        });

        let result = m.send(&c).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }
}

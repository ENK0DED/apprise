use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct Fluxer { webhook_id: String, token: String, verify_certificate: bool, tags: Vec<String> }
impl Fluxer {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let webhook_id = url.host.clone()?;
        let token = url.path_parts.first()?.clone();
        if token.is_empty() { return None; }
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "private" | "public" | "" => {}
                _ => return None,
            }
        }
        // Validate flags if provided
        if let Some(flags) = url.get("flags") {
            if !flags.is_empty() {
                let val: i64 = flags.parse().ok()?;
                if val < 0 { return None; }
            }
        }
        Some(Self { webhook_id, token, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Fluxer", service_url: None, setup_url: None, protocols: vec!["fluxer", "fluxers"], description: "Send via Fluxer webhooks.", attachment_support: true } }
}
#[async_trait]
impl Notify for Fluxer {
    fn schemas(&self) -> &[&str] { &["fluxer", "fluxers"] }
    fn service_name(&self) -> &str { "Fluxer" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let url = format!("https://api.fluxer.io/webhooks/{}/{}", self.webhook_id, self.token);
        let text = if ctx.title.is_empty() { ctx.body.clone() } else { format!("**{}**\n{}", ctx.title, ctx.body) };
        let payload = json!({ "content": text });
        let client = build_client(self.verify_certificate)?;
        let resp = if !ctx.attachments.is_empty() {
            let payload_str = serde_json::to_string(&payload).unwrap_or_default();
            let mut form = reqwest::multipart::Form::new()
                .text("payload_json", payload_str);
            for (i, att) in ctx.attachments.iter().enumerate() {
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.name.clone())
                    .mime_str(&att.mime_type)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
                form = form.part(format!("files[{}]", i), part);
            }
            client.post(&url).header("User-Agent", APP_ID).multipart(form).send().await?
        } else {
            client.post(&url).header("User-Agent", APP_ID).json(&payload).send().await?
        };
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    fn tokens() -> (&'static str, &'static str) {
        ("0000000000", "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB")
    }

    #[test]
    fn test_valid_urls() {
        let (wid, wtk) = tokens();
        let urls = vec![
            format!("fluxer://{}/{}", wid, wtk),
            format!("fluxer://l2g@{}/{}", wid, wtk),
            format!("fluxer://{}/{}?format=markdown&footer=Yes&image=Yes&ping=Joe", wid, wtk),
            format!("fluxer://{}/{}?format=markdown&footer=Yes&image=No&fields=no", wid, wtk),
            format!("fluxer://{}/{}?format=markdown&avatar=No&footer=No", wid, wtk),
            format!("fluxer://{}/{}?flags=1", wid, wtk),
            format!("fluxer://{}/{}?format=markdown", wid, wtk),
            format!("fluxer://{}/{}?format=markdown&thread=abc123", wid, wtk),
            format!("fluxer://{}/{}?format=text", wid, wtk),
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let (wid, wtk) = tokens();
        let urls = vec![
            "fluxer://".to_string(),
            "fluxer://:@/".to_string(),
            // No webhook_token specified
            format!("fluxer://{}", wid),
            // Invalid mode
            format!("fluxer://{}/{}?mode=invalid", wid, wtk),
            // Negative flags
            format!("fluxer://{}/{}?flags=-1", wid, wtk),
            // Non-numeric flags
            format!("fluxer://{}/{}?flags=invalid", wid, wtk),
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    fn parse_fluxer(url: &str) -> Fluxer {
        let parsed = crate::utils::parse::ParsedUrl::parse(url).unwrap();
        Fluxer::from_url(&parsed).unwrap()
    }

    #[test]
    fn test_from_url_basic() {
        let (wid, wtk) = tokens();
        let f = parse_fluxer(&format!("fluxer://{}/{}", wid, wtk));
        assert_eq!(f.webhook_id, wid);
        assert_eq!(f.token, wtk);
    }

    #[test]
    fn test_from_url_with_user() {
        let (wid, wtk) = tokens();
        let f = parse_fluxer(&format!("fluxer://l2g@{}/{}", wid, wtk));
        assert_eq!(f.webhook_id, wid);
        assert_eq!(f.token, wtk);
    }

    #[test]
    fn test_from_url_no_token_returns_none() {
        let (wid, _) = tokens();
        let parsed = crate::utils::parse::ParsedUrl::parse(&format!("fluxer://{}", wid)).unwrap();
        assert!(Fluxer::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_mode_validation() {
        let (wid, wtk) = tokens();
        // private mode is valid
        let parsed = crate::utils::parse::ParsedUrl::parse(
            &format!("fluxer://{}/{}?mode=private", wid, wtk)
        ).unwrap();
        assert!(Fluxer::from_url(&parsed).is_some());

        // invalid mode returns None
        let parsed = crate::utils::parse::ParsedUrl::parse(
            &format!("fluxer://{}/{}?mode=invalid", wid, wtk)
        ).unwrap();
        assert!(Fluxer::from_url(&parsed).is_none());
    }

    #[test]
    fn test_from_url_flags_validation() {
        let (wid, wtk) = tokens();
        // Valid flags
        let f = parse_fluxer(&format!("fluxer://{}/{}?flags=1", wid, wtk));
        assert_eq!(f.webhook_id, wid);

        // Negative flags
        let parsed = crate::utils::parse::ParsedUrl::parse(
            &format!("fluxer://{}/{}?flags=-1", wid, wtk)
        ).unwrap();
        assert!(Fluxer::from_url(&parsed).is_none());
    }

    #[test]
    fn test_service_details() {
        let details = Fluxer::static_details();
        assert_eq!(details.service_name, "Fluxer");
        assert_eq!(details.protocols, vec!["fluxer", "fluxers"]);
        assert!(details.attachment_support);
    }
}

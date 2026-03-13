use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Twitter { consumer_key: String, consumer_secret: String, access_token: String, access_token_secret: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }

impl Twitter {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let consumer_key = url.user.clone()?;
        let consumer_secret = url.password.clone()?;
        let access_token = url.path_parts.get(0)?.clone();
        let access_token_secret = url.path_parts.get(1)?.clone();
        let targets: Vec<String> = url.path_parts.iter().skip(2).cloned().collect();
        Some(Self { consumer_key, consumer_secret, access_token, access_token_secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }

    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Twitter/X", service_url: Some("https://twitter.com"), setup_url: None, protocols: vec!["twitter", "x", "tweet"], description: "Send tweets or DMs via Twitter/X API.", attachment_support: false } }

    fn oauth1_header(&self, method: &str, url: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        use base64::Engine;
        use rand::Rng;

        let nonce: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        let timestamp = chrono::Utc::now().timestamp().to_string();

        fn pct(s: &str) -> String { urlencoding::encode(s).into_owned() }

        let mut params: Vec<(String, String)> = vec![
            ("oauth_consumer_key".into(), pct(&self.consumer_key)),
            ("oauth_nonce".into(), pct(&nonce)),
            ("oauth_signature_method".into(), "HMAC-SHA1".into()),
            ("oauth_timestamp".into(), timestamp.clone()),
            ("oauth_token".into(), pct(&self.access_token)),
            ("oauth_version".into(), "1.0".into()),
        ];
        params.sort();

        let param_string = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
        let base_string = format!("{}&{}&{}", method.to_uppercase(), pct(url), pct(&param_string));
        let signing_key = format!("{}&{}", pct(&self.consumer_secret), pct(&self.access_token_secret));

        let mut mac = Hmac::<Sha1>::new_from_slice(signing_key.as_bytes()).expect("HMAC accepts any key size");
        mac.update(base_string.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        format!(
            r#"OAuth oauth_consumer_key="{}", oauth_nonce="{}", oauth_signature="{}", oauth_signature_method="HMAC-SHA1", oauth_timestamp="{}", oauth_token="{}", oauth_version="1.0""#,
            pct(&self.consumer_key), pct(&nonce), pct(&sig), timestamp, pct(&self.access_token)
        )
    }
}

#[async_trait]
impl Notify for Twitter {
    fn schemas(&self) -> &[&str] { &["twitter", "x", "tweet"] }
    fn service_name(&self) -> &str { "Twitter/X" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

        if self.targets.is_empty() {
            let url = "https://api.twitter.com/2/tweets";
            let auth = self.oauth1_header("POST", url);
            let payload = json!({ "text": &msg[..msg.len().min(280)] });
            let resp = client.post(url).header("User-Agent", APP_ID).header("Authorization", auth).json(&payload).send().await?;
            Ok(resp.status().is_success())
        } else {
            let mut all_ok = true;
            for target in &self.targets {
                let url = format!("https://api.twitter.com/2/dm_conversations/with/{}/messages", target);
                let auth = self.oauth1_header("POST", &url);
                let payload = json!({ "text": &msg[..msg.len().min(10000)] });
                let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", auth).json(&payload).send().await?;
                if !resp.status().is_success() { all_ok = false; }
            }
            Ok(all_ok)
        }
    }
}

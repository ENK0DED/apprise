use async_trait::async_trait;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Twitter { consumer_key: String, consumer_secret: String, access_token: String, access_token_secret: String, targets: Vec<String>, verify_certificate: bool, tags: Vec<String> }

impl Twitter {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // twitter://ck:cs/at/ats or twitter://ck/cs/at/ats
        let (consumer_key, consumer_secret, access_token, access_token_secret, targets) = if url.password.is_some() {
            let ck = url.user.clone()?;
            let cs = url.password.clone()?;
            let at = url.path_parts.get(0)?.clone();
            let ats = url.path_parts.get(1)?.clone();
            let targets: Vec<String> = url.path_parts.iter().skip(2).cloned().collect();
            (ck, cs, at, ats, targets)
        } else {
            // All from host + path_parts
            let ck = url.host.clone()?;
            let cs = url.path_parts.get(0)?.clone();
            let at = url.path_parts.get(1)?.clone();
            let ats = url.path_parts.get(2)?.clone();
            let targets: Vec<String> = url.path_parts.iter().skip(3).cloned().collect();
            (ck, cs, at, ats, targets)
        };
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "tweet" | "dm" | "" => {}
                _ => return None,
            }
        }
        Some(Self { consumer_key, consumer_secret, access_token, access_token_secret, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }

    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "Twitter/X", service_url: Some("https://twitter.com"), setup_url: None, protocols: vec!["twitter", "x", "tweet"], description: "Send tweets or DMs via Twitter/X API.", attachment_support: true } }

    fn oauth1_header(&self, method: &str, url: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        use base64::Engine;
        use rand::Rng;

        let nonce: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
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
    fn body_maxlen(&self) -> usize { 280 }
    fn title_maxlen(&self) -> usize { 0 }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let msg = format!("{}{}", if ctx.title.is_empty() { String::new() } else { format!("{}: ", ctx.title) }, ctx.body);

        // Upload media attachments if present
        let mut media_ids: Vec<String> = Vec::new();
        for att in &ctx.attachments {
            let upload_url = "https://upload.twitter.com/1.1/media/upload.json";
            let auth = self.oauth1_header("POST", upload_url);
            let part = reqwest::multipart::Part::bytes(att.data.clone())
                .file_name(att.name.clone())
                .mime_str(&att.mime_type)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
            let form = reqwest::multipart::Form::new().part("media_data", part);
            let upload_resp = client.post(upload_url)
                .header("User-Agent", APP_ID)
                .header("Authorization", auth)
                .multipart(form)
                .send().await?;
            if upload_resp.status().is_success() {
                let media: Value = upload_resp.json().await.unwrap_or_default();
                if let Some(id) = media["media_id_string"].as_str() {
                    media_ids.push(id.to_string());
                }
            }
        }

        if self.targets.is_empty() {
            let url = "https://api.twitter.com/2/tweets";
            let auth = self.oauth1_header("POST", url);
            let mut payload = json!({ "text": &msg[..msg.len().min(280)] });
            if !media_ids.is_empty() {
                payload["media"] = json!({ "media_ids": media_ids });
            }
            let resp = client.post(url).header("User-Agent", APP_ID).header("Authorization", auth).json(&payload).send().await?;
            Ok(resp.status().is_success())
        } else {
            let mut all_ok = true;
            for target in &self.targets {
                let url = format!("https://api.twitter.com/2/dm_conversations/with/{}/messages", target);
                let auth = self.oauth1_header("POST", &url);
                let mut payload = json!({ "text": &msg[..msg.len().min(10000)] });
                if !media_ids.is_empty() {
                    payload["attachments"] = json!(media_ids.iter().map(|id| json!({"media_id": id})).collect::<Vec<_>>());
                }
                let resp = client.post(&url).header("User-Agent", APP_ID).header("Authorization", auth).json(&payload).send().await?;
                if !resp.status().is_success() { all_ok = false; }
            }
            Ok(all_ok)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "twitter://consumer_key/consumer_secret/atoken2/access_secret",
            "twitter://consumer_key/consumer_secret/atoken3/access_secret?cache=no",
            "twitter://consumer_key/consumer_secret/atoken4/access_secret",
            "twitter://consumer_key/consumer_secret/atoken5/access_secret",
            "twitter://consumer_key/consumer_secret2/atoken6/access_secret",
            "twitter://user@consumer_key/csecret2/atoken7/access_secret/-/%/",
            "twitter://user@consumer_key/csecret/atoken8/access_secret?cache=No&batch=No",
            "twitter://user@consumer_key/csecret/atoken9/access_secret",
            "twitter://user@consumer_key/csecret/atoken11/access_secret",
            "tweet://ckey/csecret/atoken12/access_secret",
            "twitter://usera@consumer_key/consumer_secret/atoken14/access_secret/user/?to=userb",
            "twitter://ckey/csecret/atoken16/access_secret",
            "twitter://ckey/csecret/atoken17/access_secret?mode=tweet",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "twitter://",
            "twitter://:@/",
            "twitter://consumer_key",
            "twitter://consumer_key/consumer_secret/",
            "twitter://consumer_key/consumer_secret/atoken1/",
            "twitter://user@ckey/csecret/atoken13/access_secret?mode=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_host_path_form() {
        // twitter://ck/cs/at/ats
        let parsed = ParsedUrl::parse("twitter://ckey/csecret/atoken/asecret").unwrap();
        let tw = Twitter::from_url(&parsed).unwrap();
        assert_eq!(tw.consumer_key, "ckey");
        assert_eq!(tw.consumer_secret, "csecret");
        assert_eq!(tw.access_token, "atoken");
        assert_eq!(tw.access_token_secret, "asecret");
        assert!(tw.targets.is_empty());
    }

    #[test]
    fn test_from_url_with_targets() {
        // twitter://usera@consumer_key/consumer_secret/atoken14/access_secret/user/?to=userb
        // With user@host form: user=usera, host=consumer_key
        // Path parts: [consumer_secret, atoken14, access_secret, user]
        // password is None, so host+path form: ck=consumer_key, cs=consumer_secret, at=atoken14, ats=access_secret
        // remaining path parts = [user], plus to=userb
        let parsed = ParsedUrl::parse(
            "twitter://usera@consumer_key/consumer_secret/atoken14/access_secret/user/?to=userb"
        ).unwrap();
        let tw = Twitter::from_url(&parsed).unwrap();
        assert_eq!(tw.consumer_key, "consumer_key");
        // targets from path parts after first 3
        assert!(tw.targets.contains(&"user".to_string()));
    }

    #[test]
    fn test_from_url_tweet_schema() {
        let parsed = ParsedUrl::parse("tweet://ckey/csecret/atoken/asecret").unwrap();
        let tw = Twitter::from_url(&parsed).unwrap();
        assert_eq!(tw.consumer_key, "ckey");
    }

    #[test]
    fn test_from_url_tweet_mode() {
        let parsed = ParsedUrl::parse("twitter://ckey/csecret/atoken/asecret?mode=tweet").unwrap();
        let tw = Twitter::from_url(&parsed);
        assert!(tw.is_some());
    }

    #[test]
    fn test_service_details() {
        let details = Twitter::static_details();
        assert_eq!(details.service_name, "Twitter/X");
        assert_eq!(details.service_url, Some("https://twitter.com"));
        assert!(details.protocols.contains(&"twitter"));
        assert!(details.protocols.contains(&"x"));
        assert!(details.protocols.contains(&"tweet"));
        assert!(details.attachment_support);
    }
}

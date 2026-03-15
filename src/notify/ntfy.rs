use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::types::NotifyType;
use crate::utils::parse::ParsedUrl;

pub struct Ntfy {
    host: Option<String>,
    port: Option<u16>,
    topics: Vec<String>,
    secure: bool,
    priority: &'static str,
    auth: Option<NtfyAuth>,
    verify_certificate: bool,
    tags: Vec<String>,
}

enum NtfyAuth {
    Basic { user: String, pass: String },
    Token(String),
}

impl Ntfy {
    const CLOUD_HOST: &'static str = "ntfy.sh";

    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // ntfy://topic  (cloud mode)
        // ntfy://host/topic  or  ntfys://host/topics
        // ntfy://user:pass@host/topic
        // ntfy://token@host/topic  (if user starts with "tk_")
        // https://ntfy.sh?to=topic

        let secure = url.schema == "ntfys" || url.schema == "https";

        // Validate auth mode if specified
        if let Some(auth_mode) = url.get("auth") {
            match auth_mode.to_lowercase().as_str() {
                "token" | "bearer" | "basic" | "login" | "" => {}
                _ => return None,
            }
        }

        // Validate mode if specified
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "cloud" | "private" | "" => {}
                _ => return None,
            }
        }

        // Validate hostname if present (reject hosts starting/ending with hyphen or containing invalid chars)
        if let Some(ref h) = url.host {
            if h.starts_with('-') || h.starts_with('_') || h.ends_with('-') {
                return None;
            }
        }

        // Determine host and topics
        let (host, mut topics): (Option<String>, Vec<String>) = match &url.host {
            None => (None, vec![]),
            Some(h) if url.schema == "https" || url.schema == "http" => {
                // For https://ntfy.sh URLs, host is the server
                if h == Self::CLOUD_HOST || h.ends_with(".ntfy.sh") {
                    (Some(h.clone()), url.path_parts.clone())
                } else {
                    // Not an ntfy host — reject
                    return None;
                }
            }
            Some(h) if url.path_parts.is_empty() => {
                // ntfy://topic  — host IS the topic, use cloud
                (None, vec![h.clone()])
            }
            Some(h) => {
                // ntfy://host/topic1/topic2
                (Some(h.clone()), url.path_parts.clone())
            }
        };

        // Support ?to= query param for topics
        if let Some(to) = url.get("to") {
            topics.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        if topics.is_empty() {
            return None;
        }

        // Determine authentication
        let auth_mode = url.get("auth").map(|s| s.to_lowercase());

        let auth = if let Some(token_val) = url.get("token") {
            // ?token=xxx param
            Some(NtfyAuth::Token(token_val.to_string()))
        } else {
            match (&url.user, &url.password) {
                (Some(u), _) if u.starts_with("tk_") => {
                    Some(NtfyAuth::Token(u.clone()))
                }
                (Some(u), Some(p)) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => {
                    // When auth=token, use the password as the token
                    Some(NtfyAuth::Token(p.clone()))
                }
                (None, Some(p)) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => {
                    Some(NtfyAuth::Token(p.clone()))
                }
                (Some(u), _) if auth_mode.as_deref() == Some("token") || auth_mode.as_deref() == Some("bearer") => {
                    Some(NtfyAuth::Token(u.clone()))
                }
                (Some(u), Some(p)) => {
                    Some(NtfyAuth::Basic { user: u.clone(), pass: p.clone() })
                }
                _ => None,
            }
        };

        let priority = url.get("priority").map(|p| match p.to_lowercase().as_str() {
            "min" | "1" => "min",
            "low" | "2" => "low",
            "high" | "4" => "high",
            "max" | "urgent" | "5" => "max",
            _ => "default",
        }).unwrap_or("default");

        Some(Self {
            host,
            port: url.port,
            topics,
            secure,
            priority,
            auth,
            verify_certificate: url.verify_certificate(),
            tags: url.tags(),
        })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "Ntfy",
            service_url: Some("https://ntfy.sh"),
            setup_url: Some("https://docs.ntfy.sh/publish/"),
            protocols: vec!["ntfy", "ntfys"],
            description: "Send notifications via ntfy.sh (self-hosted or cloud).",
            attachment_support: true,
        }
    }

    fn base_url(&self) -> String {
        let schema = if self.secure { "https" } else { "http" };
        match (&self.host, self.port) {
            (Some(h), Some(p)) => format!("{}://{}:{}", schema, h, p),
            (Some(h), None) => format!("{}://{}", schema, h),
            _ => format!("https://{}", Self::CLOUD_HOST),
        }
    }
}

#[async_trait]
impl Notify for Ntfy {
    fn schemas(&self) -> &[&str] { &["ntfy", "ntfys"] }
    fn service_name(&self) -> &str { "Ntfy" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let base = self.base_url();
        let mut all_ok = true;

        for topic in &self.topics {
            let url = format!("{}/{}", base, topic);
            let mut req = client
                .post(&url)
                .header("User-Agent", APP_ID)
                .header("X-Priority", self.priority);

            // Only add markdown header when format is markdown (matching Python)
            if ctx.body_format == crate::types::NotifyFormat::Markdown {
                req = req.header("X-Markdown", "yes");
            }

            if !ctx.title.is_empty() {
                req = req.header("X-Title", &ctx.title);
            }

            req = match &self.auth {
                Some(NtfyAuth::Basic { user, pass }) => {
                    req.basic_auth(user, Some(pass))
                }
                Some(NtfyAuth::Token(t)) => {
                    req.header("Authorization", format!("Bearer {}", t))
                }
                None => req,
            };

            if ctx.attachments.len() == 1 {
                // Single attachment: send as binary body with message in headers
                let attach = &ctx.attachments[0];
                let mut att_req = client.put(&url)
                    .header("User-Agent", APP_ID)
                    .header("X-Priority", self.priority)
                    .header("X-Filename", &attach.name);
                if !ctx.title.is_empty() {
                    att_req = att_req.header("X-Title", &ctx.title);
                }
                att_req = att_req.header("X-Message", &ctx.body);
                att_req = match &self.auth {
                    Some(NtfyAuth::Basic { user, pass }) => att_req.basic_auth(user, Some(pass)),
                    Some(NtfyAuth::Token(t)) => att_req.header("Authorization", format!("Bearer {}", t)),
                    None => att_req,
                };
                let resp = att_req.body(attach.data.clone()).send().await?;
                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("Ntfy send to {} failed: {}", topic, body);
                    all_ok = false;
                }
            } else {
                // No attachments or multiple: send text message first
                let resp = req.body(ctx.body.clone()).send().await?;
                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("Ntfy send to {} failed: {}", topic, body);
                    all_ok = false;
                }

                // Send each attachment as a separate PUT
                for att in &ctx.attachments {
                    let att_url = format!("{}/{}", base, topic);
                    let mut att_req = client.put(&att_url)
                        .header("User-Agent", APP_ID)
                        .header("X-Filename", &att.name);
                    att_req = match &self.auth {
                        Some(NtfyAuth::Basic { user, pass }) => att_req.basic_auth(user, Some(pass)),
                        Some(NtfyAuth::Token(t)) => att_req.header("Authorization", format!("Bearer {}", t)),
                        None => att_req,
                    };
                    let resp = att_req.body(att.data.clone()).send().await?;
                    if !resp.status().is_success() { all_ok = false; }
                }
            }
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
            "ntfy://user@localhost/topic/",
            "ntfy://ntfy.sh/topic1/topic2/",
            "ntfy://localhost/topic1/topic2/",
            "ntfy://localhost/topic1/?email=user@gmail.com",
            "ntfy://localhost/topic1/?tags=tag1,tag2,tag3",
            "ntfy://localhost/topic1/?actions=view%2CExample%2Chttp://www.example.com/%3Bview%2CTest%2Chttp://www.test.com/",
            "ntfy://localhost/topic1/?delay=3600",
            "ntfy://localhost/topic1/?title=A%20Great%20Title",
            "ntfy://localhost/topic1/?click=yes",
            "ntfy://localhost/topic1/?email=user@example.com",
            "ntfy://localhost/topic1/?image=False",
            "ntfy://localhost/topic1/?avatar_url=ttp://localhost/test.jpg",
            "ntfy://localhost/topic1/?attach=http://example.com/file.jpg",
            "ntfy://localhost/topic1/?attach=http://example.com/file.jpg&filename=smoke.jpg",
            "ntfy://localhost/topic1/?attach=http://-%20",
            "ntfy://tk_abcd123456@localhost/topic1",
            "ntfy://abcd123456@localhost/topic1?auth=token",
            "ntfy://:abcd123456@localhost/topic1?auth=token",
            "ntfy://localhost/topic1?token=abc1234",
            "ntfy://user:token@localhost/topic1?auth=token",
            "ntfy://localhost/topic1/?priority=default",
            "ntfy://localhost/topic1/?priority=high",
            "ntfy://user:pass@localhost:8080/topic/",
            "ntfys://user:pass@localhost?to=topic",
            "https://ntfy.sh?to=topic",
            "ntfy://user:pass@topic1/topic2/topic3/?mode=cloud",
            "ntfy://user:pass@ntfy.sh/topic1/topic2/?mode=cloud",
            "ntfy://user:pass@localhost:8083/topic1/topic2/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "https://just/a/random/host/that/means/nothing",
            "ntfys://user:web/token@localhost/topic/?mode=invalid",
            "ntfys://token@localhost/topic/?auth=invalid",
            "ntfys://user:web@-_/topic1/topic2/?mode=private",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

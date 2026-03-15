use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct NotificationApi { client_id: String, secret: String, verify_certificate: bool, tags: Vec<String> }
impl NotificationApi {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // napi://client_id:secret@... or napi://client_id/secret/targets...
        // or napi://?id=ci&secret=cs&to=...
        // or napi://type@cid/secret/...
        // Validate type if provided (must be alphanumeric)
        if let Some(t) = url.get("type") {
            if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { return None; }
        }
        // Validate channels if provided
        if let Some(ch) = url.get("channels") {
            let valid_channels = ["email", "sms", "inapp", "in_app", "web_push", "mobile_push", "slack"];
            for c in ch.split(',') {
                let c = c.trim().to_lowercase();
                if !c.is_empty() && !valid_channels.contains(&c.as_str()) { return None; }
            }
        }
        let (client_id, secret) = if let Some(id) = url.get("id") {
            let sec = url.get("secret")
                .map(|s| s.to_string())
                .or_else(|| url.host.clone())
                .or_else(|| url.path_parts.first().cloned())?;
            (id.to_string(), sec)
        } else if url.password.is_some() {
            (url.user.clone()?, url.password.clone()?)
        } else if url.user.is_some() {
            // napi://type@cid/secret/... — user is message_type, host is client_id
            let cid = url.host.clone().filter(|h| !h.is_empty())?;
            let sec = url.path_parts.first()?.clone();
            (cid, sec)
        } else if let Some(sec) = url.get("secret") {
            // napi://client_id?secret=cs&...
            let cid = url.host.clone().filter(|h| !h.is_empty())?;
            (cid, sec.to_string())
        } else {
            let cid = url.host.clone().filter(|h| !h.is_empty())?;
            let sec = url.path_parts.first()?.clone();
            (cid, sec)
        };
        if client_id.is_empty() || secret.is_empty() { return None; }
        // Validate mode if provided
        if let Some(mode) = url.get("mode") {
            match mode.to_lowercase().as_str() {
                "template" | "message" | "" => {}
                _ => return None,
            }
        }
        // Validate region if provided
        if let Some(region) = url.get("region") {
            match region.to_lowercase().as_str() {
                "us" | "eu" | "ca" | "" => {}
                _ => return None,
            }
        }
        // Collect targets and validate that at least one id is present
        // When host is client_id and path[0] is secret, skip 1 (the secret in path)
        // When using query params (?id=, ?secret=), skip 0
        let skip = if url.get("id").is_some() { 0 } else { 1 };
        let targets: Vec<&str> = url.path_parts.iter().skip(skip).map(|s| s.as_str()).collect();
        // Also add ?to= targets
        let to_targets: Vec<String> = url.get("to")
            .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let all_targets: Vec<&str> = targets.iter().copied()
            .chain(to_targets.iter().map(|s| s.as_str()))
            .collect();
        fn is_contact(s: &str) -> bool {
            s.contains('@') || s.starts_with('+')
        }
        if !all_targets.is_empty() {
            // At least one target must be an id (non-contact)
            let has_id = all_targets.iter().any(|t| !is_contact(t));
            if !has_id { return None; }
            // After each id, at most one email and one phone are allowed.
            // A second email or second phone without a new id is invalid.
            let mut has_email = false;
            let mut has_phone = false;
            for t in &all_targets {
                // Named emails like "Name<email>" contain @ but also <>
                let is_named_email = t.contains('<') && t.contains('>');
                let is_email = t.contains('@') && !is_named_email;
                let is_phone = t.starts_with('+');
                if !is_email && !is_phone && !is_named_email {
                    // New id — reset
                    has_email = false;
                    has_phone = false;
                } else if is_email {
                    if has_email { return None; } // duplicate email for same id
                    has_email = true;
                } else if is_phone {
                    if has_phone { return None; } // duplicate phone for same id
                    has_phone = true;
                }
                // Named emails (with <>) don't count toward the duplicate check
            }
        }
        Some(Self { client_id, secret, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "NotificationAPI", service_url: Some("https://www.notificationapi.com"), setup_url: None, protocols: vec!["napi", "notificationapi"], description: "Send via NotificationAPI.", attachment_support: false } }
}
#[async_trait]
impl Notify for NotificationApi {
    fn schemas(&self) -> &[&str] { &["napi", "notificationapi"] }
    fn service_name(&self) -> &str { "NotificationAPI" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let payload = json!({ "notificationId": "apprise", "user": { "id": "default" }, "mergeTags": { "title": ctx.title, "body": ctx.body } });
        let client = build_client(self.verify_certificate)?;
        let resp = client.post("https://api.notificationapi.com/send").header("User-Agent", APP_ID).basic_auth(&self.client_id, Some(&self.secret)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "napi://client_id/client_secret/id/g@rb@ge/+15551235553/",
            "napi://cid/secret/id/user1@example.com/?type=apprise-msg",
            "notificationapi://cid/secret/id/user1@example.com",
            "napi://cid/secret/id/id2/user1@example.com",
            "napi://type@cid/secret/id10/user2@example.com/id5/+15551235555/id8/+15551235534?reply=Chris<chris@example.com>",
            "napi://type@cid/secret/abc1/user1@example.com/id5/+15551235555/?from=Chris&reply=Christopher",
            "napi://type@cid/secret/id/user3@example.com/?from=joe@example.ca&reply=user@abc.com",
            "napi://type@cid/secret/id/user4@example.com/?from=joe@example.ca&bcc=user1@yahoo.ca&cc=user2@yahoo.ca",
            "napi://?id=ci&secret=cs&to=id,user5@example.com&type=typec",
            "napi://id?secret=cs&to=id,user5@example.com&type=typeb",
            "napi://secret?id=ci&to=id,user5@example.com&type=typea",
            "napi://?id=ci&secret=cs&type=test-type&region=eu",
            "napi://user@client_id/cs2/id/user6@example.ca?bcc=invalid",
            "napi://user@client_id/cs3/id/user8@example.ca?cc=l2g@nuxref.com",
            "napi://client_id/cs3/id/user8@example.ca?channels=email,sms,slack,mobile_push,web_push,inapp",
            "napi://user@client_id/cs4/id/user9@example.ca?cc=Chris<l2g@nuxref.com>",
            "napi://user@client_id/cs5/id/user10@example.ca?cc=invalid",
            "napi://user@client_id/cs6/id/user11@example.ca?to=invalid",
            "napi://user@client_id/cs7/id/chris1@example.com",
            "napi://user@client_id/cs8/id1/user12@example.ca?to=id,Chris<chris2@example.com>",
            "napi://user@client_id/cs9/id2/user13@example.ca/id/kris@example.com/id/chris2@example.com/id/+15552341234?:token=value",
            "napi://user@client_id/cs10/id/user14@example.ca?cc=Chris<chris10@example.com>",
            "napi://user@client_id/cs11/id/user15@example.ca?cc=chris12@example.com",
            "napi://user@client_id/cs12/id/user16@example.ca?bcc=Chris<chris14@example.com>",
            "napi://user@client_id/cs13/id/user@example.ca?bcc=chris13@example.com",
            "napi://user@client_id/cs14/id/user@example.ca?to=Chris<chris9@example.com>,id14",
            "napi://user@client_id/cs15/id?to=user@example.com",
            "napi://user@client_id/cs16/id/user@example.ca?template=1234&+sub=value&+sub2=value2",
            "napi://user@client_id/cs19/id/user@example.ca",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "napi://",
            "napi://:@/",
            "napi://abcd",
            "napi://abcd@host.com",
            "napi://user@client_id/cs14a/user@example.ca",
            "napi://user@client_id/cs14b/+15551235553",
            "napi://user@client_id/cs14c/+15551235553/user@example.ca",
            "napi://type@client_id/client_secret/id/+15551235553/?mode=invalid",
            "napi://type@client_id/client_secret/id/+15551235553/?region=invalid",
            "napi://type@client_id/client_secret/id/user@example.ca/user2@example.ca",
            "napi://type@client_id/client_secret/user@example.ca/user2@example.ca",
            "napi://type@client_id/client_secret/id/+15551235553/+15551235555",
            "napi://type@client_id/client_secret/+15551235553/+15551235555",
            "napi://client_id/client_secret/id/+15551231234/?type=*(",
            "napi://client_id/client_secret/id/+15551231234/?channels=bad",
            "napi://?secret=cs&to=id,user404@example.com&type=typed",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

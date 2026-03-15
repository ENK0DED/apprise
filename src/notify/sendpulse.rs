use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;
pub struct SendPulse { client_id: String, client_secret: String, from_email: String, to: Vec<String>, verify_certificate: bool, tags: Vec<String> }
impl SendPulse {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // sendpulse://client_id:client_secret@domain/to@email
        // or sendpulse://from_user@from_domain/client_id/client_secret/?to=...&template=...
        let (client_id, client_secret, from_email, to) = if url.password.is_some() {
            let cid = url.user.clone()?;
            let cs = url.password.clone()?;
            let from = url.host.clone().map(|h| format!("noreply@{}", h)).unwrap_or_else(|| "noreply@example.com".to_string());
            let mut to: Vec<String> = url.path_parts.clone();
            if let Some(t) = url.get("to") { to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())); }
            (cid, cs, from, to)
        } else if url.user.is_some() && url.path_parts.len() >= 2 {
            // sendpulse://from_user@from_domain/client_id/client_secret
            let from = format!("{}@{}", url.user.as_ref()?, url.host.as_deref().unwrap_or("example.com"));
            let cid = url.path_parts.get(0)?.clone();
            let cs = url.path_parts.get(1)?.clone();
            let mut to: Vec<String> = url.path_parts.get(2..).unwrap_or(&[]).to_vec();
            if let Some(t) = url.get("to") { to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())); }
            (cid, cs, from, to)
        } else if url.host.is_some() && !url.path_parts.is_empty() && url.user.is_none() {
            // sendpulse://client_id/client_secret/?user=chris@example.ca
            // or sendpulse://domain/client_id/client_secret/?user=chris
            let host = url.host.clone()?;
            let (cid, cs, extra_start) = if url.path_parts.len() >= 2 {
                // host is domain, path[0]=client_id, path[1]=client_secret
                (url.path_parts[0].clone(), url.path_parts[1].clone(), 2usize)
            } else {
                // host is client_id, path[0]=client_secret
                (host.clone(), url.path_parts[0].clone(), 1usize)
            };
            let user = url.get("user").map(|s| s.to_string()).unwrap_or_else(|| "noreply@example.com".to_string());
            let from = if user.contains('@') || user.contains('<') { user } else {
                let domain = if url.path_parts.len() >= 2 { host } else { "example.com".to_string() };
                format!("{}@{}", user, domain)
            };
            let mut to: Vec<String> = url.path_parts.get(extra_start..).unwrap_or(&[]).to_vec();
            if let Some(t) = url.get("to") { to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())); }
            (cid, cs, from, to)
        } else if let Some(id) = url.get("id") {
            // sendpulse://?id=ci&secret=cs&user=chris@example.com
            let cid = id.to_string();
            let cs = url.get("secret")?.to_string();
            let user = url.get("user").map(|s| s.to_string()).unwrap_or_else(|| "noreply@example.com".to_string());
            // When using query-only format, user must be a valid email
            if !user.contains('@') && !user.contains('<') { return None; }
            let mut to = Vec::new();
            if let Some(t) = url.get("to") { to.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())); }
            (cid, cs, user, to)
        } else {
            return None;
        };
        // Validate template if provided (must be numeric)
        if let Some(tmpl) = url.get("template") {
            if tmpl.parse::<u64>().is_err() { return None; }
        }
        Some(Self { client_id, client_secret, from_email, to, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "SendPulse", service_url: Some("https://sendpulse.com"), setup_url: None, protocols: vec!["sendpulse"], description: "Send email via SendPulse.", attachment_support: true } }
}
#[async_trait]
impl Notify for SendPulse {
    fn schemas(&self) -> &[&str] { &["sendpulse"] }
    fn service_name(&self) -> &str { "SendPulse" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let token_params = [("grant_type", "client_credentials"), ("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str())];
        let token_resp: Value = client.post("https://api.sendpulse.com/oauth/access_token").header("User-Agent", APP_ID).form(&token_params).send().await?.json().await.map_err(|e| NotifyError::Auth(e.to_string()))?;
        let access_token = token_resp["access_token"].as_str().ok_or_else(|| NotifyError::Auth("No token".into()))?;
        let to_list: Vec<_> = self.to.iter().map(|e| json!({ "email": e, "name": e })).collect();
        let mut payload = json!({ "html": ctx.body, "text": ctx.body, "subject": ctx.title, "from": { "name": "Apprise", "email": self.from_email }, "to": to_list });
        if !ctx.attachments.is_empty() {
            let mut attachments_map = serde_json::Map::new();
            for att in &ctx.attachments {
                attachments_map.insert(att.name.clone(), json!(base64::engine::general_purpose::STANDARD.encode(&att.data)));
            }
            payload["attachments"] = Value::Object(attachments_map);
        }
        let resp = client.post("https://api.sendpulse.com/smtp/emails").header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", access_token)).json(&payload).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "sendpulse://user@example.com/client_id/cs1/?template=123",
            "sendpulse://user@example.com/client_id/cs1/",
            "sendpulse://user@example.com/client_id/cs1/?format=text",
            "sendpulse://user@example.com/client_id/cs1/?format=html",
            "sendpulse://chris@example.com/client_id/cs1/?from=Chris",
            "sendpulse://?id=ci&secret=cs&user=chris@example.com",
            "sendpulse://?id=ci&secret=cs&user=Chris<chris@example.com>",
            "sendpulse://example.com/client_id/cs1/?user=chris",
            "sendpulse://client_id/cs1/?user=chris@example.ca",
            "sendpulse://client_id/cs1/?from=Chris<chris@example.com>",
            "sendpulse://?from=Chris<chris@example.com>&id=ci&secret=cs",
            "sendpulse://user@example.com/client_id/cs2/?bcc=l2g@nuxref.com",
            "sendpulse://user@example.com/client_id/cs2/?bcc=invalid",
            "sendpulse://user@example.com/client_id/cs3/?cc=l2g@nuxref.com",
            "sendpulse://user@example.com/client_id/cs4/?cc=Chris<l2g@nuxref.com>",
            "sendpulse://user@example.com/client_id/cs5/?cc=invalid",
            "sendpulse://user@example.com/client_id/cs6/?to=invalid",
            "sendpulse://user@example.com/client_id/cs7/chris@example.com",
            "sendpulse://user@example.com/client_id/cs8/?to=Chris<chris@example.com>",
            "sendpulse://user@example.com/client_id/cs9/chris@example.com/chris2@example.com/Test<test@test.com>",
            "sendpulse://user@example.com/client_id/cs10/?cc=Chris<chris@example.com>",
            "sendpulse://user@example.com/client_id/cs11/?cc=chris@example.com",
            "sendpulse://user@example.com/client_id/cs12/?bcc=Chris<chris@example.com>",
            "sendpulse://user@example.com/client_id/cs13/?bcc=chris@example.com",
            "sendpulse://user@example.com/client_id/cs14/?to=Chris<chris@example.com>",
            "sendpulse://user@example.com/client_id/cs15/?to=chris@example.com",
            "sendpulse://user@example.com/client_id/cs16/?template=1234&+sub=value&+sub2=value2",
            "sendpulse://user@example.com/client_id/cs19/",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "sendpulse://",
            "sendpulse://:@/",
            "sendpulse://abcd",
            "sendpulse://abcd@host.com",
            "sendpulse://user@example.com/client_id/cs/?template=invalid",
            "sendpulse://?id=ci&secret=cs&user=chris",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

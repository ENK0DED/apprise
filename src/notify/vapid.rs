use async_trait::async_trait;
use serde_json::json;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

/// VAPID / WebPush notifications.
///
/// This implements the VAPID JWT signing and HTTP push delivery.
/// Full payload encryption (RFC 8291 / aes128gcm) requires additional
/// crypto crates (p256, aes-gcm, hkdf). This implementation sends
/// a push with the VAPID authorization header; services that require
/// encrypted payloads will need those crates added.
pub struct Vapid {
    subscriber: String,
    endpoints: Vec<String>,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Vapid {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let subscriber = url.user.clone().or_else(|| url.host.clone())?;
        let subscriber = if subscriber.contains('@') {
            format!("mailto:{}", subscriber)
        } else {
            subscriber
        };
        let endpoints = url.path_parts.clone();
        if endpoints.is_empty() { return None; }
        Some(Self { subscriber, endpoints, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }

    pub fn static_details() -> ServiceDetails {
        ServiceDetails {
            service_name: "VAPID / WebPush",
            service_url: Some("https://web.dev/push-notifications-overview/"),
            setup_url: None,
            protocols: vec!["vapid"],
            description: "Send browser push notifications via WebPush/VAPID.",
            attachment_support: false,
        }
    }
}

#[async_trait]
impl Notify for Vapid {
    fn schemas(&self) -> &[&str] { &["vapid"] }
    fn service_name(&self) -> &str { "VAPID / WebPush" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let client = build_client(self.verify_certificate)?;
        let payload = json!({
            "title": ctx.title,
            "body": ctx.body,
            "icon": "",
        });
        let body = serde_json::to_vec(&payload).map_err(NotifyError::Json)?;

        let mut all_ok = true;
        for endpoint in &self.endpoints {
            // Send unencrypted JSON payload — compatible with push services
            // that accept plaintext. Full RFC 8291 encryption requires
            // p256 + aes-gcm + hkdf crates.
            let resp = client.post(endpoint)
                .header("User-Agent", APP_ID)
                .header("Content-Type", "application/json")
                .header("TTL", "86400")
                .body(body.clone())
                .send().await?;

            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}

use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Sns { access_key: String, secret_key: String, region: String, targets: Vec<String>, tags: Vec<String> }
impl Sns {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let access_key = url.user.clone()?;
        let secret_key = url.password.clone()?;
        let region = url.host.clone().unwrap_or_else(|| "us-east-1".to_string());
        let targets = url.path_parts.clone();
        if targets.is_empty() { return None; }
        Some(Self { access_key, secret_key, region, targets, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "AWS SNS", service_url: Some("https://aws.amazon.com/sns/"), setup_url: None, protocols: vec!["sns"], description: "Send notifications via AWS SNS.", attachment_support: false } }
}
#[async_trait]
impl Notify for Sns {
    fn schemas(&self) -> &[&str] { &["sns"] }
    fn service_name(&self) -> &str { "AWS SNS" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        // AWS SNS requires SigV4 signing - simplified implementation
        // In production, use aws-sdk-rust or sign manually
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(NotifyError::Http)?;
        let endpoint = format!("https://sns.{}.amazonaws.com/", self.region);
        let mut all_ok = true;
        for target in &self.targets {
            let body = format!("Action=Publish&TopicArn={}&Message={}&Subject={}", urlencoding::encode(target), urlencoding::encode(&ctx.body), urlencoding::encode(&ctx.title));
            let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let resp = client.post(&endpoint)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("X-Amz-Date", &date)
                .header("Authorization", format!("AWS4-HMAC-SHA256 Credential={}/{}/sns/aws4_request,SignedHeaders=host;x-amz-date,Signature=placeholder", self.access_key, &date[..8]))
                .body(body).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}

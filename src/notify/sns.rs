use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::aws::sigv4;
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
        let endpoint = format!("https://sns.{}.amazonaws.com/", self.region);
        let content_type = "application/x-www-form-urlencoded";
        let mut all_ok = true;
        for target in &self.targets {
            let body = format!(
                "Action=Publish&TopicArn={}&Message={}&Subject={}",
                urlencoding::encode(target),
                urlencoding::encode(&ctx.body),
                urlencoding::encode(&ctx.title),
            );
            let (auth, datetime) = sigv4("POST", &endpoint, body.as_bytes(), &self.access_key, &self.secret_key, &self.region, "sns", content_type);
            let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(NotifyError::Http)?;
            let resp = client.post(&endpoint).header("User-Agent", APP_ID).header("Content-Type", content_type).header("X-Amz-Date", &datetime).header("Authorization", &auth).body(body).send().await?;
            if !resp.status().is_success() { all_ok = false; }
        }
        Ok(all_ok)
    }
}

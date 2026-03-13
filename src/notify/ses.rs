use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::aws::sigv4;
use crate::utils::parse::ParsedUrl;

pub struct Ses { access_key: String, secret_key: String, region: String, from: String, targets: Vec<String>, tags: Vec<String> }
impl Ses {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let access_key = url.user.clone()?;
        let secret_key = url.password.clone()?;
        let region = url.host.clone().unwrap_or_else(|| "us-east-1".to_string());
        let from = url.get("from").unwrap_or("apprise@example.com").to_string();
        let targets: Vec<String> = url.path_parts.iter()
            .map(|s| if s.contains('@') { s.clone() } else { format!("{}@example.com", s) })
            .collect();
        if targets.is_empty() { return None; }
        Some(Self { access_key, secret_key, region, from, targets, tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails { ServiceDetails { service_name: "AWS SES", service_url: Some("https://aws.amazon.com/ses/"), setup_url: None, protocols: vec!["ses"], description: "Send email via AWS SES.", attachment_support: false } }
}
#[async_trait]
impl Notify for Ses {
    fn schemas(&self) -> &[&str] { &["ses"] }
    fn service_name(&self) -> &str { "AWS SES" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }
    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let endpoint = format!("https://email.{}.amazonaws.com/", self.region);
        let content_type = "application/x-www-form-urlencoded";
        let mut body = format!(
            "Action=SendEmail&Source={}&Message.Subject.Data={}&Message.Body.Text.Data={}",
            urlencoding::encode(&self.from),
            urlencoding::encode(&ctx.title),
            urlencoding::encode(&ctx.body),
        );
        for (i, target) in self.targets.iter().enumerate() {
            body.push_str(&format!("&Destination.ToAddresses.member.{}={}", i + 1, urlencoding::encode(target)));
        }
        let (auth, datetime) = sigv4("POST", &endpoint, body.as_bytes(), &self.access_key, &self.secret_key, &self.region, "ses", content_type);
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(NotifyError::Http)?;
        let resp = client.post(&endpoint).header("User-Agent", APP_ID).header("Content-Type", content_type).header("X-Amz-Date", &datetime).header("Authorization", &auth).body(body).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}

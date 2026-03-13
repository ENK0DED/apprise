use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::utils::parse::ParsedUrl;

pub struct Ses { access_key: String, secret_key: String, region: String, from: String, targets: Vec<String>, tags: Vec<String> }
impl Ses {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let access_key = url.user.clone()?;
        let secret_key = url.password.clone()?;
        let region = url.host.clone().unwrap_or_else(|| "us-east-1".to_string());
        let from = url.get("from").unwrap_or("apprise@example.com").to_string();
        let targets: Vec<String> = url.path_parts.iter().map(|s| if s.contains('@') { s.clone() } else { format!("{}@example.com", s) }).collect();
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
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(NotifyError::Http)?;
        let endpoint = format!("https://email.{}.amazonaws.com/", self.region);
        let to = self.targets.join(",");
        let body = format!("Action=SendEmail&Source={}&Destination.ToAddresses.member.1={}&Message.Subject.Data={}&Message.Body.Text.Data={}",
            urlencoding::encode(&self.from), urlencoding::encode(&to), urlencoding::encode(&ctx.title), urlencoding::encode(&ctx.body));
        let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let resp = client.post(&endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("X-Amz-Date", &date)
            .header("Authorization", format!("AWS4-HMAC-SHA256 Credential={}/{}/ses/aws4_request,SignedHeaders=host;x-amz-date,Signature=placeholder", self.access_key, &date[..8]))
            .body(body).send().await?;
        Ok(resp.status().is_success())
    }
}

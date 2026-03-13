use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mailgun {
    apikey: String,
    domain: String,
    from: String,
    to: Vec<String>,
    region: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mailgun {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // mailgun://user@domain/apikey/to1/to2
        let domain = url.host.clone()?;
        let apikey = url.path_parts.first()?.clone();
        let to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        let from = url.user.clone().map(|u| format!("{}@{}", u, domain)).unwrap_or_else(|| format!("noreply@{}", domain));
        let region = url.get("region").unwrap_or("us").to_string();
        Some(Self { apikey, domain, from, to, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mailgun", service_url: Some("https://mailgun.com"), setup_url: None, protocols: vec!["mailgun"], description: "Send email via Mailgun.", attachment_support: false }
    }
}

#[async_trait]
impl Notify for Mailgun {
    fn schemas(&self) -> &[&str] { &["mailgun"] }
    fn service_name(&self) -> &str { "Mailgun" }
    fn details(&self) -> ServiceDetails { Self::static_details() }
    fn tags(&self) -> Vec<String> { self.tags.clone() }

    async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
        let base = if self.region == "eu" { "https://api.eu.mailgun.net" } else { "https://api.mailgun.net" };
        let url = format!("{}/v3/{}/messages", base, self.domain);
        let to_str = self.to.join(",");
        let params = [("from", self.from.as_str()), ("to", to_str.as_str()), ("subject", ctx.title.as_str()), ("text", ctx.body.as_str())];
        let client = build_client(self.verify_certificate)?;
        let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth("api", Some(&self.apikey)).form(&params).send().await?;
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}

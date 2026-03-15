use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{build_client, Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::parse::ParsedUrl;

pub struct Mailgun {
    apikey: String,
    domain: String,
    from: String,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    region: String,
    verify_certificate: bool,
    tags: Vec<String>,
}

impl Mailgun {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        let domain = url.host.clone()?;
        let user = url.user.clone()?;
        if user.is_empty() { return None; }
        // Reject quotes in user
        if user.contains('"') { return None; }
        let apikey = url.path_parts.first()?.clone();
        let to: Vec<String> = url.path_parts.get(1..).unwrap_or(&[]).to_vec();
        let from = format!("{}@{}", user, domain);
        let region = url.get("region").unwrap_or("us").to_string();
        // Validate region
        match region.to_lowercase().as_str() {
            "us" | "eu" | "" => {}
            _ => return None,
        }
        let cc: Vec<String> = url.get("cc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let bcc: Vec<String> = url.get("bcc").map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        Some(Self { apikey, domain, from, to, cc, bcc, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
    }
    pub fn static_details() -> ServiceDetails {
        ServiceDetails { service_name: "Mailgun", service_url: Some("https://mailgun.com"), setup_url: None, protocols: vec!["mailgun"], description: "Send email via Mailgun.", attachment_support: true }
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
        let cc_str = self.cc.join(",");
        let bcc_str = self.bcc.join(",");
        let client = build_client(self.verify_certificate)?;

        let resp = if !ctx.attachments.is_empty() {
            let mut form = reqwest::multipart::Form::new()
                .text("from", self.from.clone())
                .text("to", to_str.clone())
                .text("subject", ctx.title.clone())
                .text("text", ctx.body.clone());
            if !self.cc.is_empty() { form = form.text("cc", cc_str.clone()); }
            if !self.bcc.is_empty() { form = form.text("bcc", bcc_str.clone()); }
            for att in &ctx.attachments {
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.name.clone())
                    .mime_str(&att.mime_type)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
                form = form.part("attachment", part);
            }
            client.post(&url).header("User-Agent", APP_ID).basic_auth("api", Some(&self.apikey)).multipart(form).send().await?
        } else {
            let mut params: Vec<(&str, &str)> = vec![("from", self.from.as_str()), ("to", to_str.as_str()), ("subject", ctx.title.as_str()), ("text", ctx.body.as_str())];
            if !self.cc.is_empty() { params.push(("cc", cc_str.as_str())); }
            if !self.bcc.is_empty() { params.push(("bcc", bcc_str.as_str())); }
            client.post(&url).header("User-Agent", APP_ID).basic_auth("api", Some(&self.apikey)).form(&params).send().await?
        };
        if resp.status().is_success() { Ok(true) } else { Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() }) }
    }
}


#[cfg(test)]
mod tests {
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=markdown",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=html",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?format=text",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?region=uS",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?region=EU",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?from=jack@gmail.com&name=Jason<jason@gmail.com>",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?+X-Customer-Campaign-ID=Apprise",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?:name=Chris&:status=admin",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?:from=Chris&:status=admin",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?bcc=user@example.com&cc=user2@example.com",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/test@example.com",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?to=test@example.com",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/test@example.com?name=\"Frodo\"",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/invalid",
            "mailgun://user@example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc/user1@example.com/invalid/User2:user2@example.com?bcc=user3@example.com,i@v,User1:user1@example.com&cc=user4@example.com,g@r@b,Da:user5@example.com",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "mailgun://",
            "mailgun://:@/",
            "mailgun://user@localhost.localdomain",
            "mailgun://localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
            "mailgun://\"@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc",
            "mailgun://user@localhost.localdomain/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbb-cccccccc?region=invalid",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }
}

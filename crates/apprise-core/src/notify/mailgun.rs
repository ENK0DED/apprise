use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;

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
    if user.is_empty() {
      return None;
    }
    // Reject quotes in user
    if user.contains('"') {
      return None;
    }
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
    ServiceDetails {
      service_name: "Mailgun",
      service_url: Some("https://mailgun.com"),
      setup_url: None,
      protocols: vec!["mailgun"],
      description: "Send email via Mailgun.",
      attachment_support: true,
    }
  }
}

#[async_trait]
impl Notify for Mailgun {
  fn schemas(&self) -> &[&str] {
    &["mailgun"]
  }
  fn service_name(&self) -> &str {
    "Mailgun"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

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
      if !self.cc.is_empty() {
        form = form.text("cc", cc_str.clone());
      }
      if !self.bcc.is_empty() {
        form = form.text("bcc", bcc_str.clone());
      }
      for att in &ctx.attachments {
        let part = reqwest::multipart::Part::bytes(att.data.clone())
          .file_name(att.name.clone())
          .mime_str(&att.mime_type)
          .unwrap_or_else(|_| reqwest::multipart::Part::bytes(att.data.clone()));
        form = form.part("attachment", part);
      }
      client.post(&url).header("User-Agent", APP_ID).basic_auth("api", Some(&self.apikey)).multipart(form).send().await?
    } else {
      let mut params: Vec<(&str, &str)> =
        vec![("from", self.from.as_str()), ("to", to_str.as_str()), ("subject", ctx.title.as_str()), ("text", ctx.body.as_str())];
      if !self.cc.is_empty() {
        params.push(("cc", cc_str.as_str()));
      }
      if !self.bcc.is_empty() {
        params.push(("bcc", bcc_str.as_str()));
      }
      client.post(&url).header("User-Agent", APP_ID).basic_auth("api", Some(&self.apikey)).form(&params).send().await?
    };
    if resp.status().is_success() {
      Ok(true)
    } else {
      Err(NotifyError::ServiceError { status: resp.status().as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;

  #[test]
  fn test_invalid_urls() {
    let no_user = format!("mailgun://localhost.localdomain/{}-{}-{}", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let bad_from = format!("mailgun://\"@localhost.localdomain/{}-{}-{}", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let bad_region = format!("mailgun://user@localhost.localdomain/{}-{}-{}?region=invalid", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let urls: Vec<&str> = vec![
      "mailgun://",
      "mailgun://:@/",
      "mailgun://user@localhost.localdomain",
      // Token valid but no user
      &no_user,
      // Invalid from email (quote in user)
      &bad_from,
      // Invalid region
      &bad_region,
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let token = format!("{}-{}-{}", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let urls = vec![
      format!("mailgun://user@localhost.localdomain/{}", token),
      format!("mailgun://user@localhost.localdomain/{}?format=markdown", token),
      format!("mailgun://user@localhost.localdomain/{}?format=html", token),
      format!("mailgun://user@localhost.localdomain/{}?format=text", token),
      format!("mailgun://user@localhost.localdomain/{}?region=uS", token),
      format!("mailgun://user@localhost.localdomain/{}?region=EU", token),
      format!("mailgun://user@localhost.localdomain/{}/test@example.com", token),
      format!("mailgun://user@localhost.localdomain/{}?to=test@example.com", token),
      format!("mailgun://user@localhost.localdomain/{}/test@example.com?name=\"Frodo\"", token),
      format!("mailgun://user@example.com/{}/user1@example.com?bcc=user3@example.com&cc=user4@example.com", token),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let url_str = format!(
      "mailgun://user@localhost.localdomain/{}-{}-{}/test@example.com?region=eu&cc=cc@example.com&bcc=bcc@example.com",
      "a".repeat(32),
      "b".repeat(8),
      "c".repeat(8)
    );
    let parsed = ParsedUrl::parse(&url_str).expect("parse");
    let mg = Mailgun::from_url(&parsed).expect("from_url");
    assert_eq!(mg.domain, "localhost.localdomain");
    assert_eq!(mg.from, "user@localhost.localdomain");
    assert_eq!(mg.region, "eu");
    assert_eq!(mg.to, vec!["test@example.com"]);
    assert_eq!(mg.cc, vec!["cc@example.com"]);
    assert_eq!(mg.bcc, vec!["bcc@example.com"]);
  }

  #[test]
  fn test_us_region_api_endpoint() {
    let url_str = format!("mailgun://user@example.com/{}-{}-{}?region=us", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let parsed = ParsedUrl::parse(&url_str).expect("parse");
    let mg = Mailgun::from_url(&parsed).expect("from_url");
    assert_eq!(mg.region.to_lowercase(), "us");
  }

  #[test]
  fn test_eu_region_api_endpoint() {
    let url_str = format!("mailgun://user@example.com/{}-{}-{}?region=EU", "a".repeat(32), "b".repeat(8), "c".repeat(8));
    let parsed = ParsedUrl::parse(&url_str).expect("parse");
    let mg = Mailgun::from_url(&parsed).expect("from_url");
    assert_eq!(mg.region.to_lowercase(), "eu");
  }

  #[test]
  fn test_static_details() {
    let details = Mailgun::static_details();
    assert_eq!(details.service_name, "Mailgun");
    assert!(details.protocols.contains(&"mailgun"));
    assert!(details.attachment_support);
  }
}

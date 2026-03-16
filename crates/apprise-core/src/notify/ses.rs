use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails};
use crate::utils::aws::{SigV4Params, sigv4};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;

pub struct Ses {
  access_key: String,
  secret_key: String,
  region: String,
  from: String,
  targets: Vec<String>,
  tags: Vec<String>,
}
impl Ses {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ses://user@domain/access_key/secret_key/region
    // or ses://access_key:secret_key@region/to@email
    let (access_key, secret_key, region, from) = if url.get("access").is_some() || url.get("access_key_id").is_some() {
      // All-query format: ses://?from=X&region=Y&access=Z&secret=W
      let ak = url.get("access").or_else(|| url.get("access_key_id")).map(|s| s.to_string())?;
      let sk = url.get("secret").or_else(|| url.get("secret_access_key")).map(|s| s.to_string())?;
      let region = url.get("region").map(|s| s.to_string()).unwrap_or_else(|| "us-east-1".to_string());
      let from = url.get("from").unwrap_or("apprise@example.com").to_string();
      (ak, sk, region, from)
    } else if url.password.is_some() {
      let ak = url.user.clone()?;
      let sk = url.password.clone()?;
      let region = url.host.clone().unwrap_or_else(|| "us-east-1".to_string());
      let from = url.get("from").unwrap_or("apprise@example.com").to_string();
      (ak, sk, region, from)
    } else if let Some(ref user) = url.user {
      // ses://user@domain/access_key/secret_key/region
      let from = format!("{}@{}", user, url.host.as_deref().unwrap_or("example.com"));
      let access_key = url.path_parts.first()?.clone();
      let secret_key = url.path_parts.get(1)?.clone();
      // Region must be a valid AWS region (e.g., us-east-1, eu-west-2)
      // It cannot contain '@' (that would be an email address target)
      let region =
        url.path_parts.get(2).cloned().filter(|r| !r.contains('@')).or_else(|| url.get("region").map(|s| s.to_string())).filter(|r| !r.is_empty())?;
      (access_key, secret_key, region, from)
    } else {
      return None;
    };
    let mut targets: Vec<String> = url.path_parts.iter().filter(|s| s.contains('@')).cloned().collect();
    if let Some(to) = url.get("to") {
      targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    // Validate reply address if provided
    if let Some(reply) = url.get("reply") {
      if !reply.contains('@') {
        return None;
      }
    }
    Some(Self { access_key, secret_key, region, from, targets, tags: url.tags() })
  }
  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "AWS SES",
      service_url: Some("https://aws.amazon.com/ses/"),
      setup_url: None,
      protocols: vec!["ses"],
      description: "Send email via AWS SES.",
      attachment_support: true,
    }
  }
}
#[async_trait]
impl Notify for Ses {
  fn schemas(&self) -> &[&str] {
    &["ses"]
  }
  fn service_name(&self) -> &str {
    "AWS SES"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let endpoint = format!("https://email.{}.amazonaws.com/", self.region);
    let content_type = "application/x-www-form-urlencoded";

    let body = if !ctx.attachments.is_empty() {
      // Build a raw MIME message with attachments
      let boundary = format!("----=_Part_{}", chrono::Utc::now().timestamp_millis());
      let mut mime_msg = String::new();
      // Headers
      mime_msg.push_str(&format!("From: {}\r\n", self.from));
      mime_msg.push_str(&format!("To: {}\r\n", self.targets.join(", ")));
      mime_msg.push_str(&format!("Subject: {}\r\n", ctx.title));
      mime_msg.push_str("MIME-Version: 1.0\r\n");
      mime_msg.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{}\"\r\n", boundary));
      mime_msg.push_str("\r\n");
      // Text body part
      mime_msg.push_str(&format!("--{}\r\n", boundary));
      mime_msg.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
      mime_msg.push_str("Content-Transfer-Encoding: 7bit\r\n");
      mime_msg.push_str("\r\n");
      mime_msg.push_str(&ctx.body);
      mime_msg.push_str("\r\n");
      // Attachment parts
      for att in &ctx.attachments {
        mime_msg.push_str(&format!("--{}\r\n", boundary));
        mime_msg.push_str(&format!("Content-Type: {}; name=\"{}\"\r\n", att.mime_type, att.name));
        mime_msg.push_str("Content-Transfer-Encoding: base64\r\n");
        mime_msg.push_str(&format!("Content-Disposition: attachment; filename=\"{}\"\r\n", att.name));
        mime_msg.push_str("\r\n");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
        // Write base64 in 76-char lines per MIME spec
        for chunk in b64.as_bytes().chunks(76) {
          mime_msg.push_str(std::str::from_utf8(chunk).unwrap_or_default());
          mime_msg.push_str("\r\n");
        }
      }
      mime_msg.push_str(&format!("--{}--\r\n", boundary));

      let raw_b64 = base64::engine::general_purpose::STANDARD.encode(mime_msg.as_bytes());
      let mut body = format!("Action=SendRawEmail&Source={}&RawMessage.Data={}", urlencoding::encode(&self.from), urlencoding::encode(&raw_b64),);
      for (i, target) in self.targets.iter().enumerate() {
        body.push_str(&format!("&Destinations.member.{}={}", i + 1, urlencoding::encode(target)));
      }
      body
    } else {
      let mut body = format!(
        "Action=SendEmail&Source={}&Message.Subject.Data={}&Message.Body.Text.Data={}",
        urlencoding::encode(&self.from),
        urlencoding::encode(&ctx.title),
        urlencoding::encode(&ctx.body),
      );
      for (i, target) in self.targets.iter().enumerate() {
        body.push_str(&format!("&Destination.ToAddresses.member.{}={}", i + 1, urlencoding::encode(target)));
      }
      body
    };

    let (auth, datetime) = sigv4(&SigV4Params {
      method: "POST",
      endpoint: &endpoint,
      body: body.as_bytes(),
      access_key: &self.access_key,
      secret_key: &self.secret_key,
      region: &self.region,
      service: "ses",
      content_type,
    });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().map_err(NotifyError::Http)?;
    let resp = client
      .post(&endpoint)
      .header("User-Agent", APP_ID)
      .header("Content-Type", content_type)
      .header("X-Amz-Date", &datetime)
      .header("Authorization", &auth)
      .body(body)
      .send()
      .await?;
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

  const TEST_ACCESS_KEY_ID: &str = "AHIAJGNT76XIMXDBIJYA";
  const TEST_ACCESS_KEY_SECRET: &str = "bu1dHSdO22pfaaVy/wmNsdljF4C07D3bndi9PQJ9";
  const TEST_REGION: &str = "us-east-2";

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2",
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/user2@example.ca/user3@example.eu",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlaevi7FQ/us-east-1?to=user2@example.ca",
      "ses://?from=user@example.com&region=us-west-2&access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlaevi7FQ&reply=No One <noreply@yahoo.ca>&bcc=user.bcc@example.com,user2.bcc@example.com,invalid-email&cc=user.cc@example.com,user2.cc@example.com,invalid-email&to=user2@example.ca",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?name=From%20Name&to=user2@example.ca,invalid-email",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?format=text",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiacevi7FQ/us-west-2/?to=invalid-email",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlavi7FQ/us-west-2/user2@example.com",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "ses://",
      "ses://:@/",
      "ses://user@example.com/T1JJ3T3L2",
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/",
      "ses://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2",
      "ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/user2@example.com",
      "ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2?reply=invalid-email",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields_path_format() {
    // ses://user@domain/access_key/secret_key/region
    // The URL parser splits on '/', so path parts are:
    // [0]=access_key, [1]=secret_key, [2]=region
    let parsed = crate::utils::parse::ParsedUrl::parse("ses://user@example.com/MYACCESSKEY/MYSECRETKEY/us-west-2").unwrap();
    let ses = Ses::from_url(&parsed).unwrap();
    assert_eq!(ses.from, "user@example.com");
    assert_eq!(ses.access_key, "MYACCESSKEY");
    assert_eq!(ses.secret_key, "MYSECRETKEY");
    assert_eq!(ses.region, "us-west-2");
  }

  #[test]
  fn test_from_url_fields_with_targets() {
    let parsed =
      crate::utils::parse::ParsedUrl::parse("ses://user@example.com/T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/user2@example.ca/user3@example.eu").unwrap();
    let ses = Ses::from_url(&parsed).unwrap();
    assert_eq!(ses.from, "user@example.com");
    assert!(ses.targets.contains(&"user2@example.ca".to_string()));
    assert!(ses.targets.contains(&"user3@example.eu".to_string()));
    assert_eq!(ses.targets.len(), 2);
    assert_eq!(ses.region, "us-west-2");
  }

  #[test]
  fn test_from_url_query_format() {
    let parsed = crate::utils::parse::ParsedUrl::parse(
      "ses://?from=user@example.com&region=us-west-2&access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlaevi7FQ&to=user2@example.ca",
    )
    .unwrap();
    let ses = Ses::from_url(&parsed).unwrap();
    assert_eq!(ses.from, "user@example.com");
    assert_eq!(ses.access_key, "T1JJ3T3L2");
    assert_eq!(ses.region, "us-west-2");
    assert!(ses.targets.contains(&"user2@example.ca".to_string()));
  }

  #[test]
  fn test_invalid_reply_address() {
    let parsed = crate::utils::parse::ParsedUrl::parse("ses://user@example.com/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2?reply=invalid-email").unwrap();
    assert!(Ses::from_url(&parsed).is_none());
  }

  #[test]
  fn test_service_details() {
    let d = Ses::static_details();
    assert_eq!(d.service_name, "AWS SES");
    assert!(d.protocols.contains(&"ses"));
    assert!(d.attachment_support);
  }
}

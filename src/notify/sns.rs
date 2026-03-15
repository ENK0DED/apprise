use async_trait::async_trait;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails, APP_ID};
use crate::utils::aws::sigv4;
use crate::utils::parse::ParsedUrl;

pub struct Sns { access_key: String, secret_key: String, region: String, targets: Vec<String>, tags: Vec<String> }
impl Sns {
    pub fn from_url(url: &ParsedUrl) -> Option<Self> {
        // sns://access_key:secret_key@region/target
        // or sns://access_key_id/secret_key/region/target
        let (access_key, secret_key, region, targets) = if url.password.is_some() {
            let ak = url.user.clone()?;
            let sk = url.password.clone()?;
            let region = url.host.clone().unwrap_or_else(|| "us-east-1".to_string());
            let targets = url.path_parts.clone();
            (ak, sk, region, targets)
        } else if url.get("access").is_some() || url.get("secret").is_some() {
            // sns://?access=KEY&secret=SECRET&region=REGION&to=TARGET
            let ak = url.get("access").map(|s| s.to_string())?;
            let sk = url.get("secret").map(|s| s.to_string())?;
            let region = url.get("region").unwrap_or("us-east-1").to_string();
            let mut tgts = Vec::new();
            if let Some(to) = url.get("to") {
                tgts.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            (ak, sk, region, tgts)
        } else {
            // All from host + path: sns://KEY/SECRET/REGION/TARGET...
            let ak = url.host.clone()?;
            if url.path_parts.len() < 2 { return None; }
            let sk = url.path_parts.get(0)?.clone();
            let region = url.path_parts.get(1)?.clone();
            let mut targets: Vec<String> = url.path_parts.get(2..).unwrap_or(&[]).to_vec();
            if let Some(to) = url.get("to") {
                targets.extend(to.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            (ak, sk, region, targets)
        };
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::registry::from_url;

    #[test]
    fn test_valid_urls() {
        let urls = vec![
            "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/12223334444",
            "sns://?access=T1JJ3T3L2&secret=A1BRTD4JD/TIiajkdnlazkcevi7FQ&region=us-west-2&to=12223334444",
            "sns://T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/12223334444/12223334445",
            "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/us-east-1?to=12223334444",
            "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcevi7FQ/us-west-2/15556667777",
        ];
        for url in &urls {
            assert!(from_url(url).is_some(), "Should parse: {}", url);
        }
    }

    #[test]
    fn test_invalid_urls() {
        let urls = vec![
            "sns://",
            "sns://:@/",
            "sns://T1JJ3T3L2",
            "sns://T1JJ3TD4JD/TIiajkdnlazk7FQ/",
        ];
        for url in &urls {
            assert!(from_url(url).is_none(), "Should not parse: {}", url);
        }
    }

    #[test]
    fn test_from_url_path_form_fields() {
        // sns://ACCESS_KEY/SECRET_KEY/REGION/TARGET
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "sns://AHIAJGNT76XIMXDBIJYA/bu1dHSdO22pfaaVy/us-east-2/12223334444"
        ).unwrap();
        let sns = Sns::from_url(&parsed).unwrap();
        assert_eq!(sns.access_key, "AHIAJGNT76XIMXDBIJYA");
        assert_eq!(sns.secret_key, "bu1dHSdO22pfaaVy");
        assert_eq!(sns.region, "us-east-2");
        assert_eq!(sns.targets.len(), 1);
        assert!(sns.targets.contains(&"12223334444".to_string()));
    }

    #[test]
    fn test_from_url_query_form_fields() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "sns://?access=MYKEY&secret=MYSECRET&region=us-west-2&to=12223334444"
        ).unwrap();
        let sns = Sns::from_url(&parsed).unwrap();
        assert_eq!(sns.access_key, "MYKEY");
        assert_eq!(sns.secret_key, "MYSECRET");
        assert_eq!(sns.region, "us-west-2");
        assert_eq!(sns.targets, vec!["12223334444"]);
    }

    #[test]
    fn test_from_url_multiple_targets() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "sns://T1JJ3TD4JD/TIiajkdnlazk7FQ/us-west-2/12223334444/12223334445"
        ).unwrap();
        let sns = Sns::from_url(&parsed).unwrap();
        assert_eq!(sns.targets.len(), 2);
        assert!(sns.targets.contains(&"12223334444".to_string()));
        assert!(sns.targets.contains(&"12223334445".to_string()));
    }

    #[test]
    fn test_from_url_with_to_param() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "sns://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/us-east-1?to=12223334444"
        ).unwrap();
        let sns = Sns::from_url(&parsed).unwrap();
        assert!(sns.targets.contains(&"12223334444".to_string()));
    }

    #[test]
    fn test_from_url_no_recipients() {
        let parsed = crate::utils::parse::ParsedUrl::parse(
            "sns://AHIAJGNT76XIMXDBIJYA/bu1dHSdO22pfaaVy/us-east-2/"
        ).unwrap();
        let sns = Sns::from_url(&parsed).unwrap();
        assert!(sns.targets.is_empty());
    }

    #[test]
    fn test_static_details() {
        let details = Sns::static_details();
        assert_eq!(details.service_name, "AWS SNS");
        assert_eq!(details.service_url, Some("https://aws.amazon.com/sns/"));
        assert!(details.protocols.contains(&"sns"));
        assert!(!details.attachment_support);
    }
}

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

fn sha256_hex(data: &[u8]) -> String {
  hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
  let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
  mac.update(data);
  mac.finalize().into_bytes().to_vec()
}

/// Parameters for AWS SigV4 signing.
pub struct SigV4Params<'a> {
  pub method: &'a str,
  pub endpoint: &'a str,
  pub body: &'a [u8],
  pub access_key: &'a str,
  pub secret_key: &'a str,
  pub region: &'a str,
  pub service: &'a str,
  pub content_type: &'a str,
}

/// Compute AWS SigV4 Authorization and return `(authorization_value, x_amz_date_value)`.
///
/// Caller must set both `Authorization` and `X-Amz-Date` headers on the request,
/// plus whatever `Content-Type` was passed in.
pub fn sigv4(params: &SigV4Params<'_>) -> (String, String) {
  let SigV4Params { method, endpoint, body, access_key, secret_key, region, service, content_type } = params;
  let parsed = url::Url::parse(endpoint).expect("endpoint must be a valid URL");
  let uri = if parsed.path().is_empty() { "/" } else { parsed.path() };
  let signed = sign(
    &SigV4Request {
      method,
      host: parsed.host_str().unwrap_or(""),
      uri,
      query: parsed.query().unwrap_or(""),
      body,
      content_type,
      access_key,
      secret_key,
      session_token: None,
      region,
      service,
    },
    Utc::now(),
  );
  (signed.authorization, signed.amz_date)
}

/// One request to sign with AWS Signature Version 4.
///
/// `host` is the host the request is signed for (e.g.
/// `sns.us-east-1.amazonaws.com`); upstream Apprise always signs the real AWS
/// host, so it is passed separately from the URL the request is sent to.
pub struct SigV4Request<'a> {
  pub method: &'a str,
  pub host: &'a str,
  /// Canonical URI, e.g. `/`
  pub uri: &'a str,
  /// Canonical (already encoded and sorted) query string, usually empty
  pub query: &'a str,
  pub body: &'a [u8],
  pub content_type: &'a str,
  pub access_key: &'a str,
  pub secret_key: &'a str,
  /// Temporary-credential session token, sent and signed as
  /// `X-Amz-Security-Token` when set.
  pub session_token: Option<&'a str>,
  pub region: &'a str,
  pub service: &'a str,
}

/// The headers a signed request has to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigV4Headers {
  /// `Authorization` header value
  pub authorization: String,
  /// `X-Amz-Date` header value
  pub amz_date: String,
}

/// Sign `req` as of `now`. Signs the `content-type`, `host` and `x-amz-date`
/// headers, plus `x-amz-security-token` when a session token is set; the
/// caller sends `X-Amz-Security-Token` itself in that case.
pub fn sign(req: &SigV4Request<'_>, now: DateTime<Utc>) -> SigV4Headers {
  let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
  let date = &datetime[..8];

  // Canonical headers: lowercase, sorted, newline-terminated
  let mut canonical_headers = format!("content-type:{}\nhost:{}\nx-amz-date:{}\n", req.content_type, req.host, datetime);
  let mut signed_headers = String::from("content-type;host;x-amz-date");
  if let Some(token) = req.session_token {
    canonical_headers.push_str(&format!("x-amz-security-token:{}\n", token));
    signed_headers.push_str(";x-amz-security-token");
  }

  let canonical_request = format!("{}\n{}\n{}\n{}\n{}\n{}", req.method, req.uri, req.query, canonical_headers, signed_headers, sha256_hex(req.body));

  let credential_scope = format!("{}/{}/{}/aws4_request", date, req.region, req.service);
  let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", datetime, credential_scope, sha256_hex(canonical_request.as_bytes()));

  let k_date = hmac_sha256(format!("AWS4{}", req.secret_key).as_bytes(), date.as_bytes());
  let k_region = hmac_sha256(&k_date, req.region.as_bytes());
  let k_service = hmac_sha256(&k_region, req.service.as_bytes());
  let k_signing = hmac_sha256(&k_service, b"aws4_request");
  let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

  let authorization = format!("AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}", req.access_key, credential_scope, signed_headers, signature);
  SigV4Headers { authorization, amz_date: datetime }
}

/// The parts of an AWS XML response a plugin cares about (upstream
/// `utils.xml.flatten_xml_response`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AwsResponse {
  /// Name of the root element (`PublishResponse`, `ErrorResponse`, ...);
  /// `None` when the response was not well-formed XML.
  pub kind: Option<String>,
  /// Text of every kept leaf element, keyed by its `keep` name.
  pub fields: HashMap<String, String>,
}

impl AwsResponse {
  pub fn get(&self, key: &str) -> Option<&str> {
    self.fields.get(key).map(String::as_str)
  }
}

/// Flatten a small AWS XML response: the trimmed text of every leaf element
/// whose tag is a key of `keep` is stored under the mapped name. Tags are
/// matched exactly as spelled, so a default namespace on the root does not
/// matter. Malformed XML, or a document carrying a DTD, yields an empty
/// response with no `kind`.
pub fn flatten_response(xml: &str, keep: &[(&str, &str)]) -> AwsResponse {
  match parse_leaves(xml) {
    Some((root, leaves)) => {
      let mut fields = HashMap::new();
      for (tag, text) in leaves {
        if let Some((_, key)) = keep.iter().find(|(t, _)| *t == tag) {
          fields.insert(key.to_string(), text.trim().to_string());
        }
      }
      AwsResponse { kind: Some(root), fields }
    }
    None => AwsResponse::default(),
  }
}

/// Walk an XML document without building a tree. Returns the root tag and
/// every leaf element as `(tag, text)` in document order, or `None` when the
/// document is not well-formed.
fn parse_leaves(xml: &str) -> Option<(String, Vec<(String, String)>)> {
  let mut stack: Vec<Open> = Vec::new();
  let mut leaves = Vec::new();
  let mut root: Option<String> = None;
  let mut done = false;
  let mut rest = xml;

  while !rest.is_empty() {
    let Some(lt) = rest.find('<') else {
      // Trailing text
      text_into(&mut stack, rest)?;
      break;
    };
    text_into(&mut stack, &rest[..lt])?;
    rest = &rest[lt..];

    if let Some(after) = rest.strip_prefix("<?") {
      rest = &after[after.find("?>")? + 2..];
    } else if let Some(after) = rest.strip_prefix("<!--") {
      rest = &after[after.find("-->")? + 3..];
    } else if let Some(after) = rest.strip_prefix("<![CDATA[") {
      let end = after.find("]]>")?;
      stack.last_mut()?.text.push_str(&after[..end]);
      rest = &after[end + 3..];
    } else if rest.starts_with("<!") {
      // DTDs (and anything else declared) are refused outright
      return None;
    } else if let Some(after) = rest.strip_prefix("</") {
      let end = after.find('>')?;
      let tag = after[..end].trim_end();
      let open = stack.pop()?;
      if open.tag != tag {
        return None;
      }
      if open.leaf {
        leaves.push((open.tag, open.text));
      }
      if stack.is_empty() {
        done = true;
      }
      rest = &after[end + 1..];
    } else {
      let after = &rest[1..];
      let end = tag_end(after)?;
      let inner = &after[..end];
      let (inner, self_closing) = match inner.strip_suffix('/') {
        Some(i) => (i, true),
        None => (inner, false),
      };
      let tag: String = inner.chars().take_while(|c| !c.is_whitespace()).collect();
      if tag.is_empty() || done {
        return None;
      }
      if let Some(parent) = stack.last_mut() {
        parent.leaf = false;
      } else {
        root = Some(tag.clone());
      }
      if self_closing {
        leaves.push((tag, String::new()));
        if stack.is_empty() {
          done = true;
        }
      } else {
        stack.push(Open { tag, text: String::new(), leaf: true });
      }
      rest = &after[end + 1..];
    }
  }

  if !stack.is_empty() || !done {
    return None;
  }
  Some((root?, leaves))
}

/// Index of the `>` closing a start tag, skipping quoted attribute values.
fn tag_end(s: &str) -> Option<usize> {
  let mut quote: Option<char> = None;
  for (i, c) in s.char_indices() {
    match (quote, c) {
      (Some(q), c) if c == q => quote = None,
      (Some(_), _) => {}
      (None, '"' | '\'') => quote = Some(c),
      (None, '>') => return Some(i),
      (None, '<') => return None,
      _ => {}
    }
  }
  None
}

/// An element that has been opened but not closed yet.
struct Open {
  tag: String,
  text: String,
  leaf: bool,
}

/// Append character data to the open element; text outside the root element
/// may only be whitespace.
fn text_into(stack: &mut [Open], raw: &str) -> Option<()> {
  if raw.is_empty() {
    return Some(());
  }
  match stack.last_mut() {
    Some(open) => {
      open.text.push_str(&decode_entities(raw)?);
      Some(())
    }
    None => raw.trim().is_empty().then_some(()),
  }
}

/// Decode the predefined XML entities and character references.
fn decode_entities(raw: &str) -> Option<String> {
  if !raw.contains('&') {
    return Some(raw.to_string());
  }
  let mut out = String::with_capacity(raw.len());
  let mut rest = raw;
  while let Some(amp) = rest.find('&') {
    out.push_str(&rest[..amp]);
    let after = &rest[amp + 1..];
    let semi = after.find(';')?;
    let name = &after[..semi];
    let decoded = match name {
      "lt" => '<',
      "gt" => '>',
      "amp" => '&',
      "quot" => '"',
      "apos" => '\'',
      _ => {
        let code = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
          u32::from_str_radix(hex, 16).ok()?
        } else {
          name.strip_prefix('#')?.parse().ok()?
        };
        char::from_u32(code)?
      }
    };
    out.push(decoded);
    rest = &after[semi + 1..];
  }
  out.push_str(rest);
  Some(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone;

  const KEY_ID: &str = "AHIAJGNT76XIMXDBIJYA";
  const SECRET: &str = "bu1dHSdO22pfaaVy/wmNsdljF4C07D3bndi9PQJ9";
  const PAYLOAD: &str = "Action=Publish&Message=test&Version=2010-03-31&PhoneNumber=%2B18001234567";

  fn request(token: Option<&'static str>) -> SigV4Request<'static> {
    SigV4Request {
      method: "POST",
      host: "sns.us-east-2.amazonaws.com",
      uri: "/",
      query: "",
      body: PAYLOAD.as_bytes(),
      content_type: "application/x-www-form-urlencoded; charset=utf-8",
      access_key: KEY_ID,
      secret_key: SECRET,
      session_token: token,
      region: "us-east-2",
      service: "sns",
    }
  }

  #[test]
  fn test_sign_matches_upstream() {
    // Expected values computed by upstream NotifySNS.aws_prepare_request()
    // with the clock fixed at 2026-01-02T03:04:05Z.
    let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let signed = sign(&request(None), now);
    assert_eq!(signed.amz_date, "20260102T030405Z");
    assert_eq!(
      signed.authorization,
      "AWS4-HMAC-SHA256 Credential=AHIAJGNT76XIMXDBIJYA/20260102/us-east-2/sns/aws4_request, SignedHeaders=content-type;host;x-amz-date, \
       Signature=cf9778eb5d3cba77815f1c129d8392f0ef6b3b6251d64072338a840ae68bbed6"
    );

    let signed = sign(&request(Some("FwoGZXIvYXdz+SESSION/TOKENABCDEFGHIJKLMNOPQRS=")), now);
    assert_eq!(
      signed.authorization,
      "AWS4-HMAC-SHA256 Credential=AHIAJGNT76XIMXDBIJYA/20260102/us-east-2/sns/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, \
       Signature=b33eacc596233e4056df71bc548d72fc77d94815bffe47f7d4c6f013992212a2"
    );
  }

  #[test]
  fn test_sign_matches_apprise_go_golden() {
    // apprise-go parity fixtures (captured from Python 2.1.0) at 2024-01-01T00:00:00Z
    let now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let body = "Action=Publish&Message=hello&Version=2010-03-31&PhoneNumber=%2B15551234567";
    let req = |token| SigV4Request {
      method: "POST",
      host: "sns.us-east-1.amazonaws.com",
      uri: "/",
      query: "",
      body: body.as_bytes(),
      content_type: "application/x-www-form-urlencoded; charset=utf-8",
      access_key: "access123",
      secret_key: "secret123",
      session_token: token,
      region: "us-east-1",
      service: "sns",
    };
    assert!(sign(&req(None), now).authorization.ends_with("Signature=8ba0fc0a3ef88783a48f63f0c64171302b4338747414077c2725af66519c4dbb"));
    assert!(sign(&req(Some("sessiontoken123")), now).authorization.ends_with("Signature=360ae1973288b294476017c06dc095341b0889c2383f5eee6d2becff68efa4aa"));
  }

  #[test]
  fn test_sigv4_legacy_wrapper() {
    let (auth, date) = sigv4(&SigV4Params {
      method: "POST",
      endpoint: "https://sns.us-east-2.amazonaws.com/",
      body: PAYLOAD.as_bytes(),
      access_key: KEY_ID,
      secret_key: SECRET,
      region: "us-east-2",
      service: "sns",
      content_type: "application/x-www-form-urlencoded; charset=utf-8",
    });
    assert_eq!(date.len(), 16);
    assert!(auth.starts_with(&format!(
      "AWS4-HMAC-SHA256 Credential={}/{}/us-east-2/sns/aws4_request, SignedHeaders=content-type;host;x-amz-date, ",
      KEY_ID,
      &date[..8]
    )));
  }

  const KEEP: &[(&str, &str)] = &[
    ("RequestId", "request_id"),
    ("TopicArn", "topic_arn"),
    ("MessageId", "message_id"),
    ("Type", "error_type"),
    ("Code", "error_code"),
    ("Message", "error_message"),
  ];

  #[test]
  fn test_flatten_bad_input() {
    for bad in [
      "",
      "<Bad Response xmlns=\"http://sns.amazonaws.com/doc/2010-03-31/\">",
      "<a></b>",
      "<a>x</a>junk",
      "<a/><b/>",
      "plain text",
      "<!DOCTYPE a><a>x</a>",
      "<a>&bogus;</a>",
    ] {
      let r = flatten_response(bad, KEEP);
      assert_eq!(r.kind, None, "{bad:?}");
      assert!(r.fields.is_empty(), "{bad:?}");
    }
  }

  #[test]
  fn test_flatten_responses() {
    let r = flatten_response("<SingleElement></SingleElement>", KEEP);
    assert_eq!(r.kind.as_deref(), Some("SingleElement"));
    assert_eq!(r.get("request_id"), None);

    let r = flatten_response(
      r#"<?xml version="1.0"?>
        <PublishResponse xmlns="http://sns.amazonaws.com/doc/2010-03-31/">
            <PublishResult>
                <MessageId>5e16935a-d1fb-5a31-a716-c7805e5c1d2e</MessageId>
            </PublishResult>
            <ResponseMetadata>
                <RequestId>dc258024-d0e6-56bb-af1b-d4fe5f4181a4</RequestId>
            </ResponseMetadata>
        </PublishResponse>
        "#,
      KEEP,
    );
    assert_eq!(r.kind.as_deref(), Some("PublishResponse"));
    assert_eq!(r.get("request_id"), Some("dc258024-d0e6-56bb-af1b-d4fe5f4181a4"));
    assert_eq!(r.get("message_id"), Some("5e16935a-d1fb-5a31-a716-c7805e5c1d2e"));

    let r = flatten_response(
      r#"<ErrorResponse xmlns="http://sns.amazonaws.com/doc/2010-03-31/">
            <Error>
                <Type>Sender</Type>
                <Code>InvalidParameter</Code>
                <Message>Invalid parameter: TopicArn &amp; TargetArn Reason:
                no value for required parameter</Message>
            </Error>
            <RequestId>b5614883-babe-56ca-93b2-1c592ba6191e</RequestId>
        </ErrorResponse>"#,
      KEEP,
    );
    assert_eq!(r.kind.as_deref(), Some("ErrorResponse"));
    assert_eq!(r.get("error_type"), Some("Sender"));
    assert_eq!(r.get("error_code"), Some("InvalidParameter"));
    let msg = r.get("error_message").unwrap();
    assert!(msg.starts_with("Invalid parameter: TopicArn & TargetArn"));
    assert!(msg.ends_with("required parameter"));

    // An empty known element is blank, not missing
    let r = flatten_response("<ErrorResponse><Error><Message/></Error></ErrorResponse>", KEEP);
    assert_eq!(r.get("error_message"), Some(""));

    // Deep nesting does not recurse; only the leaf is kept
    let depth = 5000;
    let doc = format!("{}abc{}", "<RequestId>".repeat(depth), "</RequestId>".repeat(depth));
    let r = flatten_response(&doc, KEEP);
    assert_eq!(r.kind.as_deref(), Some("RequestId"));
    assert_eq!(r.get("request_id"), Some("abc"));
  }
}

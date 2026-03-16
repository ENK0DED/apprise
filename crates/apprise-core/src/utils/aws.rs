use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

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
  let now = Utc::now();
  let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
  let date = &datetime[..8];

  let parsed = url::Url::parse(endpoint).expect("endpoint must be a valid URL");
  let host = parsed.host_str().unwrap_or("");
  let uri = if parsed.path().is_empty() { "/" } else { parsed.path() };
  let query = parsed.query().unwrap_or("");

  // Canonical headers — must be lowercase, sorted, newline-terminated
  let canonical_headers = format!("content-type:{}\nhost:{}\nx-amz-date:{}\n", content_type, host, datetime);
  let signed_headers = "content-type;host;x-amz-date";

  let canonical_request = format!("{}\n{}\n{}\n{}\n{}\n{}", method, uri, query, canonical_headers, signed_headers, sha256_hex(body));

  let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);
  let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", datetime, credential_scope, sha256_hex(canonical_request.as_bytes()));

  let k_date = hmac_sha256(format!("AWS4{}", secret_key).as_bytes(), date.as_bytes());
  let k_region = hmac_sha256(&k_date, region.as_bytes());
  let k_service = hmac_sha256(&k_region, service.as_bytes());
  let k_signing = hmac_sha256(&k_service, b"aws4_request");
  let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

  let auth = format!("AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}", access_key, credential_scope, signed_headers, signature);
  (auth, datetime)
}

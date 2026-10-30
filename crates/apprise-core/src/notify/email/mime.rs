//! MIME messages as Python's `email` package writes them.
//!
//! Upstream builds its messages with `email.mime` (`MIMEText`,
//! `MIMEMultipart`, `MIMEApplication`, `MIMEBase`) and serialises them with
//! `Message.as_string()` under the `compat32` policy, after Apprise
//! registers quoted-printable for UTF-8. This module reproduces that output:
//! the same header order (including `set_type()` reordering), the same
//! quoted-printable and base64 bodies, `===============NNN==` boundaries
//! chosen when the message is first serialised, and RFC 2047 `Q` encoding
//! of non-ASCII header values. Lines end with `\n`; the SMTP layer turns
//! them into CRLF.

use base64::Engine;

use crate::utils::email::header_encode;

/// One MIME entity: its headers and either a payload or sub-parts.
#[derive(Debug, Clone)]
pub struct Part {
  headers: Vec<(String, String, Charset)>,
  body: Body,
}

/// How a non-ASCII header value is encoded, which depends on how upstream
/// set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
  /// A plain string: `compat32` wraps it in `Header(value)`, whose UTF-8
  /// fallback picks the shorter of base64 and quoted-printable.
  Str,
  /// `Header(value, "utf-8")`: quoted-printable, as Apprise registers it.
  Utf8,
}

#[derive(Debug, Clone)]
enum Body {
  /// An encoded payload, written line by line.
  Leaf(String),
  /// Sub-parts, separated by the boundary chosen on first serialisation.
  Multipart(Vec<Part>),
}

impl Part {
  /// `MIMEText(text, subtype, "utf-8")`: a quoted-printable UTF-8 part.
  pub fn text_utf8(text: &str, subtype: &str) -> Self {
    let mut part = Self::leaf(&format!("text/{}; charset=\"utf-8\"", subtype));
    part.push("Content-Transfer-Encoding", "quoted-printable");
    part.body = Body::Leaf(qp_body_encode(text.as_bytes()));
    part
  }

  /// `MIMEText(text, subtype)` for ASCII text: a `us-ascii`, `7bit` part.
  pub fn text_ascii(text: &str, subtype: &str) -> Self {
    let mut part = Self::leaf(&format!("text/{}; charset=\"us-ascii\"", subtype));
    part.push("Content-Transfer-Encoding", "7bit");
    part.body = Body::Leaf(text.to_string());
    part
  }

  /// `MIMEApplication(data)`: base64 `application/octet-stream`.
  pub fn application(data: &[u8]) -> Self {
    let mut part = Self::leaf("application/octet-stream");
    part.push("Content-Transfer-Encoding", "base64");
    part.body = Body::Leaf(base64_encodebytes(data));
    part
  }

  /// `MIMEBase(maintype, subtype)` with `payload` set as is.
  pub fn base(content_type: &str, payload: &str) -> Self {
    let mut part = Self::leaf(content_type);
    part.body = Body::Leaf(payload.to_string());
    part
  }

  /// `MIMEMultipart(subtype, **params)`; params are written quoted, in order.
  pub fn multipart(subtype: &str, params: &[(&str, &str)]) -> Self {
    let mut ctype = format!("multipart/{}", subtype);
    for (k, v) in params {
      ctype.push_str(&format!("; {}=\"{}\"", k, quote_param(v)));
    }
    let mut part = Self { headers: Vec::new(), body: Body::Multipart(Vec::new()) };
    part.push("Content-Type", &ctype);
    part.push("MIME-Version", "1.0");
    part
  }

  fn leaf(content_type: &str) -> Self {
    let mut part = Self { headers: Vec::new(), body: Body::Leaf(String::new()) };
    part.push("Content-Type", content_type);
    part.push("MIME-Version", "1.0");
    part
  }

  fn push(&mut self, name: &str, value: &str) {
    self.headers.push((name.to_string(), value.to_string(), Charset::Str));
  }

  /// `msg[name] = value`: append a header (an existing one is kept).
  pub fn add_header(&mut self, name: &str, value: &str) {
    self.push(name, value);
  }

  /// `msg[name] = Header(value, "utf-8")`: append a header whose non-ASCII
  /// value is quoted-printable encoded.
  pub fn add_header_utf8(&mut self, name: &str, value: &str) {
    self.headers.push((name.to_string(), value.to_string(), Charset::Utf8));
  }

  /// `del msg[name]`: drop every header with this name.
  pub fn remove_header(&mut self, name: &str) {
    self.headers.retain(|(k, _, _)| !k.eq_ignore_ascii_case(name));
  }

  /// The first value of a header.
  pub fn header(&self, name: &str) -> Option<&str> {
    self.headers.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(name)).map(|(_, v, _)| v.as_str())
  }

  /// `msg.set_type(ctype)`: replace the type, keep its parameters. As in
  /// Python, `MIME-Version` and `Content-Type` move to the end.
  pub fn set_type(&mut self, ctype: &str) {
    let params = self.header("Content-Type").and_then(|v| v.split_once(';').map(|(_, p)| format!(";{}", p))).unwrap_or_default();
    self.remove_header("MIME-Version");
    self.push("MIME-Version", "1.0");
    self.remove_header("Content-Type");
    self.push("Content-Type", &format!("{}{}", ctype, params));
  }

  /// Add a sub-part to a multipart entity.
  pub fn attach(&mut self, part: Part) {
    if let Body::Multipart(parts) = &mut self.body {
      parts.push(part);
    }
  }

  /// `msg.as_string()`. The first call picks the boundaries and stores
  /// them, so later calls (and enclosing messages) produce the same text.
  pub fn as_string(&mut self) -> String {
    let mut out = String::new();
    self.flatten(&mut out);
    out
  }

  fn flatten(&mut self, out: &mut String) {
    // As Python's generator: the body first, so a new boundary is in the
    // Content-Type header before the headers are written
    let mut body = String::new();
    let mut boundary_needed = None;
    match &mut self.body {
      Body::Leaf(payload) => write_lines(payload, &mut body),
      Body::Multipart(parts) => {
        let texts: Vec<String> = parts.iter_mut().map(Part::as_string).collect();
        let boundary = match boundary_of(self.header("Content-Type").unwrap_or_default()) {
          Some(b) => b,
          None => {
            let b = make_boundary(&texts.join("\n"));
            boundary_needed = Some(b.clone());
            b
          }
        };
        body.push_str(&format!("--{}\n", boundary));
        for (i, text) in texts.iter().enumerate() {
          if i > 0 {
            body.push_str(&format!("\n--{}\n", boundary));
          }
          body.push_str(text);
        }
        body.push_str(&format!("\n--{}--\n", boundary));
      }
    }
    if let Some(b) = boundary_needed {
      self.set_boundary(&b);
    }
    for (name, value, charset) in &self.headers {
      out.push_str(name);
      out.push_str(": ");
      out.push_str(&match charset {
        Charset::Str => encode_header_value(value),
        Charset::Utf8 => encode_header_value_qp(value),
      });
      out.push('\n');
    }
    out.push('\n');
    out.push_str(&body);
  }

  fn set_boundary(&mut self, boundary: &str) {
    if let Some((_, value, _)) = self.headers.iter_mut().find(|(k, _, _)| k.eq_ignore_ascii_case("Content-Type")) {
      value.push_str(&format!("; boundary=\"{}\"", boundary));
    }
  }
}

/// How the `compat32` policy writes a plain string header without folding:
/// ASCII as is (line breaks become `\n`), anything else as one RFC 2047
/// encoded word in the shorter of base64 and quoted-printable (the
/// `Header(value)` UTF-8 fallback uses Python's default `SHORTEST`).
pub fn encode_header_value(value: &str) -> String {
  if value.is_ascii() {
    return value.replace("\r\n", "\n").replace('\r', "\n");
  }
  let bytes = value.as_bytes();
  let len64 = bytes.len().div_ceil(3) * 4;
  let lenqp: usize = bytes.iter().map(|b| if b.is_ascii_alphanumeric() || b"-!*+/ ".contains(b) { 1 } else { 3 }).sum();
  if len64 < lenqp { format!("=?utf-8?b?{}?=", base64::engine::general_purpose::STANDARD.encode(bytes)) } else { header_encode(value) }
}

/// A `Header(value, "utf-8")` value: ASCII as is, anything else as one
/// quoted-printable encoded word.
pub fn encode_header_value_qp(value: &str) -> String {
  if value.is_ascii() { encode_header_value(value) } else { header_encode(value) }
}

fn boundary_of(content_type: &str) -> Option<String> {
  content_type.split(';').skip(1).find_map(|p| p.trim().strip_prefix("boundary=").map(|b| b.trim_matches('"').to_string()))
}

/// `email.utils.quote()`: escape backslashes and double quotes.
fn quote_param(value: &str) -> String {
  value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `Generator._make_boundary()`: fifteen `=`, a random 19-digit number and
/// `==`, extended with `.N` should it appear in the text.
fn make_boundary(text: &str) -> String {
  let token = rand::random::<u64>() % (i64::MAX as u64);
  let boundary = format!("==============={:019}==", token);
  let collides = |b: &str| text.lines().any(|line| line.strip_prefix("--").is_some_and(|rest| rest == b || rest.strip_suffix("--") == Some(b)));
  let mut candidate = boundary.clone();
  let mut counter = 0;
  while collides(&candidate) {
    candidate = format!("{}.{}", boundary, counter);
    counter += 1;
  }
  candidate
}

/// `Generator._write_lines()`: normalise line endings to `\n`, without
/// adding one after the last line.
fn write_lines(text: &str, out: &mut String) {
  if text.is_empty() {
    return;
  }
  let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
  out.push_str(&normalised);
}

/// `email.quoprimime.body_encode()` with its defaults (76 characters per
/// line, `\n` line ends), over the bytes of the text.
pub fn qp_body_encode(data: &[u8]) -> String {
  if data.is_empty() {
    return String::new();
  }
  const SAFE: &[u8] = b" !\"#$%&'()*+,-./0123456789:;<>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\t\r\n";
  let mut translated = String::with_capacity(data.len() * 2);
  for &b in data {
    if SAFE.contains(&b) {
      translated.push(b as char);
    } else {
      translated.push_str(&format!("={:02X}", b));
    }
  }
  const MAXLINELEN: usize = 76;
  let quote = |c: char| format!("={:02X}", c as u32);
  let mut encoded: Vec<String> = Vec::new();
  for line in split_lines(&translated) {
    let chars: Vec<char> = line.chars().collect();
    let mut start: usize = 0;
    let laststart = chars.len() as isize - 1 - MAXLINELEN as isize;
    while (start as isize) <= laststart {
      let stop = start + MAXLINELEN - 1;
      // Never split an escape sequence
      if chars[stop - 2] == '=' {
        encoded.push(chars[start..stop - 1].iter().collect());
        start = stop - 2;
      } else if chars[stop - 1] == '=' {
        encoded.push(chars[start..stop].iter().collect());
        start = stop - 1;
      } else {
        encoded.push(chars[start..stop].iter().collect::<String>() + "=");
        start = stop;
      }
    }
    match chars.last() {
      // Whitespace at the end of a line must be encoded
      Some(&last) if last == ' ' || last == '\t' => {
        let room = start as isize - laststart;
        let q = if room >= 3 {
          quote(last)
        } else if room == 2 {
          format!("{}=\n", last)
        } else {
          format!("=\n{}", quote(last))
        };
        encoded.push(chars[start..chars.len() - 1].iter().collect::<String>() + &q);
      }
      _ => encoded.push(chars[start..].iter().collect()),
    }
  }
  if translated.ends_with(['\r', '\n']) {
    encoded.push(String::new());
  }
  encoded.join("\n")
}

/// Python's `str.splitlines()` over text whose only line breaks are CR/LF.
fn split_lines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let bytes = text.as_bytes();
  let mut start = 0;
  let mut i = 0;
  while i < bytes.len() {
    match bytes[i] {
      b'\r' => {
        lines.push(&text[start..i]);
        if bytes.get(i + 1) == Some(&b'\n') {
          i += 1;
        }
        start = i + 1;
      }
      b'\n' => {
        lines.push(&text[start..i]);
        start = i + 1;
      }
      _ => {}
    }
    i += 1;
  }
  if start < bytes.len() {
    lines.push(&text[start..]);
  }
  lines
}

/// `base64.encodebytes()`: 76-character lines, each ending with `\n`.
pub fn base64_encodebytes(data: &[u8]) -> String {
  let encoded = base64::engine::general_purpose::STANDARD.encode(data);
  let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 1);
  for chunk in encoded.as_bytes().chunks(76) {
    out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
    out.push('\n');
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_qp_body_encode_matches_python() {
    // Expected values from email.quoprimime.body_encode()
    assert_eq!(qp_body_encode(b""), "");
    assert_eq!(qp_body_encode(b"body"), "body");
    assert_eq!(qp_body_encode("Egy sor kódot egyszerre.".as_bytes()), "Egy sor k=C3=B3dot egyszerre.");
    assert_eq!(qp_body_encode(b"a=b\n"), "a=3Db\n");
    assert_eq!(qp_body_encode(b"trailing \nspace\t"), "trailing=20\nspace=09");
    assert_eq!(qp_body_encode(b"a\r\nb\rc"), "a\nb\nc");
    let long = "x".repeat(80);
    assert_eq!(qp_body_encode(long.as_bytes()), format!("{}=\n{}", "x".repeat(75), "x".repeat(5)));
    let long = format!("{}=", "x".repeat(74));
    assert_eq!(qp_body_encode(long.as_bytes()), format!("{}=\n=3D", "x".repeat(74)));
    let long = format!("{}é{}", "x".repeat(73), "y".repeat(10));
    assert_eq!(qp_body_encode(long.as_bytes()), format!("{}=\n=C3=A9{}", "x".repeat(73), "y".repeat(10)));
    let long = format!("{} ", "x".repeat(75));
    assert_eq!(qp_body_encode(long.as_bytes()), format!("{}=\n=20", "x".repeat(75)));
  }

  #[test]
  fn test_base64_encodebytes() {
    assert_eq!(base64_encodebytes(b""), "");
    assert_eq!(base64_encodebytes(b"attachment"), "YXR0YWNobWVudA==\n");
    let long = base64_encodebytes(&[0u8; 60]);
    assert_eq!(long.lines().map(str::len).collect::<Vec<_>>(), vec![76, 4]);
  }

  #[test]
  fn test_set_type_moves_headers() {
    let mut part = Part::text_ascii("Version: 1", "plain");
    part.set_type("application/pgp-encrypted");
    let text = part.as_string();
    assert_eq!(text, "Content-Transfer-Encoding: 7bit\nMIME-Version: 1.0\nContent-Type: application/pgp-encrypted; charset=\"us-ascii\"\n\nVersion: 1");
  }

  #[test]
  fn test_multipart_keeps_its_boundary() {
    let mut outer = Part::multipart("mixed", &[]);
    outer.attach(Part::text_utf8("hello", "plain"));
    let first = outer.as_string();
    assert_eq!(first, outer.as_string());
    let boundary = boundary_of(outer.header("Content-Type").unwrap()).unwrap();
    assert!(boundary.starts_with("===============") && boundary.ends_with("==") && boundary.len() == 36);
    assert!(first.ends_with(&format!("\n--{}--\n", boundary)));
  }

  #[test]
  fn test_header_encoding() {
    // Expected values from Python's email.header with Apprise's charset
    assert_eq!(encode_header_value("plain value"), "plain value");
    assert_eq!(encode_header_value("café"), "=?utf-8?b?Y2Fmw6k=?=");
    assert_eq!(encode_header_value("cafés au lait"), "=?utf-8?q?caf=C3=A9s_au_lait?=");
    assert_eq!(encode_header_value("attachment; filename=\"café résumé.txt\""), "=?utf-8?b?YXR0YWNobWVudDsgZmlsZW5hbWU9ImNhZsOpIHLDqXN1bcOpLnR4dCI=?=");
    assert_eq!(encode_header_value_qp("café"), "=?utf-8?q?caf=C3=A9?=");
    assert_eq!(encode_header_value_qp("日本"), "=?utf-8?q?=E6=97=A5=E6=9C=AC?=");
  }
}

// BSD 2-Clause License
//
// Apprise - Push Notification Library.
// Copyright (c) 2026, Chris Caron <lead2gold@gmail.com>
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

use std::collections::HashMap;

use crate::asset::AppriseAsset;
use crate::attachment::{AttachSource, parse_attachment};
use crate::error::AttachError;
use crate::types::NotifyType;

/// Defines the different template types we can perform parsing on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateType {
  /// RAW does nothing at all to the content being parsed.
  /// Data is taken at its absolute value.
  Raw,
  /// Data is presumed to be of type JSON and is therefore escaped
  /// if required to do so (such as quotes, backslashes, newlines, etc).
  Json,
}

/// Escape a string value for safe embedding in a JSON string.
/// This produces the inner content (without surrounding quotes),
/// matching Python's `json.dumps(content)[1:-1]`, which also escapes
/// every non-ASCII character (`ensure_ascii`).
fn escape_json(content: &str) -> String {
  let mut s = String::with_capacity(content.len());
  for ch in content.chars() {
    match ch {
      '"' => s.push_str("\\\""),
      '\\' => s.push_str("\\\\"),
      '\n' => s.push_str("\\n"),
      '\r' => s.push_str("\\r"),
      '\t' => s.push_str("\\t"),
      '\u{08}' => s.push_str("\\b"),
      '\u{0c}' => s.push_str("\\f"),
      // Everything outside ' '..='~' (json.dumps' ESCAPE_ASCII)
      c if !(' '..='~').contains(&c) => {
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units) {
          s.push_str(&format!("\\u{:04x}", unit));
        }
      }
      c => s.push(c),
    }
  }
  s
}

/// Takes a template string and applies keyword substitutions (upstream
/// `apply_template()`).
///
/// The template uses double curly braces `{{keyword}}` for placeholders.
/// Whitespace inside the braces is allowed (e.g., `{{ keyword }}`).
/// Matching is case-insensitive on the keyword names; an exact match is
/// preferred when two keys differ only in case. (Upstream matches without
/// case too, then fails looking the name up when its case differs.)
///
/// The `app_mode` parameter controls escaping:
/// - `TemplateType::Raw` - no escaping, values inserted as-is
/// - `TemplateType::Json` - values are JSON-escaped before insertion
///
/// If a placeholder has no matching key, it is left unchanged.
pub fn apply_template(template: &str, app_mode: TemplateType, kwargs: &HashMap<String, String>) -> String {
  if kwargs.is_empty() || template.is_empty() {
    return template.to_string();
  }

  // Build a lowercase lookup map for case-insensitive matching
  let mut lower_map: HashMap<String, &str> = HashMap::with_capacity(kwargs.len());
  for (k, v) in kwargs {
    let lower = k.to_lowercase();
    if *k == lower || !lower_map.contains_key(&lower) {
      lower_map.insert(lower, v.as_str());
    }
  }

  let escape_fn: fn(&str) -> String = match app_mode {
    TemplateType::Raw => |s: &str| s.to_string(),
    TemplateType::Json => escape_json,
  };

  let mut result = String::with_capacity(template.len());
  let mut rest = template;

  while let Some(open) = rest.find("{{") {
    // Find the closing `}}`
    let Some(close) = rest[open + 2..].find("}}") else { break };
    let inner = &rest[open + 2..open + 2 + close];
    let key = inner.trim();
    let value = kwargs.get(key).map(String::as_str).or_else(|| lower_map.get(&key.to_lowercase()).copied());
    match value {
      Some(val) => {
        result.push_str(&rest[..open]);
        result.push_str(&escape_fn(val));
        rest = &rest[open + 2 + close + 2..];
      }
      None => {
        // No match; emit the first brace and look again from the next one
        result.push_str(&rest[..open + 1]);
        rest = &rest[open + 1..];
      }
    }
  }

  result.push_str(rest);
  result
}

/// Build the standard template variables from a notification context.
///
/// These are the common variables available in all templates:
/// - `title` / `body` / `message` (alias for body)
/// - `type` (notification type)
/// - `app_id` / `app_desc` / `app_url`
pub fn build_template_vars(title: &str, body: &str, notify_type: &str, app_id: &str, app_desc: &str, app_url: &str) -> HashMap<String, String> {
  let mut vars = HashMap::new();
  vars.insert("title".to_string(), title.to_string());
  vars.insert("body".to_string(), body.to_string());
  vars.insert("message".to_string(), body.to_string());
  vars.insert("type".to_string(), notify_type.to_string());
  vars.insert("app_id".to_string(), app_id.to_string());
  vars.insert("app_desc".to_string(), app_desc.to_string());
  vars.insert("app_url".to_string(), app_url.to_string());
  vars
}

/// The tokens upstream's JSON payload templates (Slack, Discord,
/// Workflows, Telegram) are filled with: the user's own `?:name=value`
/// tokens, overridden by `app_body`, `app_title`, `app_type`, `app_id`,
/// `app_desc`, `app_color`, `app_color_hex`, `app_image_url` (empty when
/// there is no image) and `app_url`.
pub fn app_tokens<'a, I>(user: I, asset: &AppriseAsset, notify_type: &NotifyType, title: &str, body: &str, image_url: Option<&str>) -> HashMap<String, String>
where
  I: IntoIterator<Item = (&'a String, &'a String)>,
{
  let mut tokens: HashMap<String, String> = user.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect();
  let color = asset.color(notify_type);
  tokens.insert("app_body".into(), body.to_string());
  tokens.insert("app_title".into(), title.to_string());
  tokens.insert("app_type".into(), notify_type.as_str().to_string());
  tokens.insert("app_id".into(), asset.app_id.clone());
  tokens.insert("app_desc".into(), asset.app_desc.clone());
  tokens.insert("app_color".into(), color.clone());
  tokens.insert("app_color_hex".into(), color);
  tokens.insert("app_image_url".into(), image_url.unwrap_or_default().to_string());
  tokens.insert("app_url".into(), asset.app_url.clone());
  tokens
}

/// A payload template given with `?template=` (upstream keeps it in an
/// `AppriseAttachment` with a `max_file_size`): a local path, a
/// `file://` URL or an `http(s)://` URL.
#[derive(Debug, Clone)]
pub struct TemplateFile {
  source: AttachSource,
  max_size: usize,
}

impl TemplateFile {
  /// Parse a `?template=` value. `None`, after a warning, where upstream's
  /// `AppriseAttachment.add()` fails (an unsupported schema, an
  /// unparseable URL); the plugin must then refuse the URL.
  pub fn parse(value: &str, max_size: usize) -> Option<Self> {
    match parse_attachment(value) {
      Ok(source) => Some(Self { source, max_size }),
      Err(e) => {
        tracing::warn!("The template specified could not be loaded: {}", e);
        None
      }
    }
  }

  /// The source with secrets masked, for logs.
  pub fn loggable(&self) -> &str {
    self.source.loggable()
  }

  /// Read the template text. A missing or oversized template logs
  /// "Could not access {service} template ..." and one that cannot be
  /// read logs "{service} template ... could not be read.", as upstream.
  pub async fn read(&self, service: &str) -> Option<String> {
    let loaded = match self.source.load().await {
      Ok(a) if a.data.len() <= self.max_size => a,
      Ok(_) | Err(AttachError::NotFound(_)) | Err(AttachError::TooLarge { .. }) | Err(AttachError::Disallowed(_)) => {
        tracing::error!("Could not access {} template {}.", service, self.loggable());
        return None;
      }
      Err(_) => {
        tracing::error!("{} template {} could not be read.", service, self.loggable());
        return None;
      }
    };
    match String::from_utf8(loaded.data) {
      Ok(text) => Some(text),
      Err(_) => {
        tracing::error!("{} template {} could not be read.", service, self.loggable());
        None
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_apply_template_raw_basic() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());
    vars.insert("body".to_string(), "World".to_string());

    let result = apply_template("Title: {{title}}, Body: {{body}}", TemplateType::Raw, &vars);
    assert_eq!(result, "Title: Hello, Body: World");
  }

  #[test]
  fn test_apply_template_with_whitespace() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Test".to_string());

    let result = apply_template("{{ title }} and {{  title  }}", TemplateType::Raw, &vars);
    assert_eq!(result, "Test and Test");
  }

  #[test]
  fn test_apply_template_case_insensitive() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());

    let result = apply_template("{{Title}} {{TITLE}} {{title}}", TemplateType::Raw, &vars);
    assert_eq!(result, "Hello Hello Hello");
  }

  #[test]
  fn test_apply_template_no_match_unchanged() {
    let vars = HashMap::new();
    let template = "{{unknown}} stays";
    let result = apply_template(template, TemplateType::Raw, &vars);
    assert_eq!(result, "{{unknown}} stays");
  }

  #[test]
  fn test_apply_template_partial_match() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());

    let result = apply_template("{{title}} and {{missing}}", TemplateType::Raw, &vars);
    assert_eq!(result, "Hello and {{missing}}");
  }

  #[test]
  fn test_apply_template_json_escaping() {
    let mut vars = HashMap::new();
    vars.insert("body".to_string(), "He said \"hello\"\nNew line".to_string());

    let result = apply_template(r#"{"text": "{{body}}"}"#, TemplateType::Json, &vars);
    assert_eq!(result, r#"{"text": "He said \"hello\"\nNew line"}"#);
  }

  #[test]
  fn test_apply_template_json_backslash() {
    let mut vars = HashMap::new();
    vars.insert("path".to_string(), r"C:\Users\test".to_string());

    let result = apply_template(r#"{"path": "{{path}}"}"#, TemplateType::Json, &vars);
    assert_eq!(result, r#"{"path": "C:\\Users\\test"}"#);
  }

  #[test]
  fn test_apply_template_empty_template() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());

    let result = apply_template("", TemplateType::Raw, &vars);
    assert_eq!(result, "");
  }

  #[test]
  fn test_apply_template_empty_vars() {
    let vars = HashMap::new();
    let result = apply_template("no vars here", TemplateType::Raw, &vars);
    assert_eq!(result, "no vars here");
  }

  #[test]
  fn test_apply_template_single_braces_not_replaced() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());

    let result = apply_template("{title}", TemplateType::Raw, &vars);
    assert_eq!(result, "{title}");
  }

  #[test]
  fn test_build_template_vars() {
    let vars = build_template_vars("My Title", "My Body", "info", "Apprise", "Apprise Notifications", "https://github.com/caronc/apprise");

    assert_eq!(vars.get("title").unwrap(), "My Title");
    assert_eq!(vars.get("body").unwrap(), "My Body");
    assert_eq!(vars.get("message").unwrap(), "My Body");
    assert_eq!(vars.get("type").unwrap(), "info");
    assert_eq!(vars.get("app_id").unwrap(), "Apprise");
    assert_eq!(vars.get("app_desc").unwrap(), "Apprise Notifications");
    assert_eq!(vars.get("app_url").unwrap(), "https://github.com/caronc/apprise");
    assert_eq!(vars.len(), 7);
  }

  #[test]
  fn test_apply_template_multiple_occurrences() {
    let mut vars = HashMap::new();
    vars.insert("name".to_string(), "World".to_string());

    let result = apply_template("Hello {{name}}! Goodbye {{name}}!", TemplateType::Raw, &vars);
    assert_eq!(result, "Hello World! Goodbye World!");
  }

  #[test]
  fn test_apply_template_json_tab_chars() {
    let mut vars = HashMap::new();
    vars.insert("body".to_string(), "line1\tline2".to_string());

    let result = apply_template(r#"{"text": "{{body}}"}"#, TemplateType::Json, &vars);
    assert_eq!(result, r#"{"text": "line1\tline2"}"#);
  }

  #[test]
  fn test_template_type_equality() {
    assert_eq!(TemplateType::Raw, TemplateType::Raw);
    assert_eq!(TemplateType::Json, TemplateType::Json);
    assert_ne!(TemplateType::Raw, TemplateType::Json);
  }

  #[test]
  fn test_apply_template_no_closing_braces() {
    let mut vars = HashMap::new();
    vars.insert("title".to_string(), "Hello".to_string());

    let result = apply_template("{{title", TemplateType::Raw, &vars);
    assert_eq!(result, "{{title");
  }

  #[test]
  fn test_apply_template_nested_braces() {
    let mut vars = HashMap::new();
    vars.insert("x".to_string(), "val".to_string());

    // {{{x}}} should replace {{x}} -> "val" leaving outer braces
    // Actually: first {{ starts, then {x}} - inner is "{x" trimmed = "{x"
    // which won't match. Let's just verify it doesn't panic.
    let result = apply_template("{{{x}}}", TemplateType::Raw, &vars);
    // The parser sees {{ at pos 0, then finds }} at pos 4,
    // inner is "{x" trimmed = "{x" which doesn't match.
    // Then it emits '{' and advances. Next {{ at pos 1, inner is "x",
    // which matches -> "val", then remaining "}".
    assert_eq!(result, "{val}");
  }

  #[test]
  fn test_apply_template_keeps_unicode() {
    let mut vars = HashMap::new();
    vars.insert("name".to_string(), "Wörld".to_string());

    // Text around placeholders is copied as-is, multi-byte characters included
    let result = apply_template("Héllo {{name}} — ☃ {{missing}} ü", TemplateType::Raw, &vars);
    assert_eq!(result, "Héllo Wörld — ☃ {{missing}} ü");
  }

  #[test]
  fn test_apply_template_json_ensure_ascii() {
    let mut vars = HashMap::new();
    vars.insert("body".to_string(), "é 👉 \u{7f}\u{1}\u{8}\u{c}/".to_string());

    // json.dumps() escapes everything outside printable ASCII, astral
    // characters as surrogate pairs
    let result = apply_template(r#"{"text": "{{body}}"}"#, TemplateType::Json, &vars);
    assert_eq!(result, r#"{"text": "\u00e9 \ud83d\udc49 \u007f\u0001\b\f/"}"#);
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["text"], "é 👉 \u{7f}\u{1}\u{8}\u{c}/");
  }

  #[test]
  fn test_apply_template_prefers_exact_case() {
    let mut vars = HashMap::new();
    vars.insert("Key".to_string(), "upper".to_string());
    vars.insert("key".to_string(), "lower".to_string());

    assert_eq!(apply_template("{{Key}} {{key}} {{KEY}}", TemplateType::Raw, &vars), "upper lower lower");
  }

  #[test]
  fn test_app_tokens() {
    let asset = AppriseAsset::default();
    let user = indexmap::IndexMap::from([("app_body".to_string(), "user".to_string()), ("mine".to_string(), "x".to_string())]);
    let tokens = app_tokens(&user, &asset, &NotifyType::Warning, "T", "B", None);
    // app_* tokens win over the user's own
    assert_eq!(tokens["app_body"], "B");
    assert_eq!(tokens["app_title"], "T");
    assert_eq!(tokens["app_type"], "warning");
    assert_eq!(tokens["app_id"], "Apprise");
    assert_eq!(tokens["app_desc"], "Apprise Notifications");
    assert_eq!(tokens["app_color"], asset.color(&NotifyType::Warning));
    assert_eq!(tokens["app_color_hex"], tokens["app_color"]);
    assert_eq!(tokens["app_image_url"], "");
    assert_eq!(tokens["app_url"], "https://github.com/caronc/apprise");
    assert_eq!(tokens["mine"], "x");
  }

  #[tokio::test]
  async fn test_template_file() {
    let dir = std::env::temp_dir().join(format!("apprise-template-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.json");
    std::fs::write(&path, "{\"a\": \"{{app_body}}\"}").unwrap();

    let file = TemplateFile::parse(path.to_str().unwrap(), 100).unwrap();
    assert_eq!(file.read("Test").await.as_deref(), Some("{\"a\": \"{{app_body}}\"}"));

    // Too large, missing and unreadable templates are refused
    assert!(TemplateFile::parse(path.to_str().unwrap(), 5).unwrap().read("Test").await.is_none());
    assert!(TemplateFile::parse(dir.join("missing.json").to_str().unwrap(), 100).unwrap().read("Test").await.is_none());
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(file.read("Test").await.is_none());

    // An unsupported schema cannot be loaded at all
    assert!(TemplateFile::parse("invalid://template", 100).is_none());
    std::fs::remove_dir_all(&dir).ok();
  }
}

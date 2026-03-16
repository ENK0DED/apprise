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
/// matching Python's `json.dumps(content)[1:-1]`.
fn escape_json(content: &str) -> String {
  let serialized = serde_json::to_string(content).unwrap_or_else(|_| {
    // Fallback: manual escaping
    let mut s = String::with_capacity(content.len());
    for ch in content.chars() {
      match ch {
        '"' => s.push_str("\\\""),
        '\\' => s.push_str("\\\\"),
        '\n' => s.push_str("\\n"),
        '\r' => s.push_str("\\r"),
        '\t' => s.push_str("\\t"),
        c if (c as u32) < 0x20 => {
          s.push_str(&format!("\\u{:04x}", c as u32));
        }
        c => s.push(c),
      }
    }
    s
  });
  // Remove surrounding quotes from serde_json output
  if serialized.len() >= 2 { serialized[1..serialized.len() - 1].to_string() } else { serialized }
}

/// Takes a template string and applies keyword substitutions.
///
/// The template uses double curly braces `{{keyword}}` for placeholders.
/// Whitespace inside the braces is allowed (e.g., `{{ keyword }}`).
/// Matching is case-insensitive on the keyword names.
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
  let lower_map: HashMap<String, &str> = kwargs.iter().map(|(k, v)| (k.to_lowercase(), v.as_str())).collect();

  let escape_fn: fn(&str) -> String = match app_mode {
    TemplateType::Raw => |s: &str| s.to_string(),
    TemplateType::Json => escape_json,
  };

  let mut result = String::with_capacity(template.len());
  let bytes = template.as_bytes();
  let len = bytes.len();
  let mut i = 0;

  while i < len {
    // Look for opening `{{`
    if i + 1 < len && bytes[i] == b'{' && bytes[i + 1] == b'{' {
      // Find the closing `}}`
      if let Some(close_pos) = find_closing_braces(template, i + 2) {
        let inner = &template[i + 2..close_pos];
        let key = inner.trim().to_lowercase();

        if let Some(val) = lower_map.get(&key) {
          result.push_str(&escape_fn(val));
          i = close_pos + 2; // skip past `}}`
          continue;
        }
      }
    }

    // No match or no closing braces found; emit the character as-is
    result.push(bytes[i] as char);
    i += 1;
  }

  result
}

/// Find the position of the first `}}` starting from `start` in the template.
/// Returns the index of the first `}` of the `}}` pair, or None.
fn find_closing_braces(template: &str, start: usize) -> Option<usize> {
  let bytes = template.as_bytes();
  let len = bytes.len();
  let mut i = start;
  while i + 1 < len {
    if bytes[i] == b'}' && bytes[i + 1] == b'}' {
      return Some(i);
    }
    i += 1;
  }
  None
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
}

use crate::types::NotifyFormat;

/// Convert body from source format to target format
pub fn convert_format(body: &str, from: &NotifyFormat, to: &NotifyFormat) -> String {
  if from == to {
    return body.to_string();
  }
  match (from, to) {
    (NotifyFormat::Html, NotifyFormat::Text) => html_to_text(body),
    (NotifyFormat::Markdown, NotifyFormat::Text) => markdown_to_text(body),
    (NotifyFormat::Text, NotifyFormat::Html) => text_to_html(body),
    (NotifyFormat::Markdown, NotifyFormat::Html) => markdown_to_html(body),
    (NotifyFormat::Text, NotifyFormat::Markdown) => body.to_string(),
    (NotifyFormat::Html, NotifyFormat::Markdown) => html_to_text(body),
    _ => body.to_string(),
  }
}

/// Convert HTML to plain text, matching Python apprise's HTMLConverter behavior.
///
/// - Block tags (p, h1-h6, div, td, th, code, pre, label, li) produce newlines
/// - `<br>` produces a newline
/// - `<hr>` produces `\n---\n`
/// - `<li>` prepends `- `
/// - `<blockquote>` prepends ` >`
/// - Ignore tags (form, input, textarea, select, ul, ol, style, link, meta,
///   title, html, head, script) suppress their content
/// - Consecutive whitespace is condensed to a single space
/// - HTML entities are decoded
fn html_to_text(html: &str) -> String {
  const BLOCK_TAGS: &[&str] = &["p", "h1", "h2", "h3", "h4", "h5", "h6", "div", "td", "th", "code", "pre", "label", "li"];
  const IGNORE_TAGS: &[&str] = &["form", "input", "textarea", "select", "ul", "ol", "style", "link", "meta", "title", "html", "head", "script"];

  // Token-based approach: collect text fragments and block-end markers
  enum Token {
    Text(String),
    BlockEnd,
    Newline,
  }

  let mut tokens: Vec<Token> = Vec::new();
  let mut do_store = true;
  let mut pos = 0;
  let bytes = html.as_bytes();
  let len = bytes.len();

  while pos < len {
    if bytes[pos] == b'<' {
      // Parse tag
      let _tag_start = pos;
      pos += 1;
      let is_closing = pos < len && bytes[pos] == b'/';
      if is_closing {
        pos += 1;
      }

      // Extract tag name
      let name_start = pos;
      while pos < len && bytes[pos] != b'>' && bytes[pos] != b' ' && bytes[pos] != b'/' {
        pos += 1;
      }
      let tag_name = std::str::from_utf8(&bytes[name_start..pos]).unwrap_or("").to_ascii_lowercase();

      // Skip to end of tag
      while pos < len && bytes[pos] != b'>' {
        pos += 1;
      }
      if pos < len {
        pos += 1;
      } // skip '>'

      if is_closing {
        // End tag
        do_store = true;
        if BLOCK_TAGS.contains(&tag_name.as_str()) {
          tokens.push(Token::BlockEnd);
        }
      } else {
        // Start tag
        do_store = !IGNORE_TAGS.contains(&tag_name.as_str());

        if BLOCK_TAGS.contains(&tag_name.as_str()) {
          tokens.push(Token::BlockEnd);
        }

        match tag_name.as_str() {
          "li" => tokens.push(Token::Text("- ".to_string())),
          "br" => tokens.push(Token::Newline),
          "hr" => {
            // Trim trailing spaces from previous text
            if let Some(Token::Text(s)) = tokens.last_mut() {
              *s = s.trim_end_matches(' ').to_string();
            }
            tokens.push(Token::Text("\n---\n".to_string()));
          }
          "blockquote" => tokens.push(Token::Text(" >".to_string())),
          _ => {}
        }
      }
    } else {
      // Text content
      if do_store {
        let text_start = pos;
        while pos < len && bytes[pos] != b'<' {
          pos += 1;
        }
        let raw = std::str::from_utf8(&bytes[text_start..pos]).unwrap_or("");
        // Condense whitespace (matching Python's WS_TRIM)
        let condensed = condense_whitespace(raw);
        if !condensed.is_empty() {
          tokens.push(Token::Text(condensed));
        }
      } else {
        // Skip content inside ignored tags
        while pos < len && bytes[pos] != b'<' {
          pos += 1;
        }
      }
    }
  }

  // Finalize: combine tokens, collapsing consecutive BlockEnds into single newlines
  // This matches Python's _finalize() method
  let mut result = String::with_capacity(html.len());
  let mut accum: Option<String> = None;

  for token in tokens {
    match token {
      Token::BlockEnd => {
        if let Some(s) = accum.take() {
          result.push_str(s.trim());
          result.push('\n');
        }
        // If accum is already None, consecutive BlockEnd → skip
      }
      Token::Newline => {
        let s = accum.get_or_insert_with(String::new);
        s.push('\n');
      }
      Token::Text(t) => {
        let s = accum.get_or_insert_with(String::new);
        s.push_str(&t);
      }
    }
  }

  if let Some(s) = accum {
    result.push_str(s.trim());
  }

  // Decode HTML entities
  decode_entities(&result).trim().to_string()
}

/// Condense runs of whitespace into a single space
fn condense_whitespace(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut last_ws = false;
  for ch in s.chars() {
    if ch.is_whitespace() {
      if !last_ws {
        out.push(' ');
        last_ws = true;
      }
    } else {
      out.push(ch);
      last_ws = false;
    }
  }
  out
}

/// Decode common HTML entities
fn decode_entities(s: &str) -> String {
  s.replace("&amp;", "&")
    .replace("&lt;", "<")
    .replace("&gt;", ">")
    .replace("&quot;", "\"")
    .replace("&#39;", "'")
    .replace("&nbsp;", " ")
    .replace("&#x27;", "'")
    .replace("&#x2F;", "/")
    .replace("&apos;", "'")
}

/// Replace `open … close` delimiters, wrapping content with `before`/`after`.
/// If no closing delimiter is found the opening delimiter is emitted literally.
fn replace_delimited(s: &str, open: &str, close: &str, before: &str, after: &str) -> String {
  let mut out = String::with_capacity(s.len() + 32);
  let mut rest = s;
  while let Some(start) = rest.find(open) {
    out.push_str(&rest[..start]);
    let inner = &rest[start + open.len()..];
    if let Some(end) = inner.find(close) {
      out.push_str(before);
      out.push_str(&inner[..end]);
      out.push_str(after);
      rest = &inner[end + close.len()..];
    } else {
      out.push_str(open);
      rest = inner;
    }
  }
  out.push_str(rest);
  out
}

fn strip_links(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut rest = s;
  while let Some(ob) = rest.find('[') {
    let after_bracket = &rest[ob + 1..];
    if let Some(cb) = after_bracket.find(']') {
      let text = &after_bracket[..cb];
      let after_cb = &after_bracket[cb + 1..];
      if let Some(after_paren) = after_cb.strip_prefix('(') {
        if let Some(cp) = after_paren.find(')') {
          out.push_str(&rest[..ob]);
          out.push_str(text);
          rest = &after_paren[cp + 1..];
          continue;
        }
      }
    }
    out.push_str(&rest[..ob + 1]);
    rest = &rest[ob + 1..];
  }
  out.push_str(rest);
  out
}

fn markdown_to_text(md: &str) -> String {
  let s = replace_delimited(md, "**", "**", "", "");
  let s = replace_delimited(&s, "*", "*", "", "");
  let s = replace_delimited(&s, "`", "`", "", "");
  let s = strip_links(&s);
  s.lines().map(|line| line.trim_start_matches('#').trim_start()).collect::<Vec<_>>().join("\n")
}

fn text_to_html(text: &str) -> String {
  text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br/>")
}

fn markdown_to_html(md: &str) -> String {
  let s = replace_delimited(md, "**", "**", "<strong>", "</strong>");
  let s = replace_delimited(&s, "*", "*", "<em>", "</em>");
  let s = replace_delimited(&s, "`", "`", "<code>", "</code>");
  let s = replace_links_html(&s);
  s.replace('\n', "<br/>")
}

/// Replace `[text](url)` → `<a href="url">text</a>`
fn replace_links_html(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 64);
  let mut rest = s;
  while let Some(ob) = rest.find('[') {
    let after_bracket = &rest[ob + 1..];
    if let Some(cb) = after_bracket.find(']') {
      let text = &after_bracket[..cb];
      let after_cb = &after_bracket[cb + 1..];
      if let Some(after_paren) = after_cb.strip_prefix('(') {
        if let Some(cp) = after_paren.find(')') {
          let url = &after_paren[..cp];
          out.push_str(&rest[..ob]);
          out.push_str("<a href=\"");
          out.push_str(url);
          out.push_str("\">");
          out.push_str(text);
          out.push_str("</a>");
          rest = &after_paren[cp + 1..];
          continue;
        }
      }
    }
    out.push_str(&rest[..ob + 1]);
    rest = &rest[ob + 1..];
  }
  out.push_str(rest);
  out
}

/// Intelligently split a message body into chunks of at most `max_len` characters.
///
/// Split priority:
/// 1. Last newline before the limit
/// 2. Last space/tab before the limit
/// 3. Last punctuation followed by whitespace before the limit
/// 4. Hard split at the character limit
pub fn smart_split(text: &str, max_len: usize) -> Vec<String> {
  if max_len == 0 || text.len() <= max_len {
    return vec![text.to_string()];
  }

  let mut chunks = Vec::new();
  let mut remaining = text;

  while !remaining.is_empty() {
    if remaining.len() <= max_len {
      chunks.push(remaining.to_string());
      break;
    }

    let window = &remaining[..max_len];

    // 1. Try to split at the last newline
    let split_pos = if let Some(pos) = window.rfind('\n') {
      pos + 1 // include the newline in the current chunk
    }
    // 2. Try to split at the last space or tab
    else if let Some(pos) = window.rfind([' ', '\t']) {
      pos + 1 // split after the whitespace
    }
    // 3. Try to split at punctuation followed by whitespace
    else {
      let mut punct_pos = None;
      let chars: Vec<char> = window.chars().collect();
      for i in (0..chars.len().saturating_sub(1)).rev() {
        if matches!(chars[i], '.' | ',' | ';' | ':' | '!' | '?') && chars[i + 1].is_whitespace() {
          // byte offset after the punctuation
          let byte_offset: usize = chars[..=i].iter().map(|c| c.len_utf8()).sum();
          punct_pos = Some(byte_offset);
          break;
        }
      }
      // 4. Fall back to hard split
      punct_pos.unwrap_or(max_len)
    };

    // Avoid zero-length splits (shouldn't happen, but be safe)
    let split_pos = if split_pos == 0 { max_len } else { split_pos };

    chunks.push(remaining[..split_pos].to_string());
    remaining = &remaining[split_pos..];
  }

  chunks
}

/// Truncate text to max_len characters (appending "..." if needed)
pub fn truncate(text: &str, max_len: usize) -> String {
  if text.len() <= max_len {
    text.to_string()
  } else {
    let truncated = &text[..max_len.saturating_sub(3)];
    format!("{}...", truncated)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::NotifyFormat;

  // ── convert_format ─────────────────────────────────────────────

  #[test]
  fn test_convert_same_format() {
    let body = "Hello world";
    assert_eq!(convert_format(body, &NotifyFormat::Text, &NotifyFormat::Text), body);
    assert_eq!(convert_format(body, &NotifyFormat::Html, &NotifyFormat::Html), body);
    assert_eq!(convert_format(body, &NotifyFormat::Markdown, &NotifyFormat::Markdown), body);
  }

  #[test]
  fn test_convert_text_to_html() {
    assert_eq!(convert_format("Hello\nWorld", &NotifyFormat::Text, &NotifyFormat::Html), "Hello<br/>World");
  }

  #[test]
  fn test_convert_text_to_html_escaping() {
    assert_eq!(convert_format("<b>bold</b> & 'stuff'", &NotifyFormat::Text, &NotifyFormat::Html), "&lt;b&gt;bold&lt;/b&gt; &amp; 'stuff'");
  }

  #[test]
  fn test_convert_html_to_text() {
    assert_eq!(convert_format("<b>Hello</b> <i>World</i>", &NotifyFormat::Html, &NotifyFormat::Text), "Hello World");
  }

  #[test]
  fn test_convert_html_to_text_entities() {
    // Each entity is separated by a space in the input
    let result = convert_format("&amp; &lt; &gt; &quot; &#39; &nbsp;", &NotifyFormat::Html, &NotifyFormat::Text);
    assert!(result.contains("&"));
    assert!(result.contains("<"));
    assert!(result.contains(">"));
    assert!(result.contains("\""));
    assert!(result.contains("'"));
  }

  #[test]
  fn test_convert_markdown_to_html_bold() {
    let result = convert_format("**bold**", &NotifyFormat::Markdown, &NotifyFormat::Html);
    assert_eq!(result, "<strong>bold</strong>");
  }

  #[test]
  fn test_convert_markdown_to_html_italic() {
    let result = convert_format("*italic*", &NotifyFormat::Markdown, &NotifyFormat::Html);
    assert_eq!(result, "<em>italic</em>");
  }

  #[test]
  fn test_convert_markdown_to_html_code() {
    let result = convert_format("`code`", &NotifyFormat::Markdown, &NotifyFormat::Html);
    assert_eq!(result, "<code>code</code>");
  }

  #[test]
  fn test_convert_markdown_to_html_link() {
    let result = convert_format("[click](http://example.com)", &NotifyFormat::Markdown, &NotifyFormat::Html);
    assert_eq!(result, "<a href=\"http://example.com\">click</a>");
  }

  #[test]
  fn test_convert_markdown_to_html_newlines() {
    let result = convert_format("line1\nline2", &NotifyFormat::Markdown, &NotifyFormat::Html);
    assert_eq!(result, "line1<br/>line2");
  }

  #[test]
  fn test_convert_markdown_to_text_strips_bold() {
    assert_eq!(convert_format("**bold** text", &NotifyFormat::Markdown, &NotifyFormat::Text), "bold text");
  }

  #[test]
  fn test_convert_markdown_to_text_strips_italic() {
    assert_eq!(convert_format("*italic* text", &NotifyFormat::Markdown, &NotifyFormat::Text), "italic text");
  }

  #[test]
  fn test_convert_markdown_to_text_strips_code() {
    assert_eq!(convert_format("`code` text", &NotifyFormat::Markdown, &NotifyFormat::Text), "code text");
  }

  #[test]
  fn test_convert_markdown_to_text_strips_links() {
    assert_eq!(convert_format("[click](http://example.com)", &NotifyFormat::Markdown, &NotifyFormat::Text), "click");
  }

  #[test]
  fn test_convert_markdown_to_text_strips_headers() {
    assert_eq!(convert_format("## Header", &NotifyFormat::Markdown, &NotifyFormat::Text), "Header");
  }

  #[test]
  fn test_convert_text_to_markdown_passthrough() {
    let body = "plain text body";
    assert_eq!(convert_format(body, &NotifyFormat::Text, &NotifyFormat::Markdown), body);
  }

  // ── html_to_text ───────────────────────────────────────────────

  #[test]
  fn test_html_to_text_nested_tags() {
    assert_eq!(html_to_text("<div><p>Hello</p></div>"), "Hello");
  }

  #[test]
  fn test_html_to_text_empty() {
    assert_eq!(html_to_text(""), "");
  }

  #[test]
  fn test_html_to_text_no_tags() {
    assert_eq!(html_to_text("plain text"), "plain text");
  }

  #[test]
  fn test_html_to_text_all_entities() {
    // Test individual entities (result is trimmed, so wrap in text)
    assert_eq!(html_to_text("a&amp;b"), "a&b");
    assert_eq!(html_to_text("a&lt;b"), "a<b");
    assert_eq!(html_to_text("a&gt;b"), "a>b");
    assert_eq!(html_to_text("a&quot;b"), "a\"b");
    assert_eq!(html_to_text("a&#39;b"), "a'b");
    assert_eq!(html_to_text("a&nbsp;b"), "a b");
  }

  #[test]
  fn test_html_to_text_br_tag() {
    assert_eq!(html_to_text("Hello<br>World"), "Hello\nWorld");
    assert_eq!(html_to_text("Hello<br/>World"), "Hello\nWorld");
  }

  #[test]
  fn test_html_to_text_p_tags() {
    assert_eq!(html_to_text("<p>First</p><p>Second</p>"), "First\nSecond");
  }

  #[test]
  fn test_html_to_text_list_items() {
    assert_eq!(html_to_text("<ul><li>One</li><li>Two</li></ul>"), "- One\n- Two");
  }

  #[test]
  fn test_html_to_text_hr() {
    assert_eq!(html_to_text("Above<hr>Below"), "Above\n---\nBelow");
  }

  #[test]
  fn test_html_to_text_blockquote() {
    let result = html_to_text("<blockquote>Quote</blockquote>");
    assert!(result.contains(">"));
    assert!(result.contains("Quote"));
  }

  #[test]
  fn test_html_to_text_ignores_script() {
    assert_eq!(html_to_text("Hello<script>alert('x')</script>World"), "HelloWorld");
  }

  #[test]
  fn test_html_to_text_ignores_style() {
    assert_eq!(html_to_text("Hello<style>.x{color:red}</style>World"), "HelloWorld");
  }

  #[test]
  fn test_html_to_text_heading() {
    assert_eq!(html_to_text("<h1>Title</h1><p>Body</p>"), "Title\nBody");
  }

  #[test]
  fn test_html_to_text_whitespace_condensing() {
    assert_eq!(html_to_text("Hello   \n  World"), "Hello World");
  }

  // ── text_to_html ───────────────────────────────────────────────

  #[test]
  fn test_text_to_html_special_chars() {
    assert_eq!(text_to_html("a & b < c > d"), "a &amp; b &lt; c &gt; d");
  }

  #[test]
  fn test_text_to_html_newlines() {
    assert_eq!(text_to_html("line1\nline2\nline3"), "line1<br/>line2<br/>line3");
  }

  #[test]
  fn test_text_to_html_empty() {
    assert_eq!(text_to_html(""), "");
  }

  // ── truncate ───────────────────────────────────────────────────

  #[test]
  fn test_truncate_short_string() {
    assert_eq!(truncate("hello", 10), "hello");
  }

  #[test]
  fn test_truncate_exact_length() {
    assert_eq!(truncate("hello", 5), "hello");
  }

  #[test]
  fn test_truncate_long_string() {
    assert_eq!(truncate("hello world!", 8), "hello...");
  }

  #[test]
  fn test_truncate_empty() {
    assert_eq!(truncate("", 10), "");
  }

  #[test]
  fn test_truncate_very_small_max() {
    // With max_len=3, we get 0 chars + "..."
    let result = truncate("abcdef", 3);
    assert_eq!(result, "...");
  }

  // ── smart_split ───────────────────────────────────────────────

  #[test]
  fn test_smart_split_short_message() {
    let result = smart_split("hello", 100);
    assert_eq!(result, vec!["hello"]);
  }

  #[test]
  fn test_smart_split_exact_length() {
    let result = smart_split("hello", 5);
    assert_eq!(result, vec!["hello"]);
  }

  #[test]
  fn test_smart_split_at_newline() {
    let result = smart_split("hello\nworld!", 8);
    assert_eq!(result, vec!["hello\n", "world!"]);
  }

  #[test]
  fn test_smart_split_at_space() {
    let result = smart_split("hello world!", 8);
    assert_eq!(result, vec!["hello ", "world!"]);
  }

  #[test]
  fn test_smart_split_hard_split() {
    let result = smart_split("abcdefghij", 5);
    assert_eq!(result, vec!["abcde", "fghij"]);
  }

  #[test]
  fn test_smart_split_multiple_chunks() {
    let result = smart_split("aaaa bbbb cccc dddd", 5);
    assert_eq!(result, vec!["aaaa ", "bbbb ", "cccc ", "dddd"]);
  }

  #[test]
  fn test_smart_split_zero_max() {
    let result = smart_split("hello", 0);
    assert_eq!(result, vec!["hello"]);
  }

  #[test]
  fn test_smart_split_empty() {
    let result = smart_split("", 10);
    assert_eq!(result, vec![""]);
  }

  #[test]
  fn test_smart_split_prefers_newline_over_space() {
    // Both a newline and a space exist before limit; newline should win
    let result = smart_split("ab cd\nef gh", 7);
    assert_eq!(result, vec!["ab cd\n", "ef gh"]);
  }
}

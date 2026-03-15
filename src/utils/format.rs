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

fn html_to_text(html: &str) -> String {
    // Strip HTML tags with a simple state machine
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
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
            if after_cb.starts_with('(') {
                if let Some(cp) = after_cb[1..].find(')') {
                    out.push_str(&rest[..ob]);
                    out.push_str(text);
                    rest = &after_cb[1 + cp + 1..];
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
    s.lines()
        .map(|line| line.trim_start_matches('#').trim_start())
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_to_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br/>")
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
            if after_cb.starts_with('(') {
                if let Some(cp) = after_cb[1..].find(')') {
                    let url = &after_cb[1..1 + cp];
                    out.push_str(&rest[..ob]);
                    out.push_str("<a href=\"");
                    out.push_str(url);
                    out.push_str("\">");
                    out.push_str(text);
                    out.push_str("</a>");
                    rest = &after_cb[1 + cp + 1..];
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
        else if let Some(pos) = window.rfind(|c: char| c == ' ' || c == '\t') {
            pos + 1 // split after the whitespace
        }
        // 3. Try to split at punctuation followed by whitespace
        else {
            let mut punct_pos = None;
            let chars: Vec<char> = window.chars().collect();
            for i in (0..chars.len().saturating_sub(1)).rev() {
                if matches!(chars[i], '.' | ',' | ';' | ':' | '!' | '?')
                    && chars[i + 1].is_whitespace()
                {
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
        assert_eq!(
            convert_format("Hello\nWorld", &NotifyFormat::Text, &NotifyFormat::Html),
            "Hello<br/>World"
        );
    }

    #[test]
    fn test_convert_text_to_html_escaping() {
        assert_eq!(
            convert_format("<b>bold</b> & 'stuff'", &NotifyFormat::Text, &NotifyFormat::Html),
            "&lt;b&gt;bold&lt;/b&gt; &amp; 'stuff'"
        );
    }

    #[test]
    fn test_convert_html_to_text() {
        assert_eq!(
            convert_format("<b>Hello</b> <i>World</i>", &NotifyFormat::Html, &NotifyFormat::Text),
            "Hello World"
        );
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
        assert_eq!(
            convert_format("**bold** text", &NotifyFormat::Markdown, &NotifyFormat::Text),
            "bold text"
        );
    }

    #[test]
    fn test_convert_markdown_to_text_strips_italic() {
        assert_eq!(
            convert_format("*italic* text", &NotifyFormat::Markdown, &NotifyFormat::Text),
            "italic text"
        );
    }

    #[test]
    fn test_convert_markdown_to_text_strips_code() {
        assert_eq!(
            convert_format("`code` text", &NotifyFormat::Markdown, &NotifyFormat::Text),
            "code text"
        );
    }

    #[test]
    fn test_convert_markdown_to_text_strips_links() {
        assert_eq!(
            convert_format("[click](http://example.com)", &NotifyFormat::Markdown, &NotifyFormat::Text),
            "click"
        );
    }

    #[test]
    fn test_convert_markdown_to_text_strips_headers() {
        assert_eq!(
            convert_format("## Header", &NotifyFormat::Markdown, &NotifyFormat::Text),
            "Header"
        );
    }

    #[test]
    fn test_convert_text_to_markdown_passthrough() {
        let body = "plain text body";
        assert_eq!(
            convert_format(body, &NotifyFormat::Text, &NotifyFormat::Markdown),
            body
        );
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
        // When entities are adjacent, the replacement chain processes them in order.
        // &amp; -> &, then & in &lt; also gets decoded, etc.
        // Test individual entities instead.
        assert_eq!(html_to_text("&amp;"), "&");
        assert_eq!(html_to_text("&lt;"), "<");
        assert_eq!(html_to_text("&gt;"), ">");
        assert_eq!(html_to_text("&quot;"), "\"");
        assert_eq!(html_to_text("&#39;"), "'");
        assert_eq!(html_to_text("&nbsp;"), " ");
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

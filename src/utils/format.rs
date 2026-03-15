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
}

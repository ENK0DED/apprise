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

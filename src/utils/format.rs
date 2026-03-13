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
    // Simple HTML stripping - remove tags and decode entities
    let re_tag = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re_tag.replace_all(html, "");
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
}

fn markdown_to_text(md: &str) -> String {
    // Strip markdown formatting
    let re_bold = regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let re_italic = regex::Regex::new(r"\*(.+?)\*").unwrap();
    let re_code = regex::Regex::new(r"`(.+?)`").unwrap();
    let re_link = regex::Regex::new(r"\[(.+?)\]\(.+?\)").unwrap();
    let re_heading = regex::Regex::new(r"^#{1,6}\s*").unwrap();

    let text = re_bold.replace_all(md, "$1");
    let text = re_italic.replace_all(&text, "$1");
    let text = re_code.replace_all(&text, "$1");
    let text = re_link.replace_all(&text, "$1");
    let text = text
        .lines()
        .map(|line| re_heading.replace(line, "").to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    text
}

fn text_to_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br/>")
}

fn markdown_to_html(md: &str) -> String {
    // Very basic markdown to HTML
    let re_bold = regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let re_italic = regex::Regex::new(r"\*(.+?)\*").unwrap();
    let re_code = regex::Regex::new(r"`(.+?)`").unwrap();
    let re_link = regex::Regex::new(r"\[(.+?)\]\((.+?)\)").unwrap();

    let html = re_bold.replace_all(md, "<strong>$1</strong>");
    let html = re_italic.replace_all(&html, "<em>$1</em>");
    let html = re_code.replace_all(&html, "<code>$1</code>");
    let html = re_link.replace_all(&html, "<a href=\"$2\">$1</a>");
    html.replace('\n', "<br/>").to_owned()
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

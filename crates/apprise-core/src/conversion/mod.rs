//! Body format conversion between Apprise's `text`, `html` and `markdown`
//! formats, plus the CommonMark helpers plugins use to build their own
//! markup dialects (upstream `apprise/conversion/`).
//!
//! - [`convert_between`] converts a whole message between the three formats.
//! - [`html`] holds the HTML parsers ([`html_to_text`], [`html_to_markdown`])
//!   and [`HtmlTagReducer`], which rewrites HTML down to a small tag set.
//! - [`commonmark`] scans and repairs CommonMark so a plugin can translate
//!   it into a service dialect (Slack `mrkdwn`, Telegram MarkdownV2, ...).
//! - [`dialect`] fits a plugin's dialect output into its size limit.
//!
//! Every length in this module is counted in Unicode scalar values
//! (`chars`), as upstream counts Python string characters. Index-based
//! scanners take `&[char]`, so plugin code ported from upstream can keep
//! upstream's index arithmetic unchanged.

pub mod commonmark;
pub mod dialect;
mod entities;
pub mod html;
mod markdown;
pub(crate) mod pyhtml;
pub mod pystr;
mod unicode_tables;
#[cfg(test)]
mod vectors;

pub use commonmark::*;
pub use dialect::{split_dialect_chunk, truncate_dialect_chunk};
pub use html::{
  BLOCKQUOTE_DEPTH_MAX, HtmlTagReducer, LIST_DEPTH_MAX, MAX_FRAME_DEPTH, TagReducer, html_to_markdown, html_to_text, reduce_with, reducer_end,
  reducer_handle_starttag, reducer_line, reducer_start, reducer_text,
};
pub use markdown::{MarkdownExtension, markdown_to_html, markdown_to_html_with};
pub use pyhtml::Attrs;

use crate::types::NotifyFormat;

/// Convert `content` from one Apprise format to another.
///
/// The content is returned unchanged when no converter exists for the pair,
/// as upstream does. Upstream has no Markdown to text converter, so Markdown
/// sent to a text-only service is delivered as written.
pub fn convert_between(from: &NotifyFormat, to: &NotifyFormat, content: &str) -> String {
  match (from, to) {
    (NotifyFormat::Markdown, NotifyFormat::Html) => markdown_to_html(content),
    (NotifyFormat::Text, NotifyFormat::Html) => text_to_html(content),
    (NotifyFormat::Html, NotifyFormat::Text) => html_to_text(content),
    (NotifyFormat::Html, NotifyFormat::Markdown) => html_to_markdown(content),
    (NotifyFormat::Text, NotifyFormat::Markdown) => text_to_markdown(content),
    _ => content.to_string(),
  }
}

/// Convert plain text to HTML: escape markup characters and whitespace, and
/// turn line breaks into `<br/>` (upstream `URLBase.escape_html` with
/// `convert_new_lines=True`).
pub fn text_to_html(content: &str) -> String {
  let mut out = String::with_capacity(content.len() + content.len() / 4);
  for c in content.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '\'' => out.push_str("&apos;"),
      '"' => out.push_str("&quot;"),
      '\t' => out.push_str("&emsp;"),
      ' ' => out.push_str("&nbsp;"),
      '\n' => out.push_str("<br/>"),
      _ => out.push(c),
    }
  }
  out
}

/// Convert plain text to CommonMark by escaping every character CommonMark
/// treats as syntax, including backslashes, so the text renders literally.
/// An `&` is only escaped when it would otherwise read as an entity such as
/// `&amp;`.
pub fn text_to_markdown(content: &str) -> String {
  let chars: Vec<char> = content.chars().collect();
  let mut out = String::with_capacity(content.len() + content.len() / 8);
  for (i, &c) in chars.iter().enumerate() {
    let escape = match c {
      '\\' | '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '=' | '|' | '{' | '}' | '.' | '!' | '-' => true,
      '&' => looks_like_entity(&chars[i + 1..]),
      _ => false,
    };
    if escape {
      out.push('\\');
    }
    out.push(c);
  }
  out
}

// `(?=#?[0-9A-Za-z]{1,32};)`
fn looks_like_entity(rest: &[char]) -> bool {
  let rest = if rest.first() == Some(&'#') { &rest[1..] } else { rest };
  let run = rest.iter().take(33).take_while(|c| c.is_ascii_alphanumeric()).count();
  (1..=32).contains(&run) && rest.get(run) == Some(&';')
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_text_to_html() {
    assert_eq!(text_to_html("a <b> & 'c' \"d\"\n\te"), "a&nbsp;&lt;b&gt;&nbsp;&amp;&nbsp;&apos;c&apos;&nbsp;&quot;d&quot;<br/>&emsp;e");
    assert_eq!(text_to_html(""), "");
  }

  #[test]
  fn test_text_to_markdown() {
    assert_eq!(text_to_markdown("a_b *c* 1. [x](y) &amp; & a&b;"), "a\\_b \\*c\\* 1\\. \\[x\\]\\(y\\) \\&amp; & a\\&b;");
  }

  #[test]
  fn test_unknown_pairs_are_unchanged() {
    assert_eq!(convert_between(&NotifyFormat::Markdown, &NotifyFormat::Text, "**x**"), "**x**");
    assert_eq!(convert_between(&NotifyFormat::Text, &NotifyFormat::Text, "<b>"), "<b>");
  }
}

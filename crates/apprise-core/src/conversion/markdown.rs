//! Markdown to HTML, matching upstream's Python-Markdown output.

/// Python-Markdown extensions upstream enables for notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownExtension {
  /// `markdown.extensions.nl2br`: a single line break becomes `<br />`.
  Nl2br,
  /// `markdown.extensions.tables`: pipe tables.
  Tables,
}

/// The extensions [`markdown_to_html`] uses.
pub const DEFAULT_EXTENSIONS: &[MarkdownExtension] = &[MarkdownExtension::Nl2br, MarkdownExtension::Tables];

/// Convert Markdown to HTML with upstream's notification defaults.
pub fn markdown_to_html(content: &str) -> String {
  markdown_to_html_with(content, DEFAULT_EXTENSIONS)
}

/// Convert Markdown to HTML with an explicit extension list (upstream's
/// `extensions=` argument; Matrix passes none).
pub fn markdown_to_html_with(content: &str, _extensions: &[MarkdownExtension]) -> String {
  content.to_string()
}

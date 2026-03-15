use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Parse text config format (one URL per line, # and ; for comments)
/// Supports `include <source>` directives to recursively load other configs.
/// Supports `tag1,tag2=URL` syntax to assign tags to URLs.
pub async fn parse_text(content: &str, recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let mut services: Vec<Box<dyn Notify>> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        // Strip inline comments
        let trimmed = trimmed
            .find(" #")
            .or_else(|| trimmed.find(" ;"))
            .map(|i| trimmed[..i].trim())
            .unwrap_or(trimmed);

        // Check for include directive
        if let Some(source) = trimmed.strip_prefix("include").or_else(|| trimmed.strip_prefix("Include")) {
            let source = source.trim();
            if !source.is_empty() {
                match super::load_config(source, recursion_depth - 1).await {
                    Ok((mut included, _)) => services.append(&mut included),
                    Err(e) => tracing::warn!("Failed to load included config '{}': {}", source, e),
                }
            }
            continue;
        }

        // Check for tag=URL syntax: left side has no "://" and there's an "="
        let (tags, url_str) = if let Some(eq_pos) = trimmed.find('=') {
            let left = &trimmed[..eq_pos];
            let right = &trimmed[eq_pos + 1..];
            // Only treat as tags if left doesn't contain :// (i.e., it's not a URL)
            if !left.contains("://") && right.contains("://") {
                (Some(left.to_string()), right.trim().to_string())
            } else {
                (None, trimmed.to_string())
            }
        } else {
            (None, trimmed.to_string())
        };

        // If no URL found, skip
        if !url_str.contains("://") {
            continue;
        }

        // Append tags to URL as query param so from_url() picks them up
        let final_url = if let Some(ref tags) = tags {
            let separator = if url_str.contains('?') { "&" } else { "?" };
            format!("{}{}tag={}", url_str, separator, urlencoding::encode(tags))
        } else {
            url_str
        };

        if let Some(svc) = from_url(&final_url) {
            services.push(svc);
        }
    }

    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_one_url_per_line() {
        // json:// and xml:// should be recognized by the registry
        let content = "json://localhost\nxml://localhost";
        let services = parse_text(content, 1).await.unwrap();
        // Both json and xml are registered plugins
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_parse_comments_hash() {
        let content = "# This is a comment\njson://localhost";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_comments_semicolon() {
        let content = "; This is a comment\njson://localhost";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_empty_lines() {
        let content = "\n\njson://localhost\n\n\nxml://localhost\n\n";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_parse_inline_hash_comment() {
        let content = "json://localhost # my service";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_inline_semicolon_comment() {
        let content = "json://localhost ; my service";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_tag_equals_url() {
        let content = "mytag=json://localhost";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_multiple_tags_equals_url() {
        let content = "tag1,tag2=json://localhost";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_empty_content() {
        let content = "";
        let services = parse_text(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_parse_only_comments() {
        let content = "# comment 1\n; comment 2\n# comment 3";
        let services = parse_text(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_parse_non_url_lines_skipped() {
        let content = "not a url\njson://localhost\nalso not a url";
        let services = parse_text(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_unknown_schema_skipped() {
        let content = "totallyunknownschema://whatever";
        let services = parse_text(content, 1).await.unwrap();
        assert!(services.is_empty());
    }
}

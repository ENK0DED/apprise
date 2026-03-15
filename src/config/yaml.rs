use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Parse YAML config format.
/// Supports `include:` key to recursively load other configs.
/// Supports `tag:` / `tags:` fields per URL entry to assign tags.
pub async fn parse_yaml(content: &str, recursion_depth: u32) -> Result<Vec<Box<dyn Notify>>, ConfigError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| ConfigError::Other(e.to_string()))?;
    let mut services: Vec<Box<dyn Notify>> = Vec::new();

    // Handle "urls:" key
    if let Some(urls) = doc.get("urls") {
        if let Some(url_list) = urls.as_sequence() {
            for item in url_list {
                let (url_str, tags) = if let Some(s) = item.as_str() {
                    (Some(s.to_string()), None)
                } else if let Some(m) = item.as_mapping() {
                    // First key of the mapping is the URL
                    let url = m.keys().next().and_then(|k| k.as_str()).map(|s| s.to_string());
                    // Extract tags from the mapping value
                    let tag_str = if let Some(url_key) = m.keys().next() {
                        if let Some(inner) = m.get(url_key).and_then(|v| v.as_mapping()) {
                            // Check for "tag:" or "tags:" in the inner mapping
                            inner.get(&serde_yaml::Value::String("tag".to_string()))
                                .or_else(|| inner.get(&serde_yaml::Value::String("tags".to_string())))
                                .and_then(|v| {
                                    if let Some(s) = v.as_str() {
                                        Some(s.to_string())
                                    } else if let Some(seq) = v.as_sequence() {
                                        let tags: Vec<String> = seq.iter()
                                            .filter_map(|t| t.as_str().map(|s| s.to_string()))
                                            .collect();
                                        if tags.is_empty() { None } else { Some(tags.join(",")) }
                                    } else {
                                        None
                                    }
                                })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    (url, tag_str)
                } else {
                    (None, None)
                };

                if let Some(url) = url_str {
                    // Append tags to URL as query param
                    let final_url = if let Some(ref tag_val) = tags {
                        let sep = if url.contains('?') { "&" } else { "?" };
                        format!("{}{}tag={}", url, sep, urlencoding::encode(tag_val))
                    } else {
                        url
                    };
                    if let Some(svc) = from_url(&final_url) {
                        services.push(svc);
                    }
                }
            }
        }
    }

    // Handle "include:" key for recursive config loading
    if let Some(includes) = doc.get("include") {
        let sources: Vec<String> = if let Some(s) = includes.as_str() {
            vec![s.to_string()]
        } else if let Some(seq) = includes.as_sequence() {
            seq.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
        } else {
            vec![]
        };
        for source in sources {
            match super::load_config(&source, recursion_depth - 1).await {
                Ok(mut included) => services.append(&mut included),
                Err(e) => tracing::warn!("Failed to load included config '{}': {}", source, e),
            }
        }
    }

    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_yaml_urls_string_list() {
        let content = r#"
urls:
  - json://localhost
  - xml://localhost
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_yaml_urls_single() {
        let content = r#"
urls:
  - json://localhost
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_urls_with_tag_string() {
        let content = r#"
urls:
  - json://localhost:
      tag: my_tag
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_urls_with_tags_string() {
        let content = r#"
urls:
  - json://localhost:
      tags: tag1,tag2
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_urls_with_tag_list() {
        let content = r#"
urls:
  - json://localhost:
      tag:
        - tag_a
        - tag_b
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_empty_urls() {
        let content = r#"
urls: []
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_yaml_no_urls_key() {
        let content = r#"
version: 1
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_yaml_unknown_schema_skipped() {
        let content = r#"
urls:
  - totallyunknownschema://whatever
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_yaml_mixed_string_and_mapping() {
        let content = r#"
urls:
  - json://localhost
  - xml://localhost:
      tag: special
"#;
        let services = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_yaml_invalid_yaml() {
        let content = "{{{{invalid yaml";
        let result = parse_yaml(content, 1).await;
        assert!(result.is_err());
    }
}

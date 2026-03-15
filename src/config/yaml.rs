use std::collections::{HashMap, HashSet};
use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::from_url;

/// Parse a `tag:` or `tags:` YAML value into a comma-separated string.
fn parse_tag_value(val: &serde_yaml::Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        let s = s.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    } else if let Some(seq) = val.as_sequence() {
        let tags: Vec<String> = seq.iter()
            .filter_map(|t| t.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        if tags.is_empty() { None } else { Some(tags.join(",")) }
    } else {
        None
    }
}

/// Read the top-level `tag:` or `tags:` key — these are appended to ALL URLs.
fn parse_global_tags(doc: &serde_yaml::Value) -> Option<String> {
    doc.get("tag")
        .or_else(|| doc.get("tags"))
        .and_then(parse_tag_value)
}

/// Parse the `groups:` section.  Each key maps a group name to a list of tags
/// (comma-separated string or YAML sequence).
fn parse_groups(doc: &serde_yaml::Value) -> HashMap<String, Vec<String>> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(g) = doc.get("groups").and_then(|v| v.as_mapping()) {
        for (k, v) in g {
            if let Some(name) = k.as_str() {
                let members = if let Some(s) = v.as_str() {
                    s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
                } else if let Some(seq) = v.as_sequence() {
                    seq.iter()
                        .filter_map(|t| t.as_str().map(|s| s.trim().to_string()))
                        .filter(|t| !t.is_empty())
                        .collect()
                } else {
                    Vec::new()
                };
                groups.insert(name.to_string(), members);
            }
        }
    }
    groups
}

/// Resolve tag groups transitively: if a tag matches a group member, add the
/// group name; repeat until stable.
fn resolve_groups(tags: &[String], groups: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut result: HashSet<String> = tags.iter().cloned().collect();
    loop {
        let mut changed = false;
        for (group_name, members) in groups {
            if result.contains(group_name) {
                continue; // already have this group
            }
            if members.iter().any(|m| result.contains(m)) {
                result.insert(group_name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut v: Vec<String> = result.into_iter().collect();
    v.sort();
    v
}

/// Parse the `asset:` section and build an `AppriseAsset`.
fn parse_asset_section(doc: &serde_yaml::Value) -> Option<AppriseAsset> {
    let a = doc.get("asset").and_then(|v| v.as_mapping())?;
    let mut asset = AppriseAsset::default();

    let str_key = |k: &str| serde_yaml::Value::String(k.into());

    if let Some(v) = a.get(&str_key("app_id")).and_then(|v| v.as_str()) {
        asset.app_id = v.to_string();
    }
    if let Some(v) = a.get(&str_key("app_desc")).and_then(|v| v.as_str()) {
        asset.app_desc = v.to_string();
    }
    if let Some(v) = a.get(&str_key("app_url")).and_then(|v| v.as_str()) {
        asset.app_url = v.to_string();
    }
    if let Some(v) = a.get(&str_key("image_url_mask")).and_then(|v| v.as_str()) {
        asset.image_url_mask = Some(v.to_string());
    }
    if let Some(v) = a.get(&str_key("image_url_logo")).and_then(|v| v.as_str()) {
        asset.image_url_logo = Some(v.to_string());
    }
    if let Some(v) = a.get(&str_key("theme")).and_then(|v| v.as_str()) {
        asset.theme = v.to_string();
    }
    if let Some(v) = a.get(&str_key("body_format")).and_then(|v| v.as_str()) {
        asset.body_format = Some(v.to_string());
    }
    if let Some(v) = a.get(&str_key("secure_logging")) {
        if let Some(b) = v.as_bool() {
            asset.secure_logging = b;
        } else if let Some(s) = v.as_str() {
            asset.secure_logging = matches!(s.to_lowercase().as_str(), "yes" | "true" | "1");
        }
    }

    Some(asset)
}

/// Append tags to a URL as a query parameter.
fn append_tags_to_url(url: &str, tags: &str) -> String {
    if tags.is_empty() {
        return url.to_string();
    }
    let sep = if url.contains('?') { "&" } else { "?" };
    format!("{}{}tag={}", url, sep, urlencoding::encode(tags))
}

/// Extract all key-value overrides from a mapping (excluding `tag`/`tags`)
/// and append them as query parameters.
fn append_overrides_to_url(url: &str, mapping: &serde_yaml::Mapping) -> String {
    let mut result = url.to_string();
    for (k, v) in mapping {
        if let Some(key) = k.as_str() {
            if key == "tag" || key == "tags" {
                continue;
            }
            let val = if let Some(s) = v.as_str() {
                s.to_string()
            } else if let Some(b) = v.as_bool() {
                b.to_string()
            } else if let Some(i) = v.as_i64() {
                i.to_string()
            } else if let Some(f) = v.as_f64() {
                f.to_string()
            } else {
                continue;
            };
            let sep = if result.contains('?') { "&" } else { "?" };
            result = format!("{}{}{}={}", result, sep, urlencoding::encode(key), urlencoding::encode(&val));
        }
    }
    result
}

/// Parse YAML config format.
///
/// Supports:
/// - `urls:` key with string or mapping entries
/// - `tag:` / `tags:` top-level global tags (appended to all URLs)
/// - `groups:` tag groups with transitive resolution
/// - Per-URL overrides via sequence values (multiple instances)
/// - `asset:` section (parsed and returned)
/// - `include:` key to recursively load other configs
pub async fn parse_yaml(content: &str, recursion_depth: u32) -> Result<(Vec<Box<dyn Notify>>, Option<AppriseAsset>), ConfigError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| ConfigError::Other(e.to_string()))?;
    let mut services: Vec<Box<dyn Notify>> = Vec::new();

    // A) Global tags
    let global_tags = parse_global_tags(&doc);

    // B) Tag groups
    let groups = parse_groups(&doc);

    // D) Asset section
    let asset = parse_asset_section(&doc);

    // Handle "urls:" key
    if let Some(urls) = doc.get("urls") {
        if let Some(url_list) = urls.as_sequence() {
            for item in url_list {
                // Each item can be:
                // 1. A plain string: "json://localhost"
                // 2. A mapping with a single URL key whose value is:
                //    a. A mapping of overrides (single instance)
                //    b. A sequence of mappings (multiple instances, Task 1C)
                //    c. null (just the URL)

                if let Some(s) = item.as_str() {
                    // Plain string URL
                    let mut all_tags: Vec<String> = Vec::new();
                    if let Some(ref gt) = global_tags {
                        all_tags.extend(gt.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
                    }
                    let resolved = resolve_groups(&all_tags, &groups);
                    let tag_str = resolved.join(",");
                    let final_url = append_tags_to_url(s, &tag_str);
                    if let Some(svc) = from_url(&final_url) {
                        services.push(svc);
                    }
                } else if let Some(m) = item.as_mapping() {
                    // Mapping: first key is the URL
                    let url_key = match m.keys().next() {
                        Some(k) => k,
                        None => continue,
                    };
                    let url_str = match url_key.as_str() {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    let value = m.get(url_key);

                    // Determine instances
                    let instances: Vec<Option<&serde_yaml::Mapping>> = if let Some(val) = value {
                        if let Some(seq) = val.as_sequence() {
                            // C) Per-URL overrides: each item in the sequence creates a separate instance
                            seq.iter()
                                .map(|entry| entry.as_mapping())
                                .collect()
                        } else if let Some(inner_map) = val.as_mapping() {
                            // Single mapping of overrides
                            vec![Some(inner_map)]
                        } else {
                            // null or scalar value
                            vec![None]
                        }
                    } else {
                        vec![None]
                    };

                    for instance in instances {
                        // Collect tags: global + per-URL
                        let mut all_tags: Vec<String> = Vec::new();
                        if let Some(ref gt) = global_tags {
                            all_tags.extend(gt.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
                        }

                        let mut url_with_overrides = url_str.clone();

                        if let Some(inner) = instance {
                            // Extract tag/tags from the override mapping
                            let tag_val = inner.get(&serde_yaml::Value::String("tag".to_string()))
                                .or_else(|| inner.get(&serde_yaml::Value::String("tags".to_string())))
                                .and_then(parse_tag_value);
                            if let Some(tv) = tag_val {
                                all_tags.extend(tv.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
                            }
                            // Append non-tag overrides as query params
                            url_with_overrides = append_overrides_to_url(&url_with_overrides, inner);
                        }

                        // Resolve groups
                        let resolved = resolve_groups(&all_tags, &groups);
                        let tag_str = resolved.join(",");
                        let final_url = append_tags_to_url(&url_with_overrides, &tag_str);

                        if let Some(svc) = from_url(&final_url) {
                            services.push(svc);
                        }
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
                Ok((mut included, _)) => services.append(&mut included),
                Err(e) => tracing::warn!("Failed to load included config '{}': {}", source, e),
            }
        }
    }

    Ok((services, asset))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Basic URL parsing ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_yaml_urls_string_list() {
        let content = r#"
urls:
  - json://localhost
  - xml://localhost
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_yaml_urls_single() {
        let content = r#"
urls:
  - json://localhost
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_urls_with_tag_string() {
        let content = r#"
urls:
  - json://localhost:
      tag: my_tag
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_urls_with_tags_string() {
        let content = r#"
urls:
  - json://localhost:
      tags: tag1,tag2
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
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
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    #[tokio::test]
    async fn test_yaml_empty_urls() {
        let content = r#"
urls: []
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_yaml_no_urls_key() {
        let content = r#"
version: 1
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn test_yaml_unknown_schema_skipped() {
        let content = r#"
urls:
  - totallyunknownschema://whatever
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
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
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
    }

    #[tokio::test]
    async fn test_yaml_invalid_yaml() {
        let content = "{{{{invalid yaml";
        let result = parse_yaml(content, 1).await;
        assert!(result.is_err());
    }

    // ─── A) Global tags ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_yaml_global_tag_string() {
        let content = r#"
tag: admin, devops
urls:
  - json://localhost
  - xml://localhost
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2);
        // Both services should have the global tags
        for svc in &services {
            let tags = svc.tags();
            assert!(tags.iter().any(|t| t == "admin"), "expected admin tag, got {:?}", tags);
            assert!(tags.iter().any(|t| t == "devops"), "expected devops tag, got {:?}", tags);
        }
    }

    #[tokio::test]
    async fn test_yaml_global_tags_list() {
        let content = r#"
tags:
  - admin
  - devops
urls:
  - json://localhost
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        let tags = services[0].tags();
        assert!(tags.iter().any(|t| t == "admin"));
        assert!(tags.iter().any(|t| t == "devops"));
    }

    #[tokio::test]
    async fn test_yaml_global_tags_merge_with_per_url() {
        let content = r#"
tag: global
urls:
  - json://localhost:
      tag: local
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        let tags = services[0].tags();
        assert!(tags.iter().any(|t| t == "global"), "expected global tag, got {:?}", tags);
        assert!(tags.iter().any(|t| t == "local"), "expected local tag, got {:?}", tags);
    }

    // ─── B) Tag groups ──────────────────────────────────────────────────

    #[test]
    fn test_resolve_groups_simple() {
        let mut groups = HashMap::new();
        groups.insert("admins".to_string(), vec!["tagA".to_string(), "tagB".to_string()]);
        let result = resolve_groups(&["tagA".to_string()], &groups);
        assert!(result.contains(&"admins".to_string()));
        assert!(result.contains(&"tagA".to_string()));
    }

    #[test]
    fn test_resolve_groups_transitive() {
        let mut groups = HashMap::new();
        groups.insert("team".to_string(), vec!["tagA".to_string()]);
        groups.insert("org".to_string(), vec!["team".to_string()]);
        let result = resolve_groups(&["tagA".to_string()], &groups);
        assert!(result.contains(&"team".to_string()));
        assert!(result.contains(&"org".to_string()));
    }

    #[test]
    fn test_resolve_groups_no_match() {
        let mut groups = HashMap::new();
        groups.insert("admins".to_string(), vec!["tagA".to_string()]);
        let result = resolve_groups(&["tagZ".to_string()], &groups);
        assert!(!result.contains(&"admins".to_string()));
        assert!(result.contains(&"tagZ".to_string()));
    }

    #[tokio::test]
    async fn test_yaml_groups() {
        let content = r#"
groups:
  admins: tagA, tagB
  devops:
    - tagX
    - tagY
urls:
  - json://localhost:
      tag: tagA
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        let tags = services[0].tags();
        assert!(tags.iter().any(|t| t == "tagA"), "expected tagA, got {:?}", tags);
        assert!(tags.iter().any(|t| t == "admins"), "expected admins group, got {:?}", tags);
        // Should NOT have devops since tagX/tagY are not present
        assert!(!tags.iter().any(|t| t == "devops"), "should not have devops, got {:?}", tags);
    }

    // ─── C) Per-URL overrides (multiple instances) ──────────────────────

    #[tokio::test]
    async fn test_yaml_per_url_overrides_multiple_instances() {
        let content = r#"
urls:
  - json://localhost:
    - to: person1@example.com
      tag: tag1
    - to: person2@example.com
      tag: tag2
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 2, "expected 2 instances, got {}", services.len());
    }

    #[tokio::test]
    async fn test_yaml_per_url_overrides_single_instance() {
        let content = r#"
urls:
  - json://localhost:
      to: person1@example.com
      tag: tag1
"#;
        let (services, _) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
    }

    // ─── D) Asset section ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_yaml_asset_section_parsed() {
        let content = r#"
asset:
  app_id: MyApp
  app_desc: My Application
  app_url: https://example.com
  theme: dark
  body_format: html
  secure_logging: false
urls:
  - json://localhost
"#;
        let (services, asset) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        let asset = asset.expect("asset should be present");
        assert_eq!(asset.app_id, "MyApp");
        assert_eq!(asset.app_desc, "My Application");
        assert_eq!(asset.app_url, "https://example.com");
        assert_eq!(asset.theme, "dark");
        assert_eq!(asset.body_format, Some("html".to_string()));
        assert!(!asset.secure_logging);
    }

    #[tokio::test]
    async fn test_yaml_asset_section_empty() {
        let content = r#"
asset: {}
urls:
  - json://localhost
"#;
        let (services, asset) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        // Empty mapping still parses into a default asset
        let asset = asset.expect("empty asset section still returns Some");
        assert_eq!(asset.app_id, "Apprise");
    }

    #[tokio::test]
    async fn test_yaml_no_asset_section() {
        let content = r#"
urls:
  - json://localhost
"#;
        let (services, asset) = parse_yaml(content, 1).await.unwrap();
        assert_eq!(services.len(), 1);
        assert!(asset.is_none());
    }

    // ─── Helper unit tests ──────────────────────────────────────────────

    #[test]
    fn test_parse_tag_value_string() {
        let v = serde_yaml::Value::String("a, b".to_string());
        assert_eq!(parse_tag_value(&v), Some("a, b".to_string()));
    }

    #[test]
    fn test_parse_tag_value_sequence() {
        let v = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("x".to_string()),
            serde_yaml::Value::String("y".to_string()),
        ]);
        assert_eq!(parse_tag_value(&v), Some("x,y".to_string()));
    }

    #[test]
    fn test_parse_tag_value_empty() {
        let v = serde_yaml::Value::String("".to_string());
        assert_eq!(parse_tag_value(&v), None);
    }

    #[test]
    fn test_append_tags_to_url() {
        assert_eq!(append_tags_to_url("json://localhost", "a,b"), "json://localhost?tag=a%2Cb");
        assert_eq!(append_tags_to_url("json://localhost?x=1", "a"), "json://localhost?x=1&tag=a");
        assert_eq!(append_tags_to_url("json://localhost", ""), "json://localhost");
    }

    #[test]
    fn test_append_overrides_to_url() {
        let mut m = serde_yaml::Mapping::new();
        m.insert(
            serde_yaml::Value::String("to".to_string()),
            serde_yaml::Value::String("user@example.com".to_string()),
        );
        m.insert(
            serde_yaml::Value::String("tag".to_string()),
            serde_yaml::Value::String("ignored".to_string()),
        );
        let result = append_overrides_to_url("json://localhost", &m);
        assert!(result.contains("to=user%40example.com"));
        assert!(!result.contains("tag=ignored"));
    }
}

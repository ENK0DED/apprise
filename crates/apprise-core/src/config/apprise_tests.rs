//! Tests of the `Apprise` methods that load URLs and configuration.

#[cfg(feature = "yaml")]
use std::collections::HashMap;

use crate::Apprise;

#[cfg(feature = "email")]
#[test]
fn test_add_splits_url_lists() {
  let mut a = Apprise::new();
  assert!(a.add("json://a, xml://b;json://c json://d\njson://e"));
  assert_eq!(a.len(), 5);

  // Commas inside a query string stay with their URL
  let mut a = Apprise::new();
  assert_eq!(a.add_urls("mailto://user:pass@example.com/?to=a@example.com,b@example.com, json://localhost"), 2);

  // One bad URL makes add() false; the good ones stay
  let mut a = Apprise::new();
  assert!(!a.add("json://localhost, unknown://x"));
  assert_eq!(a.len(), 1);
  assert!(!a.add(""));
}

#[test]
fn test_url_tag_parameter_is_not_a_service_tag() {
  let mut a = Apprise::new();
  assert!(a.add("json://localhost/?tag=foo"));
  a.set_tag_strings(&["foo".to_string()]);
  assert_eq!(a.filtered_len(), 0);

  let mut a = Apprise::new();
  assert!(a.add_with_tags("json://localhost/?tag=foo", &["bar", "2:prio"]));
  a.set_tag_strings(&["bar".to_string()]);
  assert_eq!(a.filtered_len(), 1);
}

#[tokio::test]
async fn test_add_config_group_alias() {
  let dir = tempfile::tempdir().unwrap();
  let cfg = dir.path().join("apprise.conf");
  std::fs::write(&cfg, "mygrouptag=mytagA\nmytagA=json://localhost/?a\nother=json://localhost/?b\n").unwrap();

  let mut a = Apprise::new();
  assert_eq!(a.add_config(&cfg.to_string_lossy(), 1).await.unwrap(), 2);
  a.set_tag_strings(&["mygrouptag".to_string()]);
  assert_eq!(a.filtered_len(), 1);
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn test_add_config_yaml_groups() {
  let mut a = Apprise::new();
  let yaml =
    "groups:\n  team: a, b\nurls:\n  - json://localhost/?1:\n      tag: a\n  - json://localhost/?2:\n      tag: b\n  - json://localhost/?3:\n      tag: c\n";
  assert_eq!(a.add_config_text(yaml, None, 1).await.unwrap(), 3);
  a.set_tag_strings(&["team".to_string()]);
  assert_eq!(a.filtered_len(), 2);
}

#[tokio::test]
async fn test_add_config_malformed_yaml_fails_closed() {
  let dir = tempfile::tempdir().unwrap();
  let cfg = dir.path().join("apprise.yml");
  std::fs::write(&cfg, "urls:\n  - json://localhost\n  bad: [\n").unwrap();
  let mut a = Apprise::new();
  assert!(a.add_config(&cfg.to_string_lossy(), 1).await.is_err());
  assert!(a.is_empty());
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn test_templates_wait_for_values() {
  let yaml = "template:\n  - token\n  - host: localhost\nurls:\n  - json://${host}/${token}:\n      tag: t\n";
  let mut a = Apprise::new();
  // Without a value the entry does not load
  assert_eq!(a.add_config_text(yaml, None, 1).await.unwrap(), 0);
  assert_eq!(a.pending_templates().len(), 1);
  assert_eq!(a.pending_templates()[0].required(), vec!["token"]);

  // Supplying it loads the entry
  assert_eq!(a.set_template_values(HashMap::from([("TOKEN".to_string(), "abc".to_string())])).unwrap(), 1);
  assert_eq!(a.len(), 1);
  assert!(a.pending_templates().is_empty());

  // Values set first apply while loading; bad names are refused
  let mut a = Apprise::new();
  a.set_template_values(HashMap::from([("token".to_string(), "abc".to_string())])).unwrap();
  assert_eq!(a.add_config_text(yaml, None, 1).await.unwrap(), 1);
  assert!(a.set_template_values(HashMap::from([("bad-name".to_string(), "x".to_string())])).is_err());

  // Templates can be switched off: the markers are literal text
  let mut a = Apprise::new();
  a.set_allow_templates(false);
  assert_eq!(a.add_config_text(yaml, None, 1).await.unwrap(), 1);
}

#[test]
fn test_default_config_paths() {
  let paths = crate::default_config_paths();
  if cfg!(windows) {
    return;
  }
  let home = dirs::home_dir().unwrap_or_default();
  assert_eq!(paths[0], home.join(".apprise"));
  assert!(paths.contains(&home.join(".config/apprise/apprise.yml")));
  assert!(paths.contains(&std::path::PathBuf::from("/etc/apprise/apprise.conf")));
  // upstream has no /etc/apprise.conf
  assert!(!paths.contains(&std::path::PathBuf::from("/etc/apprise.conf")));
  assert_eq!(paths.len(), 23);
}

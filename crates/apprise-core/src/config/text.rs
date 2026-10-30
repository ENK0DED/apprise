//! TEXT configuration (upstream `ConfigBase.config_parse_text()`).
//!
//! ```text
//! # comments start with '#' or ';'
//! <URL>
//! <tag(s)>=<URL>
//! <group(s)>=<tag(s)>
//! include <config source>
//! ```
//!
//! Any line that fits none of these makes the whole document invalid, as
//! upstream treats it as a file that was never meant to be parsed.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex_lite::Regex;

use super::{ConfigItem, GroupTags, ParseOutput, apply_groups, assign_group, loggable, normalize_tag_groups, split_lines, tag_has_retry, tag_set};
use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::registry;
use crate::utils::parse::{ParsedUrl, parse_list};

fn line_re() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| {
    Regex::new(
      r"(?i)^\s*(?P<line>([;#]+(?P<comment>.*))|(\s*(?P<tags>[a-z0-9, \t_:-]+)\s*=|=)?\s*((?P<url>[a-z0-9]{1,32}://.*)|(?P<assign>[a-z0-9, \t_-]+))|include\s+(?P<config>.+))?\s*$",
    )
    .unwrap()
  })
}

/// Parse a TEXT document. Services are built straight away; `include`
/// entries are returned for the caller to follow.
pub fn parse_text(content: &str, asset: &AppriseAsset) -> Result<ParseOutput, ConfigError> {
  let mut out = ParseOutput::default();
  let mut group_tags = GroupTags::new();
  let mut preloaded: Vec<(ParsedUrl, BTreeSet<String>, usize, String)> = Vec::new();

  for (index, entry) in split_lines(content).enumerate() {
    let line = index + 1;
    let Some(c) = line_re().captures(entry) else {
      tracing::error!("Invalid Apprise TEXT configuration format found {} on line {}.", loggable(entry, asset), line);
      return Err(ConfigError::InvalidFormat(format!("invalid TEXT configuration on line {}", line)));
    };

    let url = c.name("url").map(|m| m.as_str());
    let assign = c.name("assign").map(|m| m.as_str());
    let config = c.name("config").map(|m| m.as_str());
    let tags = c.name("tags").map(|m| m.as_str()).unwrap_or("");

    if let Some(config) = config {
      tracing::debug!("Include URL: {}", loggable(config, asset));
      out.includes.push(config.trim().to_string());
      continue;
    }

    if let Some(assign) = assign {
      let groups = parse_list(tags);
      if groups.is_empty() {
        tracing::warn!("Unparseable tag assignment - no group(s) on line {}", line);
        continue;
      }
      let members = parse_list(assign);
      if members.is_empty() {
        tracing::warn!("Unparseable tag assignment - no tag(s) to assign on line {}", line);
        continue;
      }
      assign_group(&mut group_tags, &groups, &members, true);
      continue;
    }

    let Some(url) = url else {
      // Comment or blank line
      continue;
    };

    let logged = loggable(url, asset);
    let Some(parsed) = registry::parse_service_url(url) else {
      tracing::debug!("Unparseable URL {} on line {}.", logged, line);
      continue;
    };

    let tags = tag_set(tags);
    // A retry count belongs on the URL (?retry=N), not on a service tag.
    if tags.iter().any(|t| tag_has_retry(t)) {
      tracing::warn!("You can not specify a retry count as part of a service tag on line {}; use ?retry=N in the URL instead.", line);
      continue;
    }

    preloaded.push((parsed, tags, line, logged));
  }

  normalize_tag_groups(&mut group_tags);

  for (mut parsed, mut tags, line, logged) in preloaded {
    apply_groups(&mut tags, &group_tags);
    parsed.assigned_tags = Some(tags.into_iter().collect());
    match registry::from_parsed(&parsed) {
      Some(service) => {
        tracing::debug!("Loaded URL: {}", logged);
        out.items.push(ConfigItem::Service(service));
      }
      None => tracing::error!("Could not load URL {} on line {}.", logged, line),
    }
  }

  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn services(content: &str) -> Vec<Box<dyn crate::notify::Notify>> {
    parse_text(content, &AppriseAsset::default())
      .unwrap()
      .items
      .into_iter()
      .filter_map(|i| match i {
        ConfigItem::Service(s) => Some(s),
        ConfigItem::Pending(_) => None,
      })
      .collect()
  }

  fn tags_of(content: &str) -> Vec<Vec<String>> {
    services(content).iter().map(|s| s.tags()).collect()
  }

  #[test]
  fn test_parse_one_url_per_line() {
    assert_eq!(services("json://localhost\nxml://localhost").len(), 2);
    assert_eq!(services("\r\n\njson://localhost\r\n\n\nxml://localhost\n\n").len(), 2);
  }

  #[test]
  fn test_comments() {
    assert_eq!(services("# comment\n; comment\n  # indented\njson://localhost").len(), 1);
    assert!(services("# comment 1\n; comment 2").is_empty());
    assert!(services("").is_empty());
    // A '#' starts the URL fragment, so trailing text is ignored
    assert_eq!(services("json://localhost # my service").len(), 1);
  }

  #[test]
  fn test_tags() {
    assert_eq!(tags_of("mytag=json://localhost"), vec![vec!["mytag"]]);
    assert_eq!(tags_of("tag2, tag1 tag3 =json://localhost"), vec![vec!["tag1", "tag2", "tag3"]]);
    // The priority prefix stays in the raw tag
    assert_eq!(tags_of("2:endpoint=ntfy://localhost/topic"), vec![vec!["2:endpoint"]]);
    // An empty tag list is allowed
    assert_eq!(tags_of("=json://localhost"), vec![Vec::<String>::new()]);
  }

  #[test]
  fn test_url_tag_parameter_is_not_a_service_tag() {
    // Upstream: ?tag= on the URL belongs to the plugin
    assert_eq!(tags_of("json://localhost?tag=foo"), vec![Vec::<String>::new()]);
    assert_eq!(tags_of("bar=ntfy://localhost/topic?tags=emoji"), vec![vec!["bar"]]);
  }

  #[test]
  fn test_retry_in_service_tag_is_rejected() {
    assert!(services("alerts:3=json://localhost").is_empty());
    assert_eq!(services("2:alerts=json://localhost").len(), 1);
  }

  #[test]
  fn test_group_assignment() {
    // Upstream test_config_base.py::test_config_base_config_tag_groups_text
    let content = "
      # Tag assignments
      groupA, groupB = tagB, tagC
      groupC = groupA
      groupD = groupE
      groupE = tagD

      tagA=json://localhost/?A
      tagB=json://localhost/?B
      tagC=json://localhost/?C
      tagD=json://localhost/?D
    ";
    let got = tags_of(content);
    assert_eq!(got[0], vec!["tagA"]);
    assert_eq!(got[1], vec!["groupA", "groupB", "groupC", "tagB"]);
    assert_eq!(got[2], vec!["groupA", "groupB", "groupC", "tagC"]);
    assert_eq!(got[3], vec!["groupD", "groupE", "tagD"]);
  }

  #[test]
  fn test_group_alias_from_issue() {
    // `-g mygrouptag` must reach every service tagged mytagA
    let got = tags_of("mygrouptag=mytagA\nmytagA=json://localhost\nother=json://localhost");
    assert_eq!(got, vec![vec!["mygrouptag".to_string(), "mytagA".to_string()], vec!["other".to_string()]]);
  }

  #[test]
  fn test_group_cycle_is_safe() {
    // Same result as upstream for this cycle
    let got = tags_of("a=b\nb=a\nc=a\nb=json://localhost");
    assert_eq!(got, vec![vec!["b"]]);
  }

  #[test]
  fn test_invalid_line_rejects_document() {
    assert!(parse_text("json://localhost\n!!!", &AppriseAsset::default()).is_err());
    // A tag assignment may not carry a priority
    assert!(parse_text("grp=2:tag\njson://localhost", &AppriseAsset::default()).is_err());
  }

  #[test]
  fn test_non_url_words_are_ignored() {
    // Words alone read as a tag assignment without a group
    assert_eq!(services("not a url\njson://localhost\nalso not a url").len(), 1);
  }

  #[test]
  fn test_unknown_schema_skipped() {
    assert!(services("totallyunknownschema://whatever").is_empty());
  }

  #[test]
  fn test_include_lines() {
    let out = parse_text("include http://localhost/config.cfg\nINCLUDE  /etc/other.yml\njson://localhost", &AppriseAsset::default()).unwrap();
    assert_eq!(out.includes, vec!["http://localhost/config.cfg", "/etc/other.yml"]);
    assert_eq!(out.items.len(), 1);
  }
}

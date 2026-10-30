//! Tags: parsing `priority:tag:retry` tokens and deciding which loaded
//! services a notification goes to (upstream `apprise/tag.py` and
//! `utils/logic.py::is_exclusive_match`).

use std::fmt;

/// The filter tag that selects every service.
pub const MATCH_ALL_TAG: &str = "all";

/// A service tagged with this is notified whatever tags the caller asked for.
pub const MATCH_ALWAYS_TAG: &str = "always";

/// A tag name with optional priority and retry metadata.
///
/// Supported forms:
/// - `tag`: priority 0, no retry
/// - `priority:tag`: an explicit priority (configuration files)
/// - `tag:retry`: a retry override (CLI and API filters)
/// - `priority:tag:retry`
///
/// The name is the identity: two tags are equal when their (lowercased)
/// names match, whatever their priority or retry.
#[derive(Debug, Clone, Default)]
pub struct AppriseTag {
  name: String,
  /// Dispatch priority; lower numbers are dispatched first.
  pub priority: u64,
  /// Retry count carried by a filter token, if one was written.
  pub retry: Option<u64>,
  /// Whether the priority was written explicitly (`0:tag` counts). This
  /// decides between exclusive and escalating dispatch.
  pub has_priority: bool,
}

impl AppriseTag {
  /// A tag with no priority or retry metadata.
  pub fn new(name: &str) -> Self {
    Self { name: name.trim().to_lowercase(), ..Default::default() }
  }

  /// Parse `[priority:]tag[:retry]`. Input that does not fit the pattern is
  /// kept whole (lowercased) as the tag name. Numbers too large to hold are
  /// treated as absent.
  pub fn parse(value: &str) -> Self {
    let value = value.trim();
    let parts: Vec<&str> = value.split(':').collect();
    let parsed = match parts.as_slice() {
      [tag] if is_tag_name(tag) => Some((None, *tag, None)),
      // Like upstream's regex, a leading number is a priority first.
      [p, tag] if is_digits(p) && is_tag_name(tag) => Some((Some(*p), *tag, None)),
      [tag, r] if is_tag_name(tag) && is_digits(r) => Some((None, *tag, Some(*r))),
      [p, tag, r] if is_digits(p) && is_tag_name(tag) && is_digits(r) => Some((Some(*p), *tag, Some(*r))),
      _ => None,
    };
    let Some((priority, tag, retry)) = parsed else {
      return Self::new(value);
    };
    let priority = priority.and_then(|p| p.parse::<u64>().ok());
    Self { name: tag.to_lowercase(), priority: priority.unwrap_or(0), retry: retry.and_then(|r| r.parse::<u64>().ok()), has_priority: priority.is_some() }
  }

  /// The bare lowercase tag name.
  pub fn name(&self) -> &str {
    &self.name
  }

  /// Whether this tag's name is `name` (compared case-insensitively).
  pub fn is(&self, name: &str) -> bool {
    self.name == name.trim().to_lowercase()
  }

  /// An empty name is not a usable tag.
  pub fn is_empty(&self) -> bool {
    self.name.is_empty()
  }
}

impl PartialEq for AppriseTag {
  fn eq(&self, other: &Self) -> bool {
    self.name == other.name
  }
}

impl Eq for AppriseTag {}

impl std::hash::Hash for AppriseTag {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.name.hash(state);
  }
}

impl PartialOrd for AppriseTag {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for AppriseTag {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    self.name.cmp(&other.name)
  }
}

impl fmt::Display for AppriseTag {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.name)
  }
}

impl From<&str> for AppriseTag {
  fn from(value: &str) -> Self {
    Self::parse(value)
  }
}

fn is_digits(s: &str) -> bool {
  !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_tag_name(s: &str) -> bool {
  let mut bytes = s.bytes();
  matches!(bytes.next(), Some(b) if b.is_ascii_alphanumeric()) && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Split a tag string on upstream's list delimiters (whitespace, `,`, `;`,
/// `[` and `]`), dropping empty entries and duplicates and sorting the rest,
/// as upstream `parse_list()` does.
pub fn parse_list(value: &str) -> Vec<String> {
  let mut out: Vec<String> =
    value.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).filter(|s| !s.is_empty()).map(str::to_string).collect();
  out.sort();
  out.dedup();
  out
}

/// Parse the tags a service carries (raw strings such as `2:tagA`).
pub fn parse_service_tags(raw: &[String]) -> Vec<AppriseTag> {
  let mut tags: Vec<AppriseTag> = Vec::new();
  for value in raw {
    for token in parse_list(value) {
      let tag = AppriseTag::parse(&token);
      if !tag.is_empty() && !tags.contains(&tag) {
        tags.push(tag);
      }
    }
  }
  tags
}

/// Which services a notification goes to.
///
/// `Groups` is a list of alternatives (OR); each alternative is a list of
/// tokens that must all match (AND). Tokens may carry a priority prefix
/// (`2:tag`, match only that priority) and a retry suffix (`tag:3`).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TagFilter {
  /// Every service (upstream's `tag="all"`, the API default).
  #[default]
  All,
  /// Only services that have no tags at all (upstream's `tag=None`; the CLI
  /// without `--tag`).
  Untagged,
  /// OR of AND groups.
  Groups(Vec<Vec<String>>),
}

impl TagFilter {
  /// Build a filter from CLI-style values: each value is one AND group whose
  /// tokens are separated by commas or whitespace, and the values are OR'd.
  /// No values selects untagged services only, as the upstream CLI does.
  pub fn from_cli(values: &[String]) -> Self {
    let groups: Vec<Vec<String>> = values.iter().map(|v| parse_list(v)).filter(|g| !g.is_empty()).collect();
    if values.is_empty() { Self::Untagged } else { Self::Groups(groups) }
  }

  /// Every token, parsed, in filter order.
  pub fn tokens(&self) -> Vec<AppriseTag> {
    match self {
      Self::Groups(groups) => groups.iter().flatten().map(|t| AppriseTag::parse(t)).filter(|t| !t.is(MATCH_ALL_TAG)).collect(),
      _ => Vec::new(),
    }
  }

  /// Whether any token names an exact priority (`2:tag`). Such a filter is
  /// dispatched as one flat batch instead of escalating by priority.
  pub fn has_explicit_priority(&self) -> bool {
    self.tokens().iter().any(|t| t.has_priority)
  }

  /// Whether this filter is upstream's match-all (`all`, or no filter).
  pub fn is_match_all(&self) -> bool {
    match self {
      Self::All => true,
      Self::Untagged => false,
      Self::Groups(groups) => groups.len() == 1 && groups[0].len() == 1 && groups[0][0].trim().eq_ignore_ascii_case(MATCH_ALL_TAG),
    }
  }

  /// Whether a service carrying `service_tags` is selected. A service tagged
  /// `always` is selected by any non-empty filter.
  pub fn matches(&self, service_tags: &[AppriseTag]) -> bool {
    let groups = match self {
      Self::All => return true,
      Self::Untagged => return service_tags.is_empty(),
      Self::Groups(groups) => groups,
    };
    if groups.is_empty() {
      // Upstream: no logic matches only services without tags.
      return service_tags.is_empty();
    }
    if service_tags.iter().any(|t| t.is(MATCH_ALWAYS_TAG)) {
      return true;
    }
    groups.iter().any(|group| {
      let tokens: Vec<String> = group.iter().flat_map(|t| parse_list(t)).collect();
      if tokens.is_empty() {
        return service_tags.is_empty();
      }
      tokens.iter().all(|t| token_matches(&AppriseTag::parse(t), service_tags))
    })
  }

  /// The first retry override (`tag:N`) that applies to a service. A token
  /// with a priority prefix applies only to that priority.
  pub fn retry_for(&self, service_tags: &[AppriseTag]) -> Option<u64> {
    self
      .tokens()
      .into_iter()
      .filter(|t| t.retry.is_some())
      .find(|ft| service_tags.iter().any(|st| st == ft && (!ft.has_priority || st.priority == ft.priority)))
      .and_then(|t| t.retry)
  }

  /// The independent escalation chains for this filter, by name. Each OR
  /// token becomes its own chain; services that joined through an AND group
  /// (or a match-all filter) share the `""` chain.
  pub(crate) fn chain_names(&self) -> Vec<String> {
    let Self::Groups(groups) = self else { return Vec::new() };
    if self.is_match_all() {
      return Vec::new();
    }
    let mut names = Vec::new();
    for group in groups {
      let flat: Vec<String> = group.iter().flat_map(|t| parse_list(t)).collect();
      match flat.as_slice() {
        [one] => names.push(AppriseTag::parse(one).name().to_string()),
        _ => names.push(String::new()),
      }
    }
    names
  }
}

fn token_matches(token: &AppriseTag, service_tags: &[AppriseTag]) -> bool {
  if token.is(MATCH_ALL_TAG) {
    return true;
  }
  service_tags.iter().any(|st| st == token && (!token.has_priority || st.priority == token.priority))
}

/// Whether a service with `service_tags` is selected by `tag_groups`.
///
/// Each inner group is an AND of tags; the groups are OR'd. No groups selects
/// every service. Kept for callers of the earlier API; see [`TagFilter`].
pub fn is_match(service_tags: &[String], tag_groups: &[Vec<String>]) -> bool {
  let filter = if tag_groups.is_empty() { TagFilter::All } else { TagFilter::Groups(tag_groups.to_vec()) };
  filter.matches(&parse_service_tags(service_tags))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tags(values: &[&str]) -> Vec<AppriseTag> {
    parse_service_tags(&values.iter().map(|s| s.to_string()).collect::<Vec<_>>())
  }

  fn groups(values: &[&[&str]]) -> TagFilter {
    TagFilter::Groups(values.iter().map(|g| g.iter().map(|s| s.to_string()).collect()).collect())
  }

  #[test]
  fn test_parse_forms() {
    let t = AppriseTag::parse("2:tagA");
    assert_eq!((t.name(), t.priority, t.retry, t.has_priority), ("taga", 2, None, true));
    let t = AppriseTag::parse("tagA:3");
    assert_eq!((t.name(), t.priority, t.retry, t.has_priority), ("taga", 0, Some(3), false));
    let t = AppriseTag::parse("2:tagA:3");
    assert_eq!((t.name(), t.priority, t.retry, t.has_priority), ("taga", 2, Some(3), true));
    let t = AppriseTag::parse("0:x");
    assert_eq!((t.name(), t.priority, t.has_priority), ("x", 0, true));
    // A leading number is read as a priority first.
    let t = AppriseTag::parse("1:2:3");
    assert_eq!((t.name(), t.priority, t.retry), ("2", 1, Some(3)));
    let t = AppriseTag::parse("1:2");
    assert_eq!((t.name(), t.priority, t.retry), ("2", 1, None));
  }

  #[test]
  fn test_parse_fallback_keeps_whole_value() {
    for (raw, name) in [("@invalid", "@invalid"), ("-bad", "-bad"), ("x:", "x:"), (":x", ":x"), ("  TAG  ", "tag")] {
      let t = AppriseTag::parse(raw);
      assert_eq!(t.name(), name);
      assert_eq!(t.priority, 0);
      assert_eq!(t.retry, None);
      assert!(!t.has_priority);
    }
  }

  #[test]
  fn test_parse_rejects_absurd_digit_runs() {
    let huge = "9".repeat(5000);
    let t = AppriseTag::parse(&format!("alerts:{huge}"));
    assert_eq!(t.name(), "alerts");
    assert_eq!(t.retry, None);
    let t = AppriseTag::parse(&format!("{huge}:alerts"));
    assert_eq!(t.name(), "alerts");
    assert_eq!(t.priority, 0);
    assert!(!t.has_priority);
    assert!(!groups(&[&[&format!("{huge}:alerts")]]).has_explicit_priority());
    assert!(groups(&[&["3:alerts:5"]]).has_explicit_priority());
  }

  #[test]
  fn test_equality_is_by_name_and_case_insensitive() {
    assert_eq!(AppriseTag::parse("abc"), AppriseTag::parse("5:abc"));
    assert_eq!(AppriseTag::new("ABC"), AppriseTag::new("abc"));
    assert_ne!(AppriseTag::new("abc"), AppriseTag::new("xyz"));
    assert!(AppriseTag::new("HELLO").is("hello"));
    assert!(AppriseTag::new("abc") < AppriseTag::new("def"));
    assert!(AppriseTag::new("").is_empty());
  }

  #[test]
  fn test_parse_list_matches_upstream() {
    assert_eq!(parse_list("b, a;c [d] e\tf,,a"), vec!["a", "b", "c", "d", "e", "f"]);
    assert!(parse_list(" , ").is_empty());
  }

  // Cases checked against upstream `is_exclusive_match`.
  #[test]
  fn test_matching_matches_upstream() {
    assert!(TagFilter::Untagged.matches(&tags(&[])));
    assert!(!TagFilter::Untagged.matches(&tags(&["always"])));
    assert!(!TagFilter::Untagged.matches(&tags(&["a"])));
    assert!(TagFilter::All.matches(&tags(&["a"])));
    assert!(groups(&[&["all"]]).matches(&tags(&[])));
    assert!(groups(&[&["all"]]).matches(&tags(&["a"])));
    assert!(!groups(&[&["a", "b"]]).matches(&tags(&["a"])));
    assert!(groups(&[&["a", "b"]]).matches(&tags(&["a", "b"])));
    assert!(groups(&[&["a"], &["b"]]).matches(&tags(&["b"])));
    assert!(groups(&[&["A"]]).matches(&tags(&["a"])));
    assert!(groups(&[&["Tag_A"]]).matches(&tags(&["tag_a"])));
    assert!(groups(&[&["2:a"]]).matches(&tags(&["2:a"])));
    assert!(!groups(&[&["2:a"]]).matches(&tags(&["a"])));
    assert!(groups(&[&["a"]]).matches(&tags(&["3:a"])));
    assert!(groups(&[&["2:a", "b"]]).matches(&tags(&["2:a", "b"])));
    assert!(groups(&[&["a:3"]]).matches(&tags(&["a"])));
    assert!(groups(&[&["x"]]).matches(&tags(&["always"])));
    assert!(groups(&[&["x", "y"]]).matches(&tags(&["always"])));
    assert!(!groups(&[&["all", "a"]]).matches(&tags(&["b"])));
    assert!(groups(&[&["all", "a"]]).matches(&tags(&["a"])));
    // Whitespace splits a token, so neither half matches.
    assert!(!groups(&[&["bad tag!"]]).matches(&tags(&["bad-tag!"])));
    assert!(!groups(&[&[""]]).matches(&tags(&["a"])));
    assert!(groups(&[&[""]]).matches(&tags(&[])));
    assert!(TagFilter::Groups(vec![]).matches(&tags(&[])));
    assert!(!TagFilter::Groups(vec![]).matches(&tags(&["a"])));
  }

  #[test]
  fn test_from_cli() {
    assert_eq!(TagFilter::from_cli(&[]), TagFilter::Untagged);
    assert_eq!(TagFilter::from_cli(&["a,b".into(), "c".into()]), groups(&[&["a", "b"], &["c"]]));
    assert!(TagFilter::from_cli(&["all".into()]).is_match_all());
  }

  #[test]
  fn test_retry_for() {
    let f = groups(&[&["a:3"], &["b:5"]]);
    assert_eq!(f.retry_for(&tags(&["b"])), Some(5));
    assert_eq!(f.retry_for(&tags(&["a", "b"])), Some(3));
    assert_eq!(f.retry_for(&tags(&["c"])), None);
    // A priority prefix restricts the override to that priority.
    let f = groups(&[&["2:a:4"]]);
    assert_eq!(f.retry_for(&tags(&["2:a"])), Some(4));
    assert_eq!(f.retry_for(&tags(&["1:a"])), None);
    assert_eq!(groups(&[&["a"]]).retry_for(&tags(&["a"])), None);
  }

  #[test]
  fn test_chain_names() {
    assert_eq!(groups(&[&["a"], &["2:b:3"]]).chain_names(), vec!["a", "b"]);
    assert_eq!(groups(&[&["a", "b"], &["c"]]).chain_names(), vec!["", "c"]);
    assert!(groups(&[&["all"]]).chain_names().is_empty());
    assert!(TagFilter::All.chain_names().is_empty());
  }

  #[test]
  fn test_legacy_is_match() {
    assert!(is_match(&["x".into()], &[]));
    assert!(is_match(&["A".into()], &[vec!["a".into()]]));
    assert!(is_match(&["always".into()], &[vec!["zzz".into()]]));
    assert!(!is_match(&["b".into()], &[vec!["all".into(), "a".into()]]));
  }
}

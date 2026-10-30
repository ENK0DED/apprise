//! Tag matching: decides which loaded services a notification goes to.

/// Whether a service with `service_tags` is selected by `tag_groups`.
///
/// Each inner group is an AND of tags; the groups are OR'd. No groups selects
/// every service. A service tagged `always` is always selected, and the
/// filter tag `all` selects every service.
pub fn is_match(service_tags: &[String], tag_groups: &[Vec<String>]) -> bool {
  if tag_groups.is_empty() {
    return true;
  }
  let svc_tags: Vec<String> = service_tags.iter().map(|t| t.to_lowercase()).collect();
  if svc_tags.iter().any(|t| t == "always") {
    return true;
  }
  tag_groups.iter().any(|and_group| {
    if and_group.iter().any(|t| t == "all") {
      return true;
    }
    and_group.iter().all(|t| svc_tags.contains(t))
  })
}

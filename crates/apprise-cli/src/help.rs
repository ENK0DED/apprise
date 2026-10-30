//! `--help` text, laid out the way upstream's Click formatter does.

use crate::args::{Kind, OPTIONS};

/// Defaults shown in the help text; they follow the environment.
pub struct HelpDefaults {
  pub storage_path: String,
  pub storage_prune_days: i64,
  pub storage_uid_length: i64,
}

fn describe(id: &str, d: &HelpDefaults) -> String {
  match id {
    "body" => "Specify the message body. If no body is specified then content is read from <stdin>.".into(),
    "title" => "Specify the message title. This field is completely optional.".into(),
    "plugin_path" => "Specify one or more plugin paths to scan.".into(),
    "storage_path" => format!("Specify the path to the persistent storage location (default={}).", d.storage_path),
    "storage_prune_days" => format!(
      "Define the number of days the storage prune should run using. Setting this to zero (0) will eliminate all accumulated content. By default this value is {} days.",
      d.storage_prune_days
    ),
    "storage_uid_length" => {
      format!("Define the number of unique characters to store persistent cache in. By default this value is {} characters.", d.storage_uid_length)
    }
    "storage_mode" => "Specify the persistent storage operational mode (default=auto). Possible values are: \"auto\", \"flush\", \"memory\".".into(),
    "config" => "Specify one or more configuration locations.".into(),
    "attach" => "Specify one or more attachments.".into(),
    "notification_type" => "Specify the message type (default=info). Possible values are: \"info\", \"success\", \"warning\", \"failure\".".into(),
    "input_format" => "Specify the message input format (default=text). Possible values are: \"text\", \"markdown\", \"html\".".into(),
    "theme" => "Specify the default theme.".into(),
    "template_var" => "Supply a value used by a YAML configuration written with ${NAME}. Use multiple --template-var (-tv) entries for more than one. A value not given here is looked for in the configuration's own default and then in APPRISE_TEMPLATE_<NAME>.".into(),
    "tag" => "Specify one or more tags to filter which services to notify. Use multiple --tag (-g) entries to match ANY tag. Use comma separators to require ALL tags (strict match). Omit to notify untagged services only, or use \"all\" to notify everything.".into(),
    "limit" => "Give up on the whole run if it's taking too long, in seconds (whole number or decimal, e.g. 2.5). By default (0) there is no limit -- see --service-limit to cap each service on its own instead.".into(),
    "service_limit" => format!(
      "Give up on any single service if it's taking too long, in seconds (whole number or decimal, e.g. 2.5), or 0 to turn this off entirely. Leave unset to use Apprise's own default of {} seconds.",
      apprise_core::asset::DEFAULT_SERVICE_TIMEOUT
    ),
    "disable_async" => "Send all notifications sequentially".into(),
    "dry_run" => "Perform a trial run but only prints the notification services to-be triggered to stdout. Notifications are never sent using this mode.".into(),
    "details" => "Prints details about the current services supported by Apprise.".into(),
    "recursion_depth" => "The number of recursive import entries that can be loaded from within Apprise configuration. By default this is set to 1.".into(),
    "verbose" => "Makes the operation more talkative. Use multiple v to increase the verbosity. I.e.: -vvvv".into(),
    "interpret_escapes" => "Enable interpretation of backslash escapes".into(),
    "interpret_emojis" => "Enable interpretation of :emoji: definitions".into(),
    "debug" => "Debug mode".into(),
    "version" => "Display the apprise version and exit.".into(),
    "help" => "Show this message and exit.".into(),
    _ => String::new(),
  }
}

const HEADER: &str = "Usage:
   apprise [OPTIONS] [APPRISE_URL [APPRISE_URL2 [APPRISE_URL3]]]
   apprise storage [OPTIONS] [ACTION] [UID1 [UID2 [UID3]]]

Send a notification to the services identified by their URLs
the content provided within the title, body and notification-type.

For a list of all of the supported services and information on how to use
them, check out https://github.com/caronc/apprise

Options:
";

const ACTIONS: &str = "
Actions:
  storage             Access the persistent storage disk administration
    list              List all URL IDs associated with detected URL(s). This
                      is also the default action run if nothing is provided
    prune             Eliminates stale entries found based on --storage-prune-
                      days (-SPD)
    clean             Removes any persistent data created by Apprise
";

/// Click's widths: 78 columns, a 30 column option column plus 2 spaces.
const TEXT_WIDTH: usize = 78;
const FIRST_COL: usize = 32;
const INDENT: usize = 2;

/// The complete `--help` output.
pub fn render(defaults: &HelpDefaults) -> String {
  let mut out = String::from(HEADER);
  for spec in OPTIONS {
    let mut names: Vec<&str> = spec.names.to_vec();
    // Shorter prefixes first, as Click lists them.
    names.sort_by_key(|n| if n.starts_with("--") { 2 } else { 1 });
    let mut first = names.join(", ");
    if !matches!(spec.kind, Kind::Flag | Kind::Count) {
      first.push(' ');
      first.push_str(spec.metavar);
    }
    let lines = wrap(&describe(spec.id, defaults), TEXT_WIDTH - FIRST_COL - INDENT);
    let pad = " ".repeat(FIRST_COL + INDENT);
    if first.len() <= FIRST_COL - 2 {
      out.push_str(&format!("{}{:<width$}{}\n", " ".repeat(INDENT), first, lines.first().map(String::as_str).unwrap_or(""), width = FIRST_COL));
    } else {
      out.push_str(&format!("{}{}\n", " ".repeat(INDENT), first));
      if let Some(line) = lines.first() {
        out.push_str(&format!("{pad}{line}\n"));
      }
    }
    for line in lines.iter().skip(1) {
      out.push_str(&format!("{pad}{line}\n"));
    }
  }
  out.push_str(ACTIONS);
  out
}

/// Python's `textwrap.wrap(text, width)`: words split at whitespace and
/// after hyphens inside words, long words broken to fill the line.
fn wrap(text: &str, width: usize) -> Vec<String> {
  let mut chunks: Vec<String> = Vec::new();
  for (i, part) in text.split(' ').enumerate() {
    if i > 0 {
      chunks.push(" ".into());
    }
    chunks.extend(split_hyphens(part));
  }
  chunks.retain(|c| !c.is_empty());
  chunks.reverse();
  let mut lines = Vec::new();
  while !chunks.is_empty() {
    let mut line: Vec<String> = Vec::new();
    let mut len = 0;
    // Drop leading whitespace on every line.
    if chunks.last().is_some_and(|c| c.trim().is_empty()) && !lines.is_empty() {
      chunks.pop();
    }
    while let Some(chunk) = chunks.last() {
      let l = chunk.chars().count();
      if len + l <= width {
        len += l;
        line.push(chunks.pop().unwrap());
      } else {
        break;
      }
    }
    if let Some(chunk) = chunks.last()
      && chunk.chars().count() > width
    {
      // Break a word that can never fit, filling the current line.
      let space = width.saturating_sub(len).max(1);
      let chunk = chunks.pop().unwrap();
      let head: String = chunk.chars().take(space).collect();
      let tail: String = chunk.chars().skip(space).collect();
      line.push(head);
      chunks.push(tail);
    }
    while line.last().is_some_and(|c| c.trim().is_empty()) {
      line.pop();
    }
    if !line.is_empty() {
      lines.push(line.concat());
    }
  }
  lines
}

/// Split after a hyphen preceded by two letters and followed by a letter,
/// as Python's textwrap does for hyphenated words.
fn split_hyphens(word: &str) -> Vec<String> {
  let chars: Vec<char> = word.chars().collect();
  let mut out = Vec::new();
  let mut start = 0;
  for i in 0..chars.len() {
    let breakable = chars[i] == '-'
      && i >= 2
      && chars[i - 1].is_alphabetic()
      && chars[i - 2].is_alphabetic()
      && chars.get(i + 1).is_some_and(|c| c.is_alphabetic())
      && chars.get(i + 2).is_some_and(|c| c.is_alphabetic() || *c == '-');
    if breakable {
      out.push(chars[start..=i].iter().collect());
      start = i + 1;
    }
  }
  out.push(chars[start..].iter().collect());
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_help_matches_upstream() {
    let expected = include_str!("../tests/help.txt");
    let actual = render(&HelpDefaults { storage_path: "~/.local/share/apprise/cache".into(), storage_prune_days: 30, storage_uid_length: 8 });
    for (n, (a, e)) in actual.lines().zip(expected.lines()).enumerate() {
      assert_eq!(a, e, "line {}", n + 1);
    }
    assert_eq!(actual.lines().count(), expected.lines().count());
  }

  #[test]
  fn test_long_default_is_broken_like_textwrap() {
    let actual = render(&HelpDefaults { storage_path: "/very/long/storage/path/that/goes/on/and/on".into(), storage_prune_days: 365, storage_uid_length: 8 });
    assert!(
      actual
        .contains("                                  location (default=/very/long/storage/path/th\n                                  at/goes/on/and/on).\n")
    );
  }
}

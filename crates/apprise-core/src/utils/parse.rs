//! URL and list parsing, ported from upstream `apprise/utils/parse.py`
//! (`parse_url()`, `parse_qsd()`, `parse_urls()`, `parse_list()`,
//! `parse_bool()`) and the parse side of `apprise/url.py`
//! (`URLBase.parse_url()` and its post-processing).
//!
//! Upstream behaviour is the spec, quirks included: a `;` in the last path
//! segment drops the rest of it (Python's `urlparse()` treats it as
//! parameters), a second `:` in the user info truncates the password, and
//! so on. The parity tests at the bottom of this file replay vectors
//! captured from upstream.

use std::collections::HashMap;

use indexmap::IndexMap;
use url::Url;

use crate::utils::validate::verify_hostname;

/// RFC 3986 path characters left unencoded in a path segment
/// (upstream `URL_SEGMENT_SAFE_CHARS`). Commas and semicolons stay encoded
/// because Apprise uses them as list delimiters, and `/` stays encoded so
/// an escaped slash remains part of its segment.
pub const URL_SEGMENT_SAFE_CHARS: &str = ":@!$&'()*+=";

/// The same characters plus `/`, for quoting a whole path
/// (upstream `URL_PATH_SAFE_CHARS`).
pub const URL_PATH_SAFE_CHARS: &str = "/:@!$&'()*+=";

/// A parsed Apprise URL with all components accessible.
///
/// Built by [`ParsedUrl::parse`], which follows upstream
/// `URLBase.parse_url(url, verify_host=False)`. Plugins that upstream
/// parses with host verification should check [`ParsedUrl::verified_host`].
#[derive(Debug, Clone, Default)]
pub struct ParsedUrl {
  /// Lowercase schema (`json`, `https`, ...).
  pub schema: String,
  /// Decoded user, after a `?user=` override. `None` when absent or empty.
  pub user: Option<String>,
  /// Decoded password, after a `?password=`/`?pass=` override. `None` when
  /// absent or empty.
  pub password: Option<String>,
  /// Decoded host. A port that is not a number stays part of the host
  /// (`tgram://123:abc` has host `123:abc`). `None` when empty.
  pub host: Option<String>,
  /// The host as the URL wrote it, before decoding; [`Self::verified_host`]
  /// checks this form, as upstream's `verify_host` does.
  pub host_encoded: Option<String>,
  /// Port, from the authority or a `?port=` override.
  pub port: Option<u16>,
  /// Percent-encoded path without its leading slash (upstream `fullpath`
  /// minus the `/`). Keeps a trailing slash.
  pub path: String,
  /// Decoded, non-empty path segments (upstream `split_path(fullpath)`).
  pub path_parts: Vec<String>,
  /// Query string arguments. Values are decoded and trimmed. Keys are
  /// decoded and trimmed but keep their case; keys that differ only in
  /// case collapse to the last one, as upstream's lowercased `qsd` does.
  /// Use [`ParsedUrl::get`] for case-insensitive lookups and
  /// [`ParsedUrl::qsd_ordered`] for URL order; change it through
  /// [`ParsedUrl::set_arg`] so the order stays known.
  pub qsd: HashMap<String, String>,
  /// The keys of `qsd` in the order the URL wrote them (upstream's `qsd`
  /// is an insertion-ordered dict). A repeated key keeps its first place.
  pub qsd_order: Vec<String>,
  /// Arguments written as `?+name=value`, in URL order, with the `+`
  /// removed and the case kept (upstream `qsd+`, typically extra HTTP
  /// headers).
  pub qsd_add: IndexMap<String, String>,
  /// Arguments written as `?-name=value` (upstream `qsd-`).
  pub qsd_del: IndexMap<String, String>,
  /// Arguments written as `?:name=value` (upstream `qsd:`).
  pub qsd_colon: IndexMap<String, String>,
  /// Tags assigned by whatever loaded this URL (`tag=URL` text config
  /// lines, YAML `tag:`/`tags:`, [`crate::Apprise::add_with_tags`]).
  /// Raw strings, `priority:tag` form kept intact.
  pub assigned_tags: Option<Vec<String>>,
  /// Original raw URL string
  pub raw: String,
}

/// Options for [`ParsedUrl::parse_with`]; they mirror upstream `parse_url()`
/// keyword arguments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
  /// Reject the URL unless its host is a valid hostname or IP address and
  /// any port is numeric.
  pub verify_host: bool,
  /// With `verify_host`, also require a port in 1..=65535.
  pub strict_port: bool,
  /// Decode `+` in query values as a space (only office365 does this).
  pub plus_to_space: bool,
}

/// The pieces upstream `parse_url()` extracts, before `URLBase`
/// post-processing.
struct RawParts {
  schema: String,
  user: Option<String>,
  password: Option<String>,
  host: String,
  port: Option<i64>,
  /// The port text failed to convert (`host:abc`).
  bad_port: bool,
  fullpath: Option<String>,
  qsd: Qsd,
}

/// Query string dictionaries produced by [`parse_qsd`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Qsd {
  pub qsd: HashMap<String, String>,
  /// Keys of `qsd` in URL order.
  pub order: Vec<String>,
  pub add: IndexMap<String, String>,
  pub del: IndexMap<String, String>,
  pub colon: IndexMap<String, String>,
}

impl Qsd {
  /// Insert into the main map, replacing any key that differs only in case.
  fn insert(&mut self, key: String, value: String) {
    insert_ci(&mut self.qsd, &mut self.order, key, value);
  }
}

/// Insert `key`, replacing an existing key that matches it
/// case-insensitively. The replaced key keeps its place in `order`.
fn insert_ci(map: &mut HashMap<String, String>, order: &mut Vec<String>, key: String, value: String) {
  if !map.contains_key(&key) {
    let lower = key.to_lowercase();
    if let Some(old) = map.keys().find(|k| k.to_lowercase() == lower).cloned() {
      map.remove(&old);
      if let Some(slot) = order.iter_mut().find(|k| **k == old) {
        *slot = key.clone();
      }
    } else {
      order.push(key.clone());
    }
  }
  map.insert(key, value);
}

impl ParsedUrl {
  /// Parse an Apprise-style URL (upstream `URLBase.parse_url(url,
  /// verify_host=False)`). Returns `None` when the URL has no schema or
  /// cannot be parsed at all, such as an authority with an unmatched `[`.
  pub fn parse(raw: &str) -> Option<Self> {
    Self::parse_with(raw, ParseOptions::default())
  }

  /// Parse with explicit upstream `parse_url()` options.
  ///
  /// Like upstream `url_to_dict()`, a `#` right after a slash is read as a
  /// literal character (`/#channel`), not as the start of a fragment.
  pub fn parse_with(raw: &str, opts: ParseOptions) -> Option<Self> {
    let parts = parse_url_parts(&escape_path_hash(raw), opts)?;
    Some(Self::from_parts(parts, raw))
  }

  /// Apply upstream `URLBase.post_process_parse_url_results()` and the
  /// unquoting `URLBase.__init__` performs.
  fn from_parts(parts: RawParts, raw: &str) -> Self {
    let RawParts { schema, user: url_user, password: url_password, host, port, bad_port, fullpath, qsd } = parts;

    let mut user = url_user.clone();
    let mut password = url_password;

    if let Some(v) = qsd_get_ci(&qsd.qsd, "password") {
      password = Some(v.to_string());
    }
    if let Some(v) = qsd_get_ci(&qsd.qsd, "pass") {
      password = Some(v.to_string());
    }
    if let Some(v) = qsd_get_ci(&qsd.qsd, "user") {
      user = Some(v.to_string());
      // With ?user= set, the user written in the URL becomes the password
      // (mailtos://PASSWORD@host?user=admin@example.com). Upstream re-parses
      // the URL with host verification to read it.
      if password.is_none() && !bad_port && verify_hostname(&host, true, true, true).is_some() {
        password = url_user;
      }
    }

    let mut port = port.and_then(|p| u16::try_from(p).ok());
    if let Some(v) = qsd_get_ci(&qsd.qsd, "port") {
      port = match py_int(v).map(u16::try_from) {
        Some(Ok(p)) => Some(p),
        _ => {
          if !v.is_empty() {
            tracing::warn!("Invalid port number specified {}", v);
          }
          None
        }
      };
    }

    let nonempty = |s: Option<String>| s.map(|s| unquote(&s)).filter(|s| !s.is_empty());
    let fullpath = fullpath.unwrap_or_default();

    ParsedUrl {
      schema,
      user: nonempty(user),
      password: nonempty(password),
      host_encoded: Some(host.clone()).filter(|h| !h.is_empty()),
      host: nonempty(Some(host)),
      port,
      path: fullpath.strip_prefix('/').unwrap_or(&fullpath).to_string(),
      path_parts: split_path(&fullpath),
      qsd: qsd.qsd,
      qsd_order: qsd.order,
      qsd_add: qsd.add,
      qsd_del: qsd.del,
      qsd_colon: qsd.colon,
      assigned_tags: None,
      raw: raw.to_string(),
    }
  }

  /// The host checked the way upstream's default `verify_host=True` does:
  /// a valid hostname or IP address, or `None`. IPv6 addresses come back in
  /// square brackets.
  pub fn verified_host(&self) -> Option<String> {
    let host = self.host.as_deref()?;
    // Upstream checks the host as written, before decoding it, so an
    // escaped character (`local%68ost`) is refused. A host set after
    // parsing (from YAML keys, say) has no written form to check.
    if let Some(written) = self.host_encoded.as_deref().filter(|w| unquote(w) == host) {
      verify_hostname(written, true, true, true)?;
    }
    verify_hostname(host, true, true, true)
  }

  /// The encoded path with its leading slash, `/` when the URL has none
  /// (upstream `URLBase.fullpath`).
  pub fn fullpath(&self) -> String {
    format!("/{}", self.path)
  }

  /// Recompute `path_parts` after `path` was changed.
  pub fn refresh_path_parts(&mut self) {
    let fullpath = format!("/{}", self.path);
    self.path_parts = split_path(&fullpath);
  }

  /// Get query string value by key (case-insensitive)
  pub fn get(&self, key: &str) -> Option<&str> {
    qsd_get_ci(&self.qsd, key)
  }

  /// The query arguments in URL order. Keys added to `qsd` directly,
  /// without [`ParsedUrl::set_arg`], come last in sorted order.
  pub fn qsd_ordered(&self) -> Vec<(&str, &str)> {
    let mut out: Vec<(&str, &str)> = self.qsd_order.iter().filter_map(|k| self.qsd.get_key_value(k)).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut rest: Vec<(&str, &str)> = self.qsd.iter().filter(|(k, _)| !self.qsd_order.contains(k)).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    rest.sort();
    out.extend(rest);
    out
  }

  /// Set a query argument the way a later `&key=value` in the URL would:
  /// it replaces a key that differs only in case and keeps that key's
  /// place. A key starting with `+`, `-` or `:` also updates the matching
  /// prefix map.
  pub fn set_arg(&mut self, key: &str, value: &str) {
    insert_ci(&mut self.qsd, &mut self.qsd_order, key.to_string(), value.to_string());
    if let Some(rest) = key.strip_prefix([' ', '+']) {
      self.qsd_add.insert(rest.to_string(), value.to_string());
    }
    if let Some(rest) = key.strip_prefix('-') {
      self.qsd_del.insert(rest.to_string(), value.to_string());
    }
    if let Some(rest) = key.strip_prefix(':') {
      self.qsd_colon.insert(rest.to_string(), value.to_string());
    }
  }

  /// Remove a query argument (case-insensitive).
  pub fn remove_arg(&mut self, key: &str) {
    let lower = key.to_lowercase();
    self.qsd.retain(|k, _| k.to_lowercase() != lower);
    self.qsd_order.retain(|k| k.to_lowercase() != lower);
  }

  /// Get the host, or return a default
  pub fn host_or<'a>(&'a self, default: &'a str) -> &'a str {
    self.host.as_deref().unwrap_or(default)
  }

  /// Build http(s) URL from parts
  pub fn base_url(&self, secure: bool) -> String {
    let schema = if secure { "https" } else { "http" };
    match (&self.host, self.port) {
      (Some(h), Some(p)) => format!("{}://{}:{}", schema, h, p),
      (Some(h), None) => format!("{}://{}", schema, h),
      _ => format!("{}://localhost", schema),
    }
  }

  /// Determine if this URL uses a secure variant (ends with 's' convention)
  pub fn is_secure(&self) -> bool {
    self.schema.ends_with('s')
  }

  /// Tags for the service built from this URL.
  ///
  /// These are the [`ParsedUrl::assigned_tags`] set by the loader. Upstream
  /// never reads tags from the URL itself (`?tag=`/`?tags=` belong to the
  /// plugin, ntfy for one). A `ParsedUrl` no loader touched (a plugin unit
  /// test) falls back to `?tag=`, then `?tags=`.
  pub fn tags(&self) -> Vec<String> {
    if let Some(tags) = &self.assigned_tags {
      return tags.clone();
    }
    self.get("tag").or_else(|| self.get("tags")).map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
  }

  /// Get "verify" ssl flag (default true). Any value is read with
  /// [`parse_bool`], so `?verify=` with an unrecognised value turns
  /// verification off, as upstream does.
  pub fn verify_certificate(&self) -> bool {
    self.get("verify").map(parse_bool).unwrap_or(true)
  }
}

/// Upstream `url_to_dict()` swaps `/#` for `/%23` before parsing so
/// channel names such as `/#general` survive.
pub fn escape_path_hash(url: &str) -> std::borrow::Cow<'_, str> {
  if url.contains("/#") { std::borrow::Cow::Owned(url.replace("/#", "/%23")) } else { std::borrow::Cow::Borrowed(url) }
}

/// Case-insensitive lookup that prefers an exact key match.
fn qsd_get_ci<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
  if let Some(v) = map.get(key) {
    return Some(v.as_str());
  }
  let lower = key.to_lowercase();
  map.iter().find(|(k, _)| k.to_lowercase() == lower).map(|(_, v)| v.as_str())
}

/// Upstream `URLBase.split_path()`: split on slashes (and whitespace,
/// commas or backslashes, which only appear encoded in a parsed path) and
/// decode each non-empty segment.
pub fn split_path(path: &str) -> Vec<String> {
  path.trim_start_matches('/').split([' ', '\t', '\r', '\n', ',', '\\', '/']).filter(|s| !s.is_empty()).map(unquote).collect()
}

/// Python `str.strip()` whitespace (Unicode whitespace plus the ASCII
/// separators `\x1c`-`\x1f`).
fn is_py_space(c: char) -> bool {
  c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn py_strip(s: &str) -> &str {
  s.trim_matches(is_py_space)
}

/// Upstream `VALID_URL_RE`:
/// `^\s*((?P<schema>[^:\s]+):[/\\]+)?((?P<path>[^?]+)(\?(?P<kwargs>.+))?)?\s*$`.
/// Returns `(schema, path, kwargs)`, or `None` when the expression does not
/// match.
fn split_valid_url(url: &str) -> Option<(Option<&str>, &str, Option<&str>)> {
  let t = url.trim_start_matches(is_py_space);

  // Try the optional schema group first, as the regex does.
  if let Some(colon) = t.find(':') {
    let cand = &t[..colon];
    if !cand.is_empty() && !cand.chars().any(is_py_space) {
      let after = &t[colon + 1..];
      let slashes = after.chars().take_while(|c| *c == '/' || *c == '\\').count();
      if slashes > 0 {
        let rest = &after[slashes..];
        if let Some(found) = split_path_kwargs(rest) {
          return Some((Some(cand), found.0, found.1));
        }
        // `[/\\]+` gives back its last slash so the path group can start
        // with it (json://?x=1 has the path "/").
        if slashes > 1
          && let Some(found) = split_path_kwargs(&after[slashes - 1..])
        {
          return Some((Some(cand), found.0, found.1));
        }
      }
    }
  }

  // Without a schema the whole string is the path group.
  split_path_kwargs(t).map(|(p, k)| (None, p, k))
}

/// The `((?P<path>[^?]+)(\?(?P<kwargs>.+))?)?\s*$` tail of `VALID_URL_RE`.
fn split_path_kwargs(rest: &str) -> Option<(&str, Option<&str>)> {
  match rest.find('?') {
    None => Some((rest, None)),
    Some(0) => {
      // Nothing for the path group; only trailing whitespace may remain.
      None
    }
    Some(q) => {
      let path = &rest[..q];
      let tail = &rest[q + 1..];
      // `.` does not match a newline; anything after one must be whitespace.
      let (kwargs, after) = match tail.find('\n') {
        Some(nl) => (&tail[..nl], &tail[nl..]),
        None => (tail, ""),
      };
      if kwargs.is_empty() || !after.chars().all(is_py_space) {
        return None;
      }
      Some((path, Some(kwargs)))
    }
  }
}

/// Upstream `parse_url()` without the `URLBase` post-processing.
fn parse_url_parts(url: &str, opts: ParseOptions) -> Option<RawParts> {
  let (schema, host_and_path, kwargs) = split_valid_url(url)?;
  // The Rust port only parses URLs that name their schema.
  let schema = py_strip(schema?).to_lowercase();
  let host_and_path = py_strip(host_and_path);

  let qsd = match kwargs.map(py_strip) {
    Some(k) if !k.is_empty() => parse_qsd(k, opts.plus_to_space),
    _ => Qsd::default(),
  };

  let (netloc, path) = urlsplit_authority(host_and_path)?;
  let mut host = py_strip(&netloc).to_string();

  let mut fullpath = tidy_path(py_strip(&path)).split('/').map(|seg| quote(&unquote(seg), URL_SEGMENT_SAFE_CHARS)).collect::<Vec<_>>().join("/");

  // Restore a trailing slash tidy_path() removed.
  let stripped = py_strip(url);
  if let (Some(last_full), Some(last_url)) = (fullpath.chars().last(), url.chars().last())
    && !matches!(last_full, '/' | '\\')
    && matches!(last_url, '/' | '\\')
    && let Some(c) = stripped.chars().last()
  {
    fullpath.push(c);
  }

  let fullpath = if fullpath.is_empty() { None } else { Some(fullpath) };

  // user@host; runs of '@' split, and only the first two pieces are kept.
  let mut user = None;
  let mut password = None;
  let pieces: Vec<&str> = split_runs(&host, '@');
  if pieces.len() >= 2 {
    user = Some(pieces[0].to_string());
    host = pieces[1].to_string();
  }
  if let Some(u) = user.clone() {
    let pieces: Vec<&str> = split_runs(&u, ':');
    if pieces.len() >= 2 {
      user = Some(pieces[0].to_string());
      password = Some(pieces[1].to_string());
    }
  }

  // Port parsing: ^(?P<host>(\[[0-9a-f:]+\]|[^:]+)):(?P<port>[^:]*)$
  let mut port = None;
  let mut bad_port = false;
  if let Some((h, p)) = split_host_port(&host) {
    match py_int(p) {
      Some(n) => {
        port = Some(n);
        host = h.to_string();
      }
      None => {
        if opts.verify_host {
          return None;
        }
        // Not a number: it stays part of the host.
        bad_port = true;
      }
    }
  }

  if opts.verify_host {
    host = verify_hostname(&host, true, true, true)?;
    if opts.strict_port
      && let Some(p) = port
      && !(1..=65535).contains(&p)
    {
      return None;
    }
  }

  // An out-of-range port can not be represented; upstream would only fail
  // later, when connecting.
  if let Some(p) = port
    && u16::try_from(p).is_err()
  {
    return None;
  }

  Some(RawParts { schema, user, password, host, port, bad_port, fullpath, qsd })
}

/// `re.split(r"[c]+", s)`.
fn split_runs(s: &str, c: char) -> Vec<&str> {
  let mut out = Vec::new();
  let mut start = 0;
  let mut chars = s.char_indices().peekable();
  while let Some((i, ch)) = chars.next() {
    if ch == c {
      out.push(&s[start..i]);
      let mut end = i + ch.len_utf8();
      while let Some(&(j, next)) = chars.peek() {
        if next != c {
          break;
        }
        end = j + next.len_utf8();
        chars.next();
      }
      start = end;
    }
  }
  out.push(&s[start..]);
  out
}

/// Match `^(?P<host>(\[[0-9a-f:]+\]|[^:]+)):(?P<port>[^:]*)$`.
fn split_host_port(host: &str) -> Option<(&str, &str)> {
  if host.starts_with('[')
    && let Some(close) = host.find(']')
  {
    let inner = &host[1..close];
    let after = &host[close + 1..];
    if !inner.is_empty()
      && inner.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == ':')
      && let Some(p) = after.strip_prefix(':')
      && !p.contains(':')
    {
      return Some((&host[..=close], p));
    }
  }
  let colon = host.find(':')?;
  let port = &host[colon + 1..];
  if colon == 0 || port.contains(':') {
    return None;
  }
  Some((&host[..colon], port))
}

/// Python `int(text)` restricted to what a port can look like: surrounding
/// whitespace, an optional sign and digits with single underscores between
/// them. Upstream only tries this when the text holds a digit.
fn py_int(text: &str) -> Option<i64> {
  if !text.chars().any(|c| c.is_ascii_digit()) {
    return None;
  }
  let t = py_strip(text);
  let (neg, digits) = match t.as_bytes().first() {
    Some(b'-') => (true, &t[1..]),
    Some(b'+') => (false, &t[1..]),
    _ => (false, t),
  };
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
    return None;
  }
  if !digits.chars().all(|c| c.is_ascii_digit() || c == '_') {
    return None;
  }
  let n: i64 = digits.replace('_', "").parse().ok()?;
  Some(if neg { -n } else { n })
}

/// The part of Python's `urlparse("http://" + text)` upstream reads: the
/// netloc and the path. Returns `None` where Python raises `ValueError`
/// (an unmatched or misplaced square bracket).
fn urlsplit_authority(text: &str) -> Option<(String, String)> {
  // Python strips these anywhere in a URL before splitting it.
  let cleaned: String = text.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();

  let end = cleaned.find(['/', '?', '#']).unwrap_or(cleaned.len());
  let netloc = &cleaned[..end];
  let mut path = &cleaned[end..];

  let open = netloc.contains('[');
  let close = netloc.contains(']');
  if open != close {
    return None;
  }
  if open && !bracketed_netloc_ok(netloc) {
    return None;
  }

  if let Some(i) = path.find('#') {
    path = &path[..i];
  }
  // The query string was split off earlier; this mirrors urlsplit anyway.
  if let Some(i) = path.find('?') {
    path = &path[..i];
  }
  // urlparse() moves ";params" out of the last path segment.
  if path.contains(';') {
    let cut = match path.rfind('/') {
      Some(slash) => path[slash..].find(';').map(|i| slash + i),
      None => path.find(';'),
    };
    if let Some(i) = cut {
      path = &path[..i];
    }
  }

  Some((netloc.to_string(), path.to_string()))
}

/// Python's `_check_bracketed_netloc()`.
fn bracketed_netloc_ok(netloc: &str) -> bool {
  let host_port = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
  let hostname = match host_port.split_once('[') {
    Some((before, bracketed)) => {
      if !before.is_empty() {
        return false;
      }
      let (hostname, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
      if !port.is_empty() && !port.starts_with(':') {
        return false;
      }
      hostname
    }
    None => host_port.split_once(':').map(|(h, _)| h).unwrap_or(host_port),
  };

  if let Some(rest) = hostname.strip_prefix('v') {
    // \Av[a-fA-F0-9]+\..+\Z
    return match rest.split_once('.') {
      Some((hex, tail)) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) && !tail.is_empty(),
      None => false,
    };
  }

  // ipaddress.ip_address(); an IPv4 address may not be bracketed.
  let (addr, scope) = match hostname.split_once('%') {
    Some((a, s)) => (a, Some(s)),
    None => (hostname, None),
  };
  if let Some(s) = scope
    && (s.is_empty() || s.contains('%'))
  {
    return false;
  }
  addr.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Upstream `tidy_path()` for a URL path: collapse repeated separators and
/// trim trailing backslashes.
fn tidy_path(path: &str) -> String {
  // TIDY_WIN_PATH_RE: a run of backslashes after a non-space character (or
  // at the very start, keeping two) collapses to one.
  let mut out = String::with_capacity(path.len());
  let chars: Vec<char> = path.chars().collect();
  let mut i = 0;
  while i < chars.len() {
    let c = chars[i];
    if c == '\\' {
      let mut j = i;
      while j < chars.len() && chars[j] == '\\' {
        j += 1;
      }
      let run = j - i;
      if i == 0 && run > 2 {
        out.push_str("\\\\");
      } else if i > 0 && run > 1 && !is_py_space(chars[i - 1]) {
        out.push('\\');
      } else {
        out.extend(std::iter::repeat_n('\\', run));
      }
      i = j;
      continue;
    }
    out.push(c);
    i += 1;
  }

  // TIDY_NUX_PATH_RE: collapse runs of '/'.
  let mut nux = String::with_capacity(out.len());
  for c in out.chars() {
    if c == '/' && nux.ends_with('/') {
      continue;
    }
    nux.push(c);
  }

  // TIDY_WIN_TRIM_RE: ^(.+[^:][^\\])[\s\\]*$
  let kept = nux.trim_end_matches(|c: char| c == '\\' || is_py_space(c));
  let kept_chars: Vec<char> = kept.chars().collect();
  if kept.len() < nux.len() && kept_chars.len() >= 3 {
    let n = kept_chars.len();
    if kept_chars[n - 2] != ':' && kept_chars[n - 1] != '\\' {
      return kept.to_string();
    }
  }
  nux
}

/// Upstream `parse_qsd()`: split on `&` and `;`, decode keys and values.
///
/// A key keeps a leading `+` but any later `+` becomes a space; values keep
/// `+` unless `plus_to_space` is set. Keys starting with `+` (or a space),
/// `-` or `:` are also stored, without that character and with their case
/// kept, in the `add`, `del` and `colon` maps.
pub fn parse_qsd(qs: &str, plus_to_space: bool) -> Qsd {
  let mut result = Qsd::default();
  for pair in qs.split(['&', ';']) {
    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));

    let mut key_chars = k.chars();
    let key = match key_chars.next() {
      Some(first) => format!("{}{}", first, key_chars.as_str().replace('+', " ")),
      None => String::new(),
    };
    let key = unquote(&key);

    let val = if plus_to_space { v.replace('+', " ") } else { v.to_string() };
    let val = py_strip(&unquote(&val)).to_string();

    result.insert(py_strip(&key).to_string(), val.clone());

    if let Some(rest) = key.strip_prefix([' ', '+']) {
      result.add.insert(rest.to_string(), val.clone());
    }
    if let Some(rest) = key.strip_prefix('-') {
      result.del.insert(rest.to_string(), val.clone());
    }
    if let Some(rest) = key.strip_prefix(':') {
      result.colon.insert(rest.to_string(), val);
    }
  }
  result
}

/// Python `urllib.parse.unquote()`: decode `%XX` escapes as UTF-8, with
/// invalid sequences replaced by U+FFFD. A `%` not followed by two hex
/// digits stays literal.
pub fn unquote(s: &str) -> String {
  if !s.contains('%') {
    return s.to_string();
  }
  let mut out = String::with_capacity(s.len());
  let mut buf: Vec<u8> = Vec::new();
  let bytes = s.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    if b >= 0x80 {
      // Non-ASCII text passes through and ends the current escape run.
      out.push_str(&String::from_utf8_lossy(&buf));
      buf.clear();
      let ch = s[i..].chars().next().unwrap();
      out.push(ch);
      i += ch.len_utf8();
      continue;
    }
    if b == b'%' && i + 2 < bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit() {
      let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
      buf.push(u8::from_str_radix(hex, 16).unwrap());
      i += 3;
      continue;
    }
    buf.push(b);
    i += 1;
  }
  out.push_str(&String::from_utf8_lossy(&buf));
  out
}

/// Python `urllib.parse.quote(s, safe)`: percent-encode everything except
/// ASCII letters, digits, `_.-~` and the characters in `safe`.
pub fn quote(s: &str, safe: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for ch in s.chars() {
    if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-' | '~') || (ch.is_ascii() && safe.contains(ch)) {
      out.push(ch);
    } else {
      let mut buf = [0u8; 4];
      for b in ch.encode_utf8(&mut buf).bytes() {
        out.push_str(&format!("%{:02X}", b));
      }
    }
  }
  out
}

/// Upstream `parse_urls()`: split a string holding several URLs separated
/// by commas, semicolons and/or whitespace.
///
/// A delimiter only splits when another `schema://` follows it, so commas
/// and semicolons inside a URL (`?to=a,b`) are kept. A string without any
/// URL is split on every delimiter instead, so the caller can still report
/// each piece.
pub fn parse_urls(text: &str) -> Vec<String> {
  let found = if text.contains("://") { find_urls(text.trim_end_matches(is_py_space)) } else { Vec::new() };
  if !found.is_empty() {
    return found;
  }
  text.split(is_list_delimiter).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

/// Delimiters of upstream `STRING_DELIMITERS` (`[\[\]\;,\s]+`).
fn is_list_delimiter(c: char) -> bool {
  matches!(c, '[' | ']' | ';' | ',') || is_py_space(c)
}

/// Delimiters between URLs in `URL_DETECTION_RE` (`[\s,;]`).
fn is_url_delimiter(c: char) -> bool {
  matches!(c, ',' | ';') || is_py_space(c)
}

/// `re.findall(r"([a-z0-9]+?:\/\/.*?)(?=$|[\s,;]{1,32}[a-z0-9]{1,32}?:\/\/)", text, re.I)`.
fn find_urls(text: &str) -> Vec<String> {
  let chars: Vec<char> = text.chars().collect();
  let n = chars.len();
  let is_schema_char = |c: char| c.is_ascii_alphanumeric();

  // Length of the schema run at `i` when it is followed by "://".
  let schema_at = |i: usize, max: usize| -> Option<usize> {
    let mut j = i;
    while j < n && is_schema_char(chars[j]) {
      j += 1;
    }
    let len = j - i;
    (len > 0 && len <= max && j + 3 <= n && chars[j] == ':' && chars[j + 1] == '/' && chars[j + 2] == '/').then_some(len)
  };

  // The lookahead: end of text, or 1-32 delimiters and another schema.
  let boundary = |e: usize| -> bool {
    if e == n {
      return true;
    }
    let mut d = e;
    while d < n && is_url_delimiter(chars[d]) {
      d += 1;
    }
    let run = d - e;
    (1..=32).contains(&run) && schema_at(d, 32).is_some()
  };

  let mut out = Vec::new();
  let mut pos = 0;
  while pos < n {
    let Some(len) = schema_at(pos, usize::MAX) else {
      pos += 1;
      continue;
    };
    // Body: `.*?` up to the first boundary; `.` never crosses a newline.
    let mut e = pos + len + 3;
    let mut matched = None;
    loop {
      if boundary(e) {
        matched = Some(e);
        break;
      }
      if e >= n || chars[e] == '\n' {
        break;
      }
      e += 1;
    }
    match matched {
      Some(end) => {
        out.push(chars[pos..end].iter().collect());
        pos = if end > pos { end } else { pos + 1 };
      }
      None => pos += 1,
    }
  }
  out
}

/// Upstream `parse_list()`: split on commas, semicolons, square brackets
/// and whitespace, then return the unique entries sorted.
pub fn parse_list(text: &str) -> Vec<String> {
  let mut items: Vec<String> = text.split(is_list_delimiter).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
  items.sort();
  items.dedup();
  items
}

/// Parse a boolean the way upstream `parse_bool(value, default=False)`
/// does: only the first two characters count, so `yes`, `yeah` and `y`
/// are true, `no`, `never` and `n` are false. Anything unrecognised is
/// false.
pub fn parse_bool(s: &str) -> bool {
  parse_bool_or(s, false)
}

/// [`parse_bool`] with the result for unrecognised text supplied.
pub fn parse_bool_or(s: &str, default: bool) -> bool {
  let lower = s.to_lowercase();
  let prefix: String = lower.chars().take(2).collect();
  match prefix.as_str() {
    "de" | "di" | "ne" | "f" | "n" | "no" | "of" | "0" | "fa" => false,
    "en" | "al" | "t" | "y" | "ye" | "on" | "1" | "tr" => true,
    _ => default,
  }
}

/// Try to parse multiple space/newline-separated URLs from a string
pub fn extract_urls(text: &str) -> Vec<String> {
  text
    .lines()
    .flat_map(|line| {
      let trimmed = line.trim();
      if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
        vec![]
      } else {
        // Remove inline comments
        let without_comment = trimmed.find(" #").or_else(|| trimmed.find(" ;")).map(|i| &trimmed[..i]).unwrap_or(trimmed);
        without_comment.split_whitespace().filter(|s| s.contains("://")).map(|s| s.to_string()).collect()
      }
    })
    .collect()
}

/// Parse targets from path parts + optional query key
pub fn parse_targets(path_parts: &[String], qsd: &HashMap<String, String>) -> Vec<String> {
  let mut targets: Vec<String> = path_parts.to_vec();
  if let Some(t) = qsd.get("to") {
    targets.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
  }
  targets
}

/// Mask sensitive parts of a URL for safe logging (CWE-312 compliant).
/// Replaces passwords with `***` and masks query params containing
/// "key", "token", "secret", or "password" in their names.
pub fn mask_url(raw: &str) -> String {
  // Apprise URLs use custom schemes; prepend http:// so url::Url can parse
  let (prefix, parse_input) = if raw.contains("://") {
    let scheme_end = raw.find("://").unwrap();
    let scheme = &raw[..scheme_end];
    // url::Url only accepts known schemes; swap temporarily
    (Some(scheme.to_string()), format!("http://{}", &raw[scheme_end + 3..]))
  } else {
    (None, raw.to_string())
  };

  let Ok(mut url) = Url::parse(&parse_input) else { return raw.to_string() };

  if url.password().is_some() {
    let _ = url.set_password(Some("***"));
  }

  let sensitive = ["key", "token", "secret", "password", "pass", "apikey"];
  let pairs: Vec<(String, String)> = url
    .query_pairs()
    .map(|(k, v)| {
      let kl = k.to_lowercase();
      if sensitive.iter().any(|s| kl.contains(s)) { (k.into_owned(), "***".to_string()) } else { (k.into_owned(), v.into_owned()) }
    })
    .collect();

  if !pairs.is_empty() {
    url.set_query(None);
    {
      let mut qs = url.query_pairs_mut();
      for (k, v) in &pairs {
        qs.append_pair(k, v);
      }
    }
  }

  let result = url.to_string();
  // Restore original scheme
  if let Some(scheme) = prefix { format!("{}://{}", scheme, &result["http://".len()..]) } else { result }
}

/// Normalize phone number (strip non-digit characters except leading +)
pub fn normalize_phone(phone: &str) -> String {
  let phone = phone.trim();
  if let Some(stripped) = phone.strip_prefix('+') {
    format!("+{}", stripped.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
  } else {
    phone.chars().filter(|c| c.is_ascii_digit()).collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::Value;

  fn parity() -> Value {
    serde_json::from_str(include_str!("testdata/parse_parity.json")).unwrap()
  }

  fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(unquote).filter(|s| !s.is_empty())
  }

  fn str_map(v: &Value) -> HashMap<String, String> {
    v.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect()
  }

  fn str_vec(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
  }

  /// Replays upstream `URLBase.parse_url(url, verify_host=False)` results.
  #[test]
  fn test_parse_url_parity() {
    for case in parity()["parse_url"].as_array().unwrap() {
      let url = case["url"].as_str().unwrap();
      let got = ParsedUrl::parse(url);
      let expected = &case["loose"];

      // Upstream keeps ports a u16 can not hold (-92, 70000); the port is
      // useless either way, so the Rust parser rejects the URL.
      let port = match &expected["port"] {
        Value::Number(n) => Some(n.as_i64().unwrap()),
        Value::String(s) => py_int(s),
        _ => None,
      };
      let unrepresentable = port.is_some_and(|p| u16::try_from(p).is_err()) && !expected["port"].is_string();
      // Without a schema upstream falls back to a default one.
      let schemaless = !url.contains(':');

      if expected.is_null() || unrepresentable || schemaless {
        assert!(got.is_none(), "{:?} should not parse, got {:?}", url, got);
        continue;
      }
      let got = got.unwrap_or_else(|| panic!("{:?} should parse", url));

      assert_eq!(got.schema, expected["schema"].as_str().unwrap(), "schema of {:?}", url);
      assert_eq!(got.user, opt_str(&expected["user"]), "user of {:?}", url);
      assert_eq!(got.password, opt_str(&expected["password"]), "password of {:?}", url);
      assert_eq!(got.host, opt_str(&expected["host"]), "host of {:?}", url);
      assert_eq!(got.port, port.and_then(|p| u16::try_from(p).ok()), "port of {:?}", url);
      let fullpath = expected["fullpath"].as_str().unwrap_or("/");
      assert_eq!(got.fullpath(), fullpath, "fullpath of {:?}", url);
      assert_eq!(got.path_parts, split_path(fullpath), "path_parts of {:?}", url);

      let lowered: HashMap<String, String> = got.qsd.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
      assert_eq!(lowered, str_map(&expected["qsd"]), "qsd of {:?}", url);
      assert_eq!(got.qsd_add.clone().into_iter().collect::<HashMap<_, _>>(), str_map(&expected["qsd+"]), "qsd+ of {:?}", url);
      assert_eq!(got.qsd_del.clone().into_iter().collect::<HashMap<_, _>>(), str_map(&expected["qsd-"]), "qsd- of {:?}", url);
      assert_eq!(got.qsd_colon.clone().into_iter().collect::<HashMap<_, _>>(), str_map(&expected["qsd:"]), "qsd: of {:?}", url);

      // Upstream dicts keep insertion order; so must we.
      let order = &expected["order"];
      let keys: Vec<String> = got.qsd_ordered().iter().map(|(k, _)| k.to_lowercase()).collect();
      assert_eq!(keys, str_vec(&order["qsd"]), "qsd order of {:?}", url);
      assert_eq!(got.qsd_add.keys().cloned().collect::<Vec<_>>(), str_vec(&order["qsd+"]), "qsd+ order of {:?}", url);
      assert_eq!(got.qsd_del.keys().cloned().collect::<Vec<_>>(), str_vec(&order["qsd-"]), "qsd- order of {:?}", url);
      assert_eq!(got.qsd_colon.keys().cloned().collect::<Vec<_>>(), str_vec(&order["qsd:"]), "qsd: order of {:?}", url);
    }
  }

  /// Replays upstream `URLBase.parse_url(url)` (host verification on).
  #[test]
  fn test_parse_url_verify_host_parity() {
    let opts = ParseOptions { verify_host: true, ..Default::default() };
    for case in parity()["parse_url"].as_array().unwrap() {
      let url = case["url"].as_str().unwrap();
      // Ports a u16 can not hold are rejected (see test_parse_url_parity).
      let bad_port = case["loose"]["port"].as_i64().is_some_and(|p| u16::try_from(p).is_err());
      if !url.contains(':') || bad_port {
        continue;
      }
      let got = ParsedUrl::parse_with(url, opts);
      if case["strict_ok"].as_bool().unwrap() {
        let got = got.unwrap_or_else(|| panic!("{:?} should parse with verify_host", url));
        assert_eq!(got.host, opt_str(&case["strict_host"]), "host of {:?}", url);
      } else {
        assert!(got.is_none(), "{:?} should not parse with verify_host, got {:?}", url, got);
      }
    }
  }

  #[test]
  fn test_parse_urls_parity() {
    for case in parity()["parse_urls"].as_array().unwrap() {
      let input = case["input"].as_str().unwrap();
      let expected: Vec<String> = case["output"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
      assert_eq!(parse_urls(input), expected, "parse_urls({:?})", input);
    }
  }

  #[test]
  fn test_parse_list_parity() {
    for case in parity()["parse_list"].as_array().unwrap() {
      let input = case["input"].as_str().unwrap();
      let expected: Vec<String> = case["output"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
      assert_eq!(parse_list(input), expected, "parse_list({:?})", input);
    }
  }

  #[test]
  fn test_parse_bool_parity() {
    for case in parity()["parse_bool"].as_array().unwrap() {
      let input = case["input"].as_str().unwrap();
      assert_eq!(parse_bool_or(input, false), case["default_false"].as_bool().unwrap(), "parse_bool({:?}, False)", input);
      assert_eq!(parse_bool_or(input, true), case["default_true"].as_bool().unwrap(), "parse_bool({:?}, True)", input);
      assert_eq!(parse_bool(input), case["default_false"].as_bool().unwrap());
    }
  }

  // ── ParsedUrl basic tests ──────────────────────────────────────

  #[test]
  fn test_parse_basic_http() {
    let p = ParsedUrl::parse("http://hostname").unwrap();
    assert_eq!(p.schema, "http");
    assert_eq!(p.host.as_deref(), Some("hostname"));
    assert_eq!(p.port, None);
    assert_eq!(p.user, None);
    assert_eq!(p.password, None);
    assert!(p.qsd.is_empty());
    assert_eq!(p.fullpath(), "/");
  }

  #[test]
  fn test_parse_with_user_password_port_path() {
    let p = ParsedUrl::parse("http://us%40er:p%40ss@hostname:8080/path/to/resource/").unwrap();
    assert_eq!(p.user.as_deref(), Some("us@er"));
    assert_eq!(p.password.as_deref(), Some("p@ss"));
    assert_eq!(p.host.as_deref(), Some("hostname"));
    assert_eq!(p.port, Some(8080));
    assert_eq!(p.path, "path/to/resource/");
    assert_eq!(p.path_parts, vec!["path", "to", "resource"]);
  }

  #[test]
  fn test_parse_token_host_keeps_colon() {
    // A port that is not a number stays part of the host (telegram tokens).
    let p = ParsedUrl::parse("tgram://123456789:abcdefg_hijklmnop/lead2gold/").unwrap();
    assert_eq!(p.host.as_deref(), Some("123456789:abcdefg_hijklmnop"));
    assert_eq!(p.port, None);
    assert_eq!(p.path_parts, vec!["lead2gold"]);
    assert!(p.verified_host().is_none());
  }

  #[test]
  fn test_parse_encoded_slash_stays_in_segment() {
    // upstream 8b89b08b: an escaped slash is part of its segment
    let p = ParsedUrl::parse("json://localhost/a%2Fb/c?x=1").unwrap();
    assert_eq!(p.path, "a%2Fb/c");
    assert_eq!(p.path_parts, vec!["a/b", "c"]);
  }

  #[test]
  fn test_parse_rfc3986_path_characters() {
    // upstream 4b85ec86: sub-delims stay literal, commas and semicolons encoded
    let p = ParsedUrl::parse("json://localhost/npm/pkg@1.2.3/a,b;c/x'y(z)*+=$!&").unwrap();
    assert_eq!(p.path, "npm/pkg@1.2.3/a%2Cb%3Bc/x'y(z)*+=$!&");
  }

  #[test]
  fn test_parse_malformed_authority() {
    // upstream 6ebddd3a: Markdown link syntax must not raise
    assert!(ParsedUrl::parse("**bold** and [link](https://example.com)").is_none());
    assert!(ParsedUrl::parse("json://[unclosed/bracket").is_none());
    assert!(ParsedUrl::parse("json://closed]/bracket").is_none());
  }

  #[test]
  fn test_parse_ipv6_host() {
    let p = ParsedUrl::parse("http://[2001:db8::1]:8080/path").unwrap();
    assert_eq!(p.host.as_deref(), Some("[2001:db8::1]"));
    assert_eq!(p.port, Some(8080));
    assert_eq!(p.verified_host().as_deref(), Some("[2001:db8::1]"));
  }

  #[test]
  fn test_verified_host_checks_the_written_host() {
    // Upstream's verify_host runs before decoding: an escape is refused
    // even when it decodes to a valid name, but an IPv6 zone keeps its %
    for url in ["gotify://local%68ost/token", "json://%31%32%37.0.0.1/"] {
      assert!(ParsedUrl::parse(url).unwrap().verified_host().is_none(), "{}", url);
    }
    assert_eq!(ParsedUrl::parse("json://localhost/").unwrap().verified_host().as_deref(), Some("localhost"));
    assert!(ParsedUrl::parse("json://[fe80::1%25eth0]/").unwrap().verified_host().is_some());
    // A host set after parsing has no written form to check
    let mut p = ParsedUrl::parse("json://local%68ost/").unwrap();
    p.host = Some("example.com".into());
    assert_eq!(p.verified_host().as_deref(), Some("example.com"));
  }

  #[test]
  fn test_parse_none_for_invalid() {
    assert!(ParsedUrl::parse("").is_none());
    assert!(ParsedUrl::parse("notaurl").is_none());
    assert!(ParsedUrl::parse("://missing_schema").is_none());
    assert!(ParsedUrl::parse("json://host?").is_none());
  }

  #[test]
  fn test_parse_no_redos() {
    // upstream test_parse_url_path_no_redos
    let segment = "%40%3A%21%24%26%27%28%29%2A%2B%2C%3B%3D-a".repeat(5000);
    let url = format!("https://example.com/{}/file.txt?foo=bar", segment);
    let start = std::time::Instant::now();
    assert!(ParsedUrl::parse(&url).is_some());
    assert!(start.elapsed().as_secs() < 5);

    let start = std::time::Instant::now();
    assert_eq!(parse_urls(&format!("https://example.com{}", " ".repeat(50000))), vec!["https://example.com"]);
    assert!(start.elapsed().as_secs() < 5);
  }

  // ── Query string ───────────────────────────────────────────────

  #[test]
  fn test_get_case_insensitive() {
    let p = ParsedUrl::parse("http://host?MyKey=val").unwrap();
    assert_eq!(p.get("MyKey"), Some("val"));
    assert_eq!(p.get("mykey"), Some("val"));
    assert_eq!(p.get("MYKEY"), Some("val"));
  }

  #[test]
  fn test_plus_in_query_value_is_kept() {
    // Upstream keeps '+' (tokens, passwords and phone numbers use it).
    let p = ParsedUrl::parse("sinch://u:p@host/?to=+15551234567&from=+1%20555").unwrap();
    assert_eq!(p.get("to"), Some("+15551234567"));
    assert_eq!(p.get("from"), Some("+1 555"));
    // Only office365 asks for form decoding
    let p = ParsedUrl::parse_with("o365://host/?name=a+b", ParseOptions { plus_to_space: true, ..Default::default() }).unwrap();
    assert_eq!(p.get("name"), Some("a b"));
  }

  #[test]
  fn test_query_values_are_trimmed() {
    let p = ParsedUrl::parse("json://host/?a=%20%20padded%20&b=%09tab%0A&c=  inner  space  ").unwrap();
    assert_eq!(p.get("a"), Some("padded"));
    assert_eq!(p.get("b"), Some("tab"));
    assert_eq!(p.get("c"), Some("inner  space"));
  }

  #[test]
  fn test_custom_schema_host_is_decoded() {
    let p = ParsedUrl::parse("tgram://123456789%3Aabc%2Fdef/chat").unwrap();
    assert_eq!(p.host.as_deref(), Some("123456789:abc/def"));
    let p = ParsedUrl::parse("pover://u%40ser@tok%20en/").unwrap();
    assert_eq!(p.user.as_deref(), Some("u@ser"));
    assert_eq!(p.host.as_deref(), Some("tok en"));
  }

  #[test]
  fn test_qsd_keeps_url_order() {
    // WhatsApp template components and custom JSON/XML payload keys rely on it
    let p = ParsedUrl::parse("whatsapp://t@id/?:2=second&:1=first&z=1&a=2&Z=3;m=4").unwrap();
    let keys: Vec<&str> = p.qsd_colon.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["2", "1"]);
    let order: Vec<(&str, &str)> = p.qsd_ordered();
    assert_eq!(order, vec![(":2", "second"), (":1", "first"), ("Z", "3"), ("a", "2"), ("m", "4")]);

    let mut p = p;
    p.set_arg("A", "9");
    p.set_arg("new", "x");
    p.set_arg("+Header", "h");
    let order: Vec<&str> = p.qsd_ordered().iter().map(|(k, _)| *k).collect();
    assert_eq!(order, vec![":2", ":1", "Z", "A", "m", "new", "+Header"]);
    assert_eq!(p.get("a"), Some("9"));
    assert_eq!(p.qsd_add.get("Header").map(String::as_str), Some("h"));
    p.remove_arg("z");
    assert!(p.get("z").is_none());
    assert_eq!(p.qsd_ordered()[2].0, "A");
  }

  #[test]
  fn test_qsd_splits_on_semicolons() {
    let p = ParsedUrl::parse("json://host/?a=1;b=2&c=3").unwrap();
    assert_eq!(p.get("a"), Some("1"));
    assert_eq!(p.get("b"), Some("2"));
    assert_eq!(p.get("c"), Some("3"));
  }

  #[test]
  fn test_qsd_case_duplicates_keep_last() {
    let p = ParsedUrl::parse("http://host?Format=html&format=text").unwrap();
    assert_eq!(p.qsd.len(), 1);
    assert_eq!(p.get("format"), Some("text"));
  }

  #[test]
  fn test_qsd_prefix_maps() {
    let p = ParsedUrl::parse("xml://host/?+X-Header=abc&-Remove&:Key=value").unwrap();
    assert_eq!(p.qsd_add.get("X-Header").map(String::as_str), Some("abc"));
    assert_eq!(p.qsd_del.get("Remove").map(String::as_str), Some(""));
    assert_eq!(p.qsd_colon.get("Key").map(String::as_str), Some("value"));
    // The prefixed keys stay in qsd as well
    assert_eq!(p.get("+x-header"), Some("abc"));
  }

  #[test]
  fn test_user_password_overrides() {
    let p = ParsedUrl::parse("mailtos://PASSWORD@hostname?user=admin@example.com").unwrap();
    assert_eq!(p.user.as_deref(), Some("admin@example.com"));
    assert_eq!(p.password.as_deref(), Some("PASSWORD"));

    let p = ParsedUrl::parse("mailtos://u@h?pass=secret").unwrap();
    assert_eq!(p.password.as_deref(), Some("secret"));

    let p = ParsedUrl::parse("json://h:80?port=8080").unwrap();
    assert_eq!(p.port, Some(8080));
    let p = ParsedUrl::parse("json://h:80?port=abc").unwrap();
    assert_eq!(p.port, None);
  }

  #[test]
  fn test_tags_assigned_and_fallback() {
    let mut p = ParsedUrl::parse("ntfy://host/topic?tags=emoji&tag=a,b").unwrap();
    // Without a loader the legacy ?tag= fallback applies
    assert_eq!(p.tags(), vec!["a", "b"]);
    // A loader always decides the tags; ?tag=/?tags= stay for the plugin
    p.assigned_tags = Some(vec!["2:endpoint".into()]);
    assert_eq!(p.tags(), vec!["2:endpoint"]);
    p.assigned_tags = Some(vec![]);
    assert!(p.tags().is_empty());
    assert_eq!(p.get("tags"), Some("emoji"));
  }

  #[test]
  fn test_verify_certificate() {
    // Upstream post-processing reads ?verify= with parse_bool(value), whose
    // default is False, so an unrecognised value turns verification off.
    assert!(ParsedUrl::parse("http://host").unwrap().verify_certificate());
    for v in ["no", "false", "0", "off", "random", "garbage", "active", ""] {
      assert!(!ParsedUrl::parse(&format!("http://host?verify={}", v)).unwrap().verify_certificate(), "{}", v);
    }
    for v in ["yes", "always", "yeah", "true", "1", "on"] {
      assert!(ParsedUrl::parse(&format!("http://host?verify={}", v)).unwrap().verify_certificate(), "{}", v);
    }
  }

  // ── base_url / is_secure / host_or ─────────────────────────────

  #[test]
  fn test_base_url() {
    let p = ParsedUrl::parse("slack://host:443/path").unwrap();
    assert_eq!(p.base_url(true), "https://host:443");
    assert_eq!(p.base_url(false), "http://host:443");
    assert!(ParsedUrl::parse("https://example.com").unwrap().is_secure());
    assert_eq!(ParsedUrl::parse("http://myhost").unwrap().host_or("fallback"), "myhost");
  }

  // ── unquote / quote ────────────────────────────────────────────

  #[test]
  fn test_unquote_quote() {
    assert_eq!(unquote("a%20b%2Fc"), "a b/c");
    assert_eq!(unquote("%zz%4"), "%zz%4");
    assert_eq!(unquote("%E2%82%AC"), "€");
    assert_eq!(unquote("%ff"), "\u{fffd}");
    assert_eq!(quote("a b/c,é", URL_SEGMENT_SAFE_CHARS), "a%20b%2Fc%2C%C3%A9");
    assert_eq!(quote("a/b~_.-", "/"), "a/b~_.-");
  }

  // ── parse_urls (upstream f3e50141) ─────────────────────────────

  #[test]
  fn test_parse_urls_semicolon_delimiter() {
    for text in ["json://a/;json://b/", "json://a/ ; json://b/", "json://a/; json://b/", "json://a/,;  json://b/", "json://a/ ,; json://b/"] {
      assert_eq!(parse_urls(text), vec!["json://a/", "json://b/"], "{}", text);
    }
    for text in ["json://a/?x=1;2", "json://u:p;w@h/", "json://a/;b", "json://a/?url=x;y,z"] {
      assert_eq!(parse_urls(text), vec![text]);
    }
    assert_eq!(parse_urls("json://a/?x=1;2;json://u:p;w@h/"), vec!["json://a/?x=1;2", "json://u:p;w@h/"]);
  }

  // ── extract_urls ───────────────────────────────────────────────

  #[test]
  fn test_extract_urls() {
    assert_eq!(extract_urls("json://localhost\nxml://localhost"), vec!["json://localhost", "xml://localhost"]);
    assert_eq!(extract_urls("# c\njson://localhost # my json\n; c"), vec!["json://localhost"]);
    assert!(extract_urls("").is_empty());
  }

  // ── normalize_phone ────────────────────────────────────────────

  #[test]
  fn test_normalize_phone() {
    assert_eq!(normalize_phone("1234567890"), "1234567890");
    assert_eq!(normalize_phone("+1 (234) 567-8901"), "+12345678901");
    assert_eq!(normalize_phone("  +44 123 456  "), "+44123456");
    assert_eq!(normalize_phone("123-456.7890"), "1234567890");
  }

  // ── mask_url ───────────────────────────────────────────────────

  #[test]
  fn test_mask_url() {
    let masked = mask_url("http://user:secret@host/path");
    assert!(masked.contains("***") && !masked.contains("secret"));
    assert_eq!(mask_url("http://host/path"), "http://host/path");
    let masked = mask_url("http://host/path?apikey=ABC123&normal=ok");
    assert!(!masked.contains("ABC123") && masked.contains("normal=ok"));
    let masked = mask_url("slack://user:pass@host/path");
    assert!(masked.starts_with("slack://") && !masked.contains("pass@"));
    assert_eq!(mask_url("not a url at all"), "not a url at all");
  }

  // ── parse_targets ──────────────────────────────────────────────

  #[test]
  fn test_parse_targets() {
    let mut qsd = HashMap::new();
    assert_eq!(parse_targets(&["a".to_string(), "b".to_string()], &qsd), vec!["a", "b"]);
    qsd.insert("to".to_string(), "c,d".to_string());
    assert_eq!(parse_targets(&["a".to_string()], &qsd), vec!["a", "c", "d"]);
  }
}

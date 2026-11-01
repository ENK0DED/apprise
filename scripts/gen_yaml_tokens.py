#!/usr/bin/env python3
"""Generate where a YAML setting that names a URL token goes, per schema.

Upstream YAML configuration hands each service its settings as constructor
arguments. A setting named after one of the service's URL tokens (Discord's
`botname`, ntfy's `topic`, a `target_phone`) therefore sets what that token
sets, after `ConfigBase._special_token_handler()` follows the token's
`alias_of`/`map_to` to the constructor argument. A token that stands for
several URL values (Slack's `token`) is dropped with a warning.

The port's plugins read their settings from the URL instead, so this script
finds, for each such token, the URL part (`user`, `password`) or the URL
argument that carries the same constructor argument (`?to=` for targets,
`?from=` for a source number), and writes them as a Rust table. Tokens that
are URL arguments themselves are left out: the port already applies a
setting as the URL argument of the same name.

A constructor argument no URL argument carries (Gotify's `token` and
`path`, SMPP's `source` and `targets`) can only come from its place in the
URL. Its place is found by filling upstream's URL templates with marker
values, several lengths for the parts that vary (paths, target lists), and
parsing them with upstream's `parse_url()`: the URL host, user or password,
or path segments counted from the start or the end. The setting then
rewrites that place. Such a rule is only written when upstream's own test
URLs agree: for every valid URL of the plugin in upstream's tests, and two
sample values, the plugin built from the URL plus the setting (what YAML
does) must equal the plugin built from the rewritten URL (what the port's
loader does).

Where no position holds (Office 365's source is the host or `user@host`,
a Slack setting can change the Slack mode), the setting goes to the plugin
as the constructor argument it is upstream: the loader keeps it beside the
URL (`ParsedUrl::kwargs`), and the plugin reads it after parsing the URL,
where upstream's constructor gets it. A setting upstream also reads as the
`?name=` URL argument of its name stays that argument. Every setting
without a position is listed on stderr, with the reason.

Run it with the Python of an environment where upstream Apprise is
installed, from anywhere, giving upstream's `tests` directory:

    python -I scripts/gen_yaml_tokens.py --tests UPSTREAM/tests [OUTPUT]

OUTPUT defaults to crates/apprise-core/src/config/yaml/tokens.rs in this
repository. Rerun it after syncing with a new upstream release.
"""

import argparse
import ast
import collections
import itertools
from pathlib import Path
import re
import sys
from urllib.parse import quote

import apprise
from apprise import plugins
from apprise.manager_plugins import NotificationManager
from apprise.plugins.base import NotifyBase
from apprise.utils.parse import parse_url

DEFAULT_OUTPUT = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "apprise-core"
    / "src"
    / "config"
    / "yaml"
    / "tokens.rs"
)

# URL parts the port's YAML loader already sets from a setting of that name.
PARTS = ("user", "password", "host", "port")

# Template tokens that span several path segments without being a list.
PATH_TOKENS = ("path", "fullpath")

# URL arguments upstream's parse_url() conventionally reads for a
# constructor argument without declaring them in the templates.
QUERY_CARRIERS = {"targets": {"to"}, "source": {"from", "source"}}

split_path = NotifyBase.split_path


def rust_str(value: str) -> str:
    """A Rust string literal for `value`."""
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def destination(templates: dict, key: str):
    """The constructor argument and its type that a YAML `key` sets, as
    `_special_token_handler()` resolves it; `None` for several values."""
    entry = templates["args"].get(key) or templates["tokens"][key]
    map_to = entry.get("alias_of", entry.get("map_to", key))
    if not isinstance(map_to, str):
        return None
    meta = templates["tokens"].get(map_to, templates["args"].get(map_to))
    if not isinstance(meta, dict) or "type" not in meta:
        meta = entry
    return meta.get("map_to", map_to), meta.get("type", "string")


# ── URL positions ────────────────────────────────────────────────────────────


def kind_of(templates: dict, name: str) -> str:
    """`list`, `path` (several segments) or `one` (a single value)."""
    meta = templates["tokens"].get(name, {})
    if re.match(r"^list:", meta.get("type", ""), re.IGNORECASE):
        return "list"
    return "path" if name in PATH_TOKENS else "one"


def fill(schema: str, template: str, templates: dict, lengths: dict):
    """`template` with a marker value per token, and each token's markers;
    `lengths` gives the number of segments of each varying token."""
    markers, out, pos = {}, [], 0
    for m in re.finditer(r"\{(\w+)\}", template):
        out.append(template[pos : m.start()])
        name = m.group(1)
        before = template[m.start() - 1] if m.start() else ""
        after = template[m.end()] if m.end() < len(template) else ""
        kind = kind_of(templates, name)
        n = len(markers)
        if name == "schema":
            text = schema
        elif name == "port":
            text = "4321"
        elif kind in ("list", "path"):
            values = [f"zq{n}{chr(97 + i)}" for i in range(lengths[name])]
            markers[name] = values
            text = "/".join(values)
            if kind == "path":
                # A path glued to its neighbours brings its own slashes.
                text = (
                    ("/" if before not in ("/", "") else "")
                    + text
                    + ("/" if after not in ("/", "") else "")
                )
        elif templates["tokens"].get(name, {}).get("type") == "email":
            # An address brings its own `@` (`{apikey}:{from_email}`).
            text = f"zq{n}u@zq{n}h.example"
            markers[name] = [text]
        else:
            text = f"zq{n}x"
            markers[name] = [text]
        out.append(text)
        pos = m.end()
    out.append(template[pos:])
    return "".join(out), markers


def locate(url: str, values: list):
    """Where the markers `values` landed in the parsed `url`."""
    results = parse_url(url, verify_host=False)
    if not results:
        return None
    for part in ("user", "password", "host"):
        if [str(results.get(part))] == values:
            return ("part", part)
    segments = split_path(results.get("fullpath") or "")
    if all(v in segments for v in values):
        idx = [segments.index(v) for v in values]
        if idx == list(range(idx[0], idx[0] + len(idx))):
            return ("seg", idx[0], idx[0] + len(idx), len(segments))
    return None


def position(schema: str, cls, token: str):
    """The URL position of template `token`, or `(None, reason)`.

    A position is `("part", name)` or `("seg", kind, start, end)`, where
    each bound is `("start", i)` or `("end", i)`: counted from the first
    segment, or back from the end of the path. A bound must hold whatever
    the length of the paths and lists around it, in every template."""
    templates = plugins.details(cls)
    places = []
    for template in getattr(cls, "templates", ()):
        if "{" + token + "}" not in template:
            continue
        varying = [
            n
            for n in re.findall(r"\{(\w+)\}", template)
            if kind_of(templates, n) in ("list", "path")
        ]
        for combo in itertools.product((1, 3), repeat=len(varying)):
            url, markers = fill(
                schema, template, templates, dict(zip(varying, combo))
            )
            place = locate(url, markers[token])
            if place is None:
                return None, f"not found in {template}"
            places.append(place)
    if not places:
        return None, "in no template"
    if all(p[0] == "part" for p in places):
        if len({p[1] for p in places}) == 1:
            return ("part", places[0][1]), None
        return None, "a different URL part in different templates"
    if not all(p[0] == "seg" for p in places):
        return None, "a URL part in some templates, the path in others"

    def bound(index: int):
        if len({p[index] for p in places}) == 1:
            return ("start", places[0][index])
        if len({p[3] - p[index] for p in places}) == 1:
            return ("end", places[0][3] - places[0][index])
        return None

    start, end = bound(1), bound(2)
    if start is None or end is None:
        return None, "segments that move between templates"
    return ("seg", kind_of(templates, token), start, end), None


def resolve(bound, n: int) -> int:
    return bound[1] if bound[0] == "start" else n - bound[1]


def value_at(raw: dict, rule):
    """The value at the rule's position of a parsed URL, as YAML would
    give it, or `None` when the URL has none there."""
    if rule[0] == "part":
        return raw.get(rule[1])
    segments = split_path(raw.get("fullpath") or "")
    _, kind, start, end = rule
    n = len(segments)
    s, e = resolve(start, n), resolve(end, n)
    if s < 0 or e > n or s >= e:
        return None
    span = segments[s:e]
    if kind == "path":
        return "/" + "/".join(span) + ("/" if e == n else "")
    if kind == "list":
        return span
    return span[0] if len(span) == 1 else None


def rewrite(raw: dict, url: str, rule, value, drop: tuple):
    """`url` with `value` at the rule's position and the `drop` arguments
    removed, as the port's YAML loader rewrites it; `None` when the URL
    has no room there."""
    query = [
        q
        for q in (url.split("?", 1)[1].split("&") if "?" in url else [])
        if q and q.split("=", 1)[0].lower() not in drop
    ]
    user, password, host = raw.get("user"), raw.get("password"), raw.get("host")
    fullpath = raw.get("fullpath") or ""
    segments = split_path(fullpath)
    slash = len(fullpath) > 1 and fullpath.endswith("/")
    if rule[0] == "part":
        if rule[1] == "user":
            user = value
        elif rule[1] == "password":
            password = value
        else:
            host = value
    else:
        _, kind, start, end = rule
        n = len(segments)
        s, e = max(resolve(start, n), 0), min(resolve(end, n), n)
        if s > n or s > e:
            return None
        if kind == "path":
            new = [x for x in value.split("/") if x]
            if e == n:
                slash = value.endswith("/")
        else:
            new = list(value) if kind == "list" else [value]
        segments = segments[:s] + new + segments[e:]
    auth = ""
    if user:
        auth = quote(user, safe="")
        auth += (":" + quote(password, safe="") if password else "") + "@"
    elif password:
        auth = ":" + quote(password, safe="") + "@"
    port = f":{raw['port']}" if raw.get("port") else ""
    path = "/".join(quote(x, safe="") for x in segments)
    path += "/" if slash and segments else ""
    out = f"{raw['schema']}://{auth}{host or ''}{port}/{path}"
    return out + ("?" + "&".join(query) if query else "")


def test_urls(tests: Path) -> dict:
    """Valid URLs in upstream's tests, by the plugin class they load."""
    found = collections.defaultdict(list)
    for f in sorted(tests.glob("test_plugin_*.py")):
        tree = ast.parse(f.read_text(encoding="utf-8"))
        # Module constants the URLs are built from (TOKEN = "...").
        names = {}

        def evaluate(node):
            code = compile(ast.Expression(node), str(f), "eval")
            return eval(code, {"__builtins__": {}}, dict(names))

        for stmt in tree.body:
            if (
                isinstance(stmt, ast.Assign)
                and len(stmt.targets) == 1
                and isinstance(stmt.targets[0], ast.Name)
            ):
                try:
                    names[stmt.targets[0].id] = evaluate(stmt.value)
                except Exception:
                    pass
        for node in ast.walk(tree):
            if not (
                isinstance(node, ast.Tuple)
                and len(node.elts) == 2
                and isinstance(node.elts[1], ast.Dict)
            ):
                continue
            meta = {
                k.value: v
                for k, v in zip(node.elts[1].keys, node.elts[1].values)
                if isinstance(k, ast.Constant)
            }
            instance = meta.get("instance")
            if not isinstance(instance, ast.Name):
                continue
            try:
                url = evaluate(node.elts[0])
            except Exception:
                continue
            if isinstance(url, str):
                found[instance.id].append(url)
    return found


def built(cls, kwargs: dict):
    """The plugin `kwargs` build, as its URL, or `None` if refused."""
    try:
        return cls(**kwargs).url(privacy=False)
    except Exception:
        return None


def distinct(values: list) -> list:
    """Up to three different sample values, the last ones found."""
    out = []
    for value in reversed(values):
        if value not in out:
            out.append(value)
    return out[:3]


def rotate(text: str, classes: str) -> str:
    """`text` with each digit (`d`) and/or letter (`a`) moved one place on
    within its class, so a value keeps the shape plugins check."""
    out = []
    for c in text:
        if "d" in classes and c.isdigit():
            c = chr(48 + (ord(c) - 47) % 10)
        elif "a" in classes and c.isascii() and c.isalpha():
            base = 65 if c.isupper() else 97
            c = chr(base + (ord(c) - base + 1) % 26)
        out.append(c)
    return "".join(out)


def mutants(value) -> list:
    """Values of the same shape as `value` but different from it."""
    out = []
    for classes in ("d", "a", "da"):
        if isinstance(value, list):
            other = [rotate(v, classes) for v in value]
        else:
            other = rotate(value, classes)
        if other != value and other not in out:
            out.append(other)
    return out


def check(cls, key: str, target: str, rule, urls: list, drop: tuple):
    """Against upstream's test URLs: how many agree when the value is
    written at the rule's position, how many when it is given as
    `?key=`, and out of how many.

    A path is tried with and without a trailing slash, and the better
    spelling counts: upstream joins a path setting to what follows as
    written, so each plugin works with one of them (Gotify `/a/`,
    Mattermost `/a`), where the rewritten URL works with both."""
    if rule[0] == "seg" and rule[1] == "path":
        return max(
            (check_values(cls, key, target, rule, urls, drop, tail) for tail in ("/", "")),
            key=lambda r: (r[0] == r[2], r[0]),
        )
    return check_values(cls, key, target, rule, urls, drop, None)


def raw_parse(url: str):
    """Upstream `parse_url()` as plugins get it: `url_to_dict()` keeps a
    `/#channel` in the path."""
    return parse_url(url.replace("/#", "/%23"), verify_host=False)


def check_values(cls, key, target, rule, urls, drop, tail):
    """`check()` for one spelling of the values: `tail` is the trailing
    slash a path value gets, `None` to keep them as found."""
    samples = []
    for url in urls:
        url = url.replace("/#", "/%23")
        raw = raw_parse(url)
        results = cls.parse_url(url)
        if raw and results:
            samples.append((url, raw, results))
    values = [v for v in (value_at(raw, rule) for _, raw, _ in samples) if v]
    placed = arg = total = 0
    pairs = [
        (sample, value)
        for sample in samples
        for value in distinct(values) + mutants(value_at(sample[1], rule) or "")
    ]
    for (url, raw, results), value in pairs:
        if rule[0] != "part" and value_at(raw, rule) is None:
            continue
        if tail is not None and isinstance(value, str):
            value = value.rstrip("/") + tail
        rewritten = rewrite(raw, url, rule, value, drop)
        expected = built(cls, {**results, target: value})
        # Only a setting that changes the plugin tells anything.
        if rewritten is None or expected in (None, built(cls, results)):
            continue
        total += 1
        if built(cls, cls.parse_url(rewritten) or {}) == expected:
            placed += 1
        text = ",".join(value) if isinstance(value, list) else value
        given = url + ("&" if "?" in url else "?") + f"{key}={quote(text, safe='')}"
        if built(cls, cls.parse_url(given) or {}) == expected:
            arg += 1
    return placed, arg, total


def check_arg(cls, key: str, target: str, urls: list):
    """For a setting with no URL position: how many of upstream's test URLs
    give the plugin a YAML setting gives when the value is passed as
    `?key=`, out of how many. Sample values are what the test URLs set."""
    samples = [(url, cls.parse_url(url)) for url in urls]
    samples = [(url, r) for url, r in samples if r]
    values = []
    for _, results in samples:
        try:
            value = getattr(cls(**results), target, None)
        except Exception:
            continue
        if isinstance(value, str) and value:
            values.append(value)
    arg = total = 0
    pairs = []
    for url, results in samples:
        try:
            own = getattr(cls(**results), target, None)
        except Exception:
            own = None
        own = mutants(own) if isinstance(own, str) and own else []
        pairs += [((url, results), v) for v in distinct(values) + own]
    for (url, results), value in pairs:
        expected = built(cls, {**results, target: value})
        if expected in (None, built(cls, results)):
            continue
        total += 1
        given = url + ("&" if "?" in url else "?") + f"{key}={quote(value, safe='')}"
        if built(cls, cls.parse_url(given) or {}) == expected:
            arg += 1
    return arg, total


def rust_bound(bound) -> str:
    return f"{bound[0].capitalize()}({bound[1]})"


# ── Output ───────────────────────────────────────────────────────────────────


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--tests", type=Path, required=True)
    parser.add_argument("output", type=Path, nargs="?", default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    manager = NotificationManager()
    samples = test_urls(args.tests)

    # (key, destination) -> schemas
    rows = collections.defaultdict(list)
    # (class, key) -> [(schema, constructor argument, template token)] that
    # only a URL position can carry
    unplaced = collections.defaultdict(list)
    for schema in sorted(manager.schemas()):
        cls = manager[schema]
        templates = plugins.details(cls)
        # constructor argument -> the URL arguments that carry it
        carriers = collections.defaultdict(list)
        for arg in templates["args"]:
            found = destination(templates, arg)
            if found:
                carriers[found[0]].append(arg)

        for key in sorted(set(templates["tokens"]) | set(templates["args"])):
            found = destination(templates, key)
            if found is None:
                rows[(key, "UrlOnly")].append(schema)
                continue
            if key in templates["args"] or key in PARTS or key == "schema":
                continue
            target, kind = found
            if target in PARTS:
                rows[(key, f"Part({rust_str(target)})")].append(schema)
                continue
            args_ = [a for a in carriers.get(target, []) if a != key]
            if not args_:
                # The template token that fills the same argument.
                tokens = [
                    t
                    for t in templates["tokens"]
                    if t not in templates["args"]
                    and (destination(templates, t) or (None,))[0] == target
                    and any("{" + t + "}" in x for x in cls.templates)
                ]
                unplaced[(cls, key)].append((schema, target, tokens))
                continue
            arg = "to" if "to" in args_ else sorted(args_)[0]
            variant = "List" if re.match(r"^list:", kind, re.IGNORECASE) else "Arg"
            rows[(key, f"{variant}({rust_str(arg)})")].append(schema)

    # Settings a URL position carries, checked against upstream's tests.
    # The others go to the plugin beside the URL: (class, key, constructor
    # argument, schemas, why no position, `?key=` agreement or None).
    checked = collections.defaultdict(list)
    failed = set()
    report = []
    beside = []
    for (cls, key), entries in sorted(
        unplaced.items(), key=lambda e: (e[0][0].__name__, e[0][1])
    ):
        schemas = [s for s, _, _ in entries]
        target, tokens = entries[0][1], entries[0][2]
        if not tokens:
            beside.append((cls, key, target, schemas, "in no URL template", None))
            continue
        rule, why = position(schemas[0], cls, tokens[0])
        if rule is None:
            beside.append((cls, key, target, schemas, why, None))
            continue
        # A setting wins over the URL: the `?name=` of each setting for
        # this argument, and the arguments upstream's parse_url() reads
        # for targets and a source, no longer count.
        keys = {k for (c, k), e in unplaced.items() if c is cls and e[0][1] == target}
        keys |= QUERY_CARRIERS.get(target, set())
        drop = tuple(sorted(keys))
        placed, arg, total = check(cls, key, target, rule, samples.get(cls.__name__, []), drop)
        if total == 0:
            beside.append((cls, key, target, schemas, f"no test URL to check {rule} against", None))
            continue
        if placed == total:
            checked[cls].append((key, target, rule, drop, schemas))
            continue
        failed.add(cls)
        beside.append((cls, key, target, schemas, f"{rule} holds for {placed}/{total} test URLs", (arg, total)))

    # A plugin where one position does not hold has URL forms its
    # templates do not show (Slack's workflow URLs); trust none there.
    positions = collections.defaultdict(set)
    for cls, entries in checked.items():
        for key, target, rule, drop, schemas in entries:
            if cls in failed:
                why = f"{rule} holds, but another position of {cls.__name__} does not"
                beside.append((cls, key, target, schemas, why, None))
                continue
            positions[(target, rule, drop)].update(schemas)
            for schema in schemas:
                rows[(key, f"At({rust_str(target)})")].append(schema)

    # Without a position, a setting upstream also reads as the URL argument
    # of its name stays that argument (the default); the others are handed
    # to the plugin as the constructor argument, as upstream does.
    for cls, key, target, schemas, why, agreed in sorted(beside, key=lambda b: (b[0].__name__, b[1])):
        label = f"{'/'.join(schemas)} {key}"
        arg, total = agreed or check_arg(cls, key, target, samples.get(cls.__name__, []))
        in_query = any(re.search(r"[?&]" + re.escape(key) + r"=", t) for t in cls.templates)
        if in_query or (total and arg == total):
            report.append(f"  {label}: {why}; read as ?{key}= upstream ({arg}/{total})")
            continue
        report.append(f"  {label}: {why}; ?{key}= upstream: {arg}/{total}; passed beside the URL")
        for schema in schemas:
            rows[(key, f"Kwarg({rust_str(target)})")].append(schema)

    lines = [
        "//! Where a YAML setting that names one of a service's URL tokens",
        "//! goes, keyed by schema (upstream `_special_token_handler()`).",
        "//!",
        f"//! Generated from Apprise {apprise.__version__} by",
        "//! `scripts/gen_yaml_tokens.py`; do not edit by hand.",
        "",
        "/// What a YAML setting named after a URL token sets.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum UrlToken {",
        "  /// The URL argument that carries the same value.",
        "  Arg(&'static str),",
        "  /// The URL argument that carries the same list; several settings",
        "  /// that name it add to it.",
        "  List(&'static str),",
        "  /// This part of the URL.",
        "  Part(&'static str),",
        "  /// The place in the URL of this constructor argument, which no URL",
        "  /// argument carries ([`url_position`]).",
        "  At(&'static str),",
        "  /// This constructor argument, which neither a URL argument nor one",
        "  /// place in the URL carries: the plugin gets it beside the URL",
        "  /// (`ParsedUrl::kwargs`).",
        "  Kwarg(&'static str),",
        "  /// Several URL values: only the URL can set them.",
        "  UrlOnly,",
        "}",
        "",
        "/// One end of a run of path segments.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum Bound {",
        "  /// Counted from the first segment.",
        "  Start(usize),",
        "  /// Counted back from the end of the path.",
        "  End(usize),",
        "}",
        "",
        "/// What a setting replaces in the URL path.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum Segments {",
        "  /// One segment, the value.",
        "  One,",
        "  /// Segments of a path, the value split on `/`.",
        "  Path,",
        "  /// One segment per value of a list.",
        "  List,",
        "}",
        "",
        "/// Where a constructor argument sits in the URL.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum Place {",
        "  /// This part of the URL.",
        "  Part(&'static str),",
        "  /// The path segments from `start` to `end` (exclusive).",
        "  Path { start: Bound, end: Bound, kind: Segments },",
        "}",
        "",
        "/// Where a constructor argument sits in a URL, and the URL arguments",
        "/// a setting for it overrides.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct UrlPosition {",
        "  pub place: Place,",
        "  pub overrides: &'static [&'static str],",
        "}",
        "",
        "/// What the YAML setting `key` of a `schema` service (lowercase) sets,",
        "/// or `None` for a setting applied as the URL argument of its name.",
        "#[rustfmt::skip]",
        "pub fn url_token(schema: &str, key: &str) -> Option<UrlToken> {",
        "  use UrlToken::*;",
        "  Some(match (key, schema) {",
    ]
    for (key, dest), schemas in sorted(rows.items()):
        names = " | ".join(rust_str(s) for s in sorted(schemas))
        lines.append(f"    ({rust_str(key)}, {names}) => {dest},")
    lines += [
        "    _ => return None,",
        "  })",
        "}",
        "",
        "/// Where the constructor argument `name` of a `schema` service sits in",
        "/// its URL ([`UrlToken::At`]).",
        "#[rustfmt::skip]",
        "pub fn url_position(schema: &str, name: &str) -> Option<UrlPosition> {",
        "  use Bound::*;",
        "  Some(match (name, schema) {",
    ]
    for (target, rule, drop), schemas in sorted(positions.items(), key=lambda e: (e[0][0], sorted(e[1]))):
        names = " | ".join(rust_str(s) for s in sorted(schemas))
        if rule[0] == "part":
            place = f"Place::Part({rust_str(rule[1])})"
        else:
            kind = {"one": "One", "path": "Path", "list": "List"}[rule[1]]
            place = (
                f"Place::Path {{ start: {rust_bound(rule[2])},"
                f" end: {rust_bound(rule[3])}, kind: Segments::{kind} }}"
            )
        overrides = ", ".join(rust_str(d) for d in drop)
        lines.append(
            f"    ({rust_str(target)}, {names}) => UrlPosition {{ place: {place},"
            f" overrides: &[{overrides}] }},"
        )
    lines += ["    _ => return None,", "  })", "}", ""]

    args.output.write_text("\n".join(lines), encoding="utf-8")
    print(
        f"wrote {len(rows)} rules for"
        f" {sum(len(s) for s in rows.values())} settings"
        f" ({len(positions)} URL positions) to {args.output}",
        file=sys.stderr,
    )
    if report:
        print("settings with no rule:", file=sys.stderr)
        print("\n".join(report), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())

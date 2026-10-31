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
setting as the URL argument of the same name. So are tokens no URL argument
carries; only the URL can set those.

Run it with the Python of an environment where upstream Apprise is
installed, from anywhere:

    python -I scripts/gen_yaml_tokens.py [OUTPUT]

OUTPUT defaults to crates/apprise-core/src/config/yaml/tokens.rs in this
repository. Rerun it after syncing with a new upstream release.
"""

import collections
import re
import sys
from pathlib import Path

import apprise
from apprise import plugins
from apprise.manager_plugins import NotificationManager

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


def main() -> int:
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_OUTPUT
    manager = NotificationManager()

    # (key, destination) -> schemas
    rows = collections.defaultdict(list)
    for schema in sorted(manager.schemas()):
        templates = plugins.details(manager[schema])
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
            if key in templates["args"] or key in PARTS:
                continue
            target, kind = found
            if target in PARTS:
                rows[(key, f"Part({rust_str(target)})")].append(schema)
                continue
            args = [a for a in carriers.get(target, []) if a != key]
            if not args:
                continue
            arg = "to" if "to" in args else sorted(args)[0]
            variant = "List" if re.match(r"^list:", kind, re.IGNORECASE) else "Arg"
            rows[(key, f"{variant}({rust_str(arg)})")].append(schema)

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
        "  /// Several URL values: only the URL can set them.",
        "  UrlOnly,",
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
        names = " | ".join(rust_str(s) for s in schemas)
        lines.append(f"    ({rust_str(key)}, {names}) => {dest},")
    lines += ["    _ => return None,", "  })", "}", ""]

    output.write_text("\n".join(lines), encoding="utf-8")
    print(
        f"wrote {len(rows)} rules for"
        f" {sum(len(s) for s in rows.values())} settings to {output}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

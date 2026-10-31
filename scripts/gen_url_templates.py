#!/usr/bin/env python3
"""Generate the URL templates `apprise --details` prints for each service.

Upstream Apprise describes every service with the URL forms it accepts
(`Apprise().details()["schemas"][n]["details"]["templates"]`); its CLI prints
them under each service. This script reads them from an installed upstream
Apprise and writes them as a Rust table keyed by schema, so the port can
print the same guidance without hand-copying them.

Run it with the Python of an environment where upstream Apprise is
installed, from anywhere:

    python -I scripts/gen_url_templates.py [OUTPUT]

OUTPUT defaults to crates/apprise-cli/src/url_templates.rs in this
repository. Rerun it after syncing with a new upstream release.
"""

import sys
from pathlib import Path

import apprise

DEFAULT_OUTPUT = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "apprise-cli"
    / "src"
    / "url_templates.rs"
)


def rust_str(value: str) -> str:
    """A Rust string literal for `value`."""
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def schemas(entry: dict) -> list:
    """Every schema of a service, in upstream's order (insecure first)."""
    out = []
    for key in ("protocols", "secure_protocols"):
        out.extend(p for p in (entry.get(key) or ()) if isinstance(p, str))
    return out


def main() -> int:
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_OUTPUT
    details = apprise.Apprise().details(
        show_requirements=True, show_disabled=True
    )

    seen = {}
    rows = []
    for entry in details["schemas"]:
        names = []
        for schema in schemas(entry):
            schema = schema.lower()
            if schema in seen:
                # A schema can only lead to one list; keep the first
                print(
                    f"warning: {schema}:// is claimed by both"
                    f" {seen[schema]} and {entry['service_name']}",
                    file=sys.stderr,
                )
                continue
            seen[schema] = entry["service_name"]
            names.append(schema)

        templates = list(entry["details"]["templates"])
        if names and templates:
            rows.append((sorted(names), templates))

    rows.sort(key=lambda row: row[0][0])
    lines = [
        "//! URL templates for `apprise --details`, keyed by schema.",
        "//!",
        f"//! Generated from Apprise {apprise.__version__} by",
        "//! `scripts/gen_url_templates.py`; do not edit by hand.",
        "",
        "/// The URL forms upstream documents for the service behind"
        " `schema`",
        "/// (lowercase), as upstream's `details()` lists them.",
        "#[rustfmt::skip]",
        "pub fn url_templates(schema: &str) -> &'static [&'static str] {",
        "  match schema {",
    ]
    for names, templates in rows:
        lines.append(f"    {' | '.join(rust_str(n) for n in names)} => &[")
        lines.extend(f"      {rust_str(t)}," for t in templates)
        lines.append("    ],")
    lines += ["    _ => &[],", "  }", "}", ""]

    output.write_text("\n".join(lines), encoding="utf-8")
    print(
        f"wrote {len(rows)} services, {len(seen)} schemas to {output}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

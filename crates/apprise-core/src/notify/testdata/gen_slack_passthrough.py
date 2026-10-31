"""Record Slack's legacy-API text for bodies with no input format.

Usage:
  python -I gen_slack_passthrough.py OUT.json [COUNT [SEED]]

Sends COUNT random bodies (default 30000) through upstream's
`Apprise().notify(body_format=None)` to a legacy Slack webhook with
`?format=markdown`, where upstream escapes `&`, `<` and `>` and turns
channel, user and URL links back into links.

Each body is sent twice: through upstream as it is, then with the port's
deliberate fix applied to upstream's code, which is what `text` records.
Upstream's `re.sub(re.escape(match), link, body, flags=re.IGNORECASE)`
reads the link as a replacement template (backslashes are escapes, `\\d`
or `\\1` fail the send) and also replaces copies in other cases. The port
puts the link in as written and replaces exact copies only.

Writes a sample for `notify/slack.rs` as `[{"body", "text"}]`, plus
`"upstream"` where upstream sent something else: 250 bodies that had
links restored, 100 where upstream differs, up to 20 that both fail on
(they raise) and 50 that were only escaped. `text` and `upstream` are the
attachment text posted, or null when the notification failed. Requests
are patched; nothing leaves the machine.
"""

import json
import random
import re
import sys

import requests

FRAGMENTS = [
    "<", ">", "&", "&lt;", "&gt;", "&amp;", "&LT;", "!", "@", "|", " ", "  ",
    "\t", "\n", "\r", "*", "\\", "\\\\", "\\g", "\\n", "\\d", "\\0", "\\1",
    "\\12", "\\101", "\\7", "\\&", "<0>", "g<0>", "<!", "<@", "&lt;!", "&lt;@",
    "http://", "https://", "mailto://", "HTTP://", "http\u017f://",
    "ma\u0130lto://", "\u017f", "\u0130", "\u0131", "\u212a", "\u03c3",
    "\u03c2", "\u2003", "a", "A", "x", "X", "here", "HERE", "Here", "channel",
    "chan", "U123", "u123", "x.com", "/", "?", "=", ":", "b=1", "desc", "Desc",
    " | ",
]


class LiteralRe:
    """`re` for upstream's Slack module with the port's fix: its only
    `re.sub()` calls put a link in place of each exact copy of a match."""

    def __getattr__(self, name):
        return getattr(re, name)

    @staticmethod
    def sub(pattern, repl, string, count=0, flags=0):
        assert flags == re.IGNORECASE and isinstance(repl, str)
        return re.sub(pattern, lambda _: repl, string, count=count)


def main():
    out_path = sys.argv[1]
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 30000
    rnd = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 11)

    import apprise
    from apprise.plugins import slack

    posted = []

    def fake_request(self, method, url, data=None, json=None, **kwargs):
        posted.append(json if json is not None else data)
        r = requests.Response()
        r.status_code = 200
        r._content = b"ok"
        r.url = url
        r.request = requests.Request(method, url).prepare()
        return r

    requests.Session.request = fake_request
    # No pause between requests
    apprise.URLBase.throttle = lambda self, *a, **k: None

    a = apprise.Apprise()
    assert a.add("slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/"
                 "?format=markdown&footer=no&image=no")

    def send(body, module_re):
        slack.re = module_re
        posted.clear()
        if not a.notify(body=body, title="", body_format=None):
            return None
        payload = posted[-1]
        if isinstance(payload, (str, bytes)):
            payload = json.loads(payload)
        return payload["attachments"][0]["text"]

    def escaped(body):
        body = body.rstrip()
        return body.replace("&", "&amp;").replace("<", "&lt;") \
            .replace(">", "&gt;")

    groups = {"linked": [], "differs": [], "failed": [], "escaped": []}
    seen = set()
    for _ in range(count):
        body = "".join(rnd.choice(FRAGMENTS)
                       for _ in range(rnd.randint(1, 14)))
        if not body.strip() or body in seen:
            continue
        seen.add(body)
        upstream = send(body, re)
        text = send(body, LiteralRe())
        case = {"body": body, "text": text}
        if upstream != text:
            case["upstream"] = upstream
            group = "differs"
        elif text is None:
            group = "failed"
        else:
            group = "escaped" if text == escaped(body) else "linked"
        groups[group].append(case)

    sample = (groups["linked"][:250] + groups["differs"][:100]
              + groups["failed"][:20] + groups["escaped"][:50])
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(sample, fh, ensure_ascii=False, indent=0)
        fh.write("\n")
    print(", ".join(f"{k}: {len(v)}" for k, v in groups.items()),
          f"-> {len(sample)} written")


if __name__ == "__main__":
    main()

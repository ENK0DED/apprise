"""Record upstream Slack's legacy-API text for bodies with no input format.

Usage:
  python -I gen_slack_passthrough.py OUT.json [COUNT [SEED]]

Sends COUNT random bodies (default 30000) through upstream's
`Apprise().notify(body_format=None)` to a legacy Slack webhook with
`?format=markdown`, where upstream escapes `&`, `<` and `>` and turns
channel, user and URL links back into links. Writes a sample for
`notify/slack.rs`: 250 bodies that had links restored, 100 that upstream
failed on (it raises), and 50 that were only escaped, as
`[{"body", "ok", "text"}]`. `text` is the attachment text upstream posted,
or null when the notification failed. Requests are patched; nothing leaves
the machine.
"""

import json
import random
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


def main():
    out_path = sys.argv[1]
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 30000
    rnd = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 11)

    import apprise

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

    def escaped(body):
        body = body.rstrip()
        return body.replace("&", "&amp;").replace("<", "&lt;") \
            .replace(">", "&gt;")

    groups = {"linked": [], "failed": [], "escaped": []}
    seen = set()
    for _ in range(count):
        body = "".join(rnd.choice(FRAGMENTS)
                       for _ in range(rnd.randint(1, 14)))
        if not body.strip() or body in seen:
            continue
        seen.add(body)
        posted.clear()
        ok = bool(a.notify(body=body, title="", body_format=None))
        text = None
        if ok:
            payload = posted[-1]
            if isinstance(payload, (str, bytes)):
                payload = json.loads(payload)
            text = payload["attachments"][0]["text"]
        group = ("failed" if not ok
                 else "escaped" if text == escaped(body) else "linked")
        groups[group].append({"body": body, "ok": ok, "text": text})

    sample = (groups["linked"][:250] + groups["failed"][:100]
              + groups["escaped"][:50])
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(sample, fh, ensure_ascii=False, indent=0)
        fh.write("\n")
    print(", ".join(f"{k}: {len(v)}" for k, v in groups.items()),
          f"-> {len(sample)} written")


if __name__ == "__main__":
    main()

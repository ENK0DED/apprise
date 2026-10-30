"""Record what upstream Apprise sends to Telegram, for `notify/telegram.rs`.

Usage:
  python -I gen_telegram.py UPSTREAM_TESTS_DIR APPRISE_GO_DIR OUT.json

Writes two groups of vectors:

- `dialect`: `NotifyTelegram.dialect_convert()` inputs and outputs, taken
  from upstream's own Telegram tests (run directly), the apprise-go
  corpora and seeded random documents, for Markdown v1, MarkdownV2 and HTML.
- `send`: full `Apprise.notify()` calls (URL, title, body, input format)
  and every request upstream made for them (endpoint and JSON payload),
  including long bodies split or truncated by `?overflow=`.

Every input format is declared, as the Rust port always declares one.
"""
import inspect, json, os, random, sys
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
STUB = os.path.join(HERE, "..", "..", "conversion", "testdata", "stub")
UPSTREAM_TESTS, APPRISE_GO, OUT = sys.argv[1:4]
sys.path.insert(0, STUB)
sys.path.insert(0, UPSTREAM_TESTS)

import requests
from apprise import Apprise, NotifyFormat
from apprise.plugins.telegram import NotifyTelegram

rng = random.Random(1842)
TOKEN = "123456789:abcdefg_hijklmnop"
OWNER = {"ok": True, "result": [{"update_id": 1, "message": {"message_id": 2, "from": {"id": 532389719, "first_name": "Chris"}}}]}
MAX_INPUT = 6000

# ── dialect vectors ─────────────────────────────────────────────────────────

dialect = {}

# The unpatched conversions.
orig_dialect = NotifyTelegram.dialect_convert
orig_cm = NotifyTelegram._commonmark_to_telegram.__func__
recording = [False]

def add_dialect(mdv, fmt, body):
    # Split searches probe many prefixes; keep the shorter inputs only.
    if not isinstance(body, str) or len(body) > 1500 or recording[0]:
        return
    key = (mdv, fmt, body)
    if key in dialect:
        return
    recording[0] = True
    try:
        obj = NotifyTelegram(bot_token=TOKEN, targets=1, mdv=mdv)
        dialect[key] = orig_dialect(obj, body, NotifyFormat(fmt))
    finally:
        recording[0] = False

# Record every dialect call upstream's own tests make.

def rec_dialect(self, body, body_format=None, *a, **k):
    if body_format is not None:
        mdv = "v2" if self.markdown_ver == "MarkdownV2" else "v1"
        try:
            add_dialect(mdv, NotifyFormat(body_format).value, body)
        except Exception:
            pass
    return orig_dialect(self, body, body_format, *a, **k)

def rec_cm(cls, body, strict=False):
    add_dialect("v2" if strict else "v1", "markdown", body)
    return orig_cm(cls, body, strict=strict)

# Record the Apprise.notify() calls upstream's tests make.
recorded_calls = []
orig_add = Apprise.add
orig_notify = Apprise.notify

def rec_add(self, servers, *a, **k):
    if isinstance(servers, str):
        self.__dict__.setdefault("_rec_urls", []).append(servers)
    return orig_add(self, servers, *a, **k)

def rec_notify(self, body="", title="", notify_type="info", body_format=None, attach=None, **k):
    urls = self.__dict__.get("_rec_urls", [])
    if body_format is not None and attach is None and len(urls) == 1 and urls[0].startswith("tgram://") and isinstance(body, str) and len(body) <= MAX_INPUT:
        recorded_calls.append((urls[0], title or "", body, NotifyFormat(body_format).value))
    return orig_notify(self, body=body, title=title, notify_type=notify_type, body_format=body_format, attach=attach, **k)

NotifyTelegram.dialect_convert = rec_dialect
NotifyTelegram._commonmark_to_telegram = classmethod(rec_cm)
Apprise.add = rec_add
Apprise.notify = rec_notify

import test_plugin_telegram as T
import logging
logging.disable(logging.CRITICAL)
for name, fn in sorted(vars(T).items()):
    if not name.startswith("test_") or not callable(fn):
        continue
    params = [p for p in inspect.signature(fn).parameters]
    if params:
        continue  # mock.patch supplies its own arguments
    try:
        fn()
    except Exception:
        pass
for name, fn in sorted(vars(T).items()):
    if not name.startswith("test_") or not callable(fn):
        continue
    if not inspect.signature(fn).parameters:
        continue
    try:
        fn()
    except Exception:
        pass

print(f"recorded {len(dialect)} dialect calls, {len(recorded_calls)} notify calls", file=sys.stderr)
NotifyTelegram.dialect_convert = orig_dialect
NotifyTelegram._commonmark_to_telegram = classmethod(orig_cm)
Apprise.add = orig_add
Apprise.notify = orig_notify

# apprise-go corpora (Markdown and HTML documents pinned against upstream).
corpus = []
tdir = os.path.join(APPRISE_GO, "internal", "notify", "testdata")
for fname in sorted(os.listdir(tdir)):
    if not fname.endswith(".json"):
        continue
    try:
        data = json.load(open(os.path.join(tdir, fname)))
    except Exception:
        continue
    def walk(v):
        if isinstance(v, dict):
            for key in ("input", "markdown", "html", "body", "md"):
                if isinstance(v.get(key), str):
                    corpus.append(v[key])
            for x in v.values():
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)
    walk(data)
corpus = sorted(set(c for c in corpus if 0 < len(c) <= 1500))
rng.shuffle(corpus)
corpus = corpus[:150]

MD = ["**bold**", "*it*", "_it_", "__u__", "***bi***", "`code`", "``a`b``", "```\nblock\n```", "[l](http://x.io/a_(b))", "[l](<http://x.io/a b>)",
      "<https://a.b/c_d*e>", "# Head", "## Sub *x*", "- item", "1. one", "> quote", "app_data", "snake_case_word", "5 * 3", "a [b", "[ERROR]", "x ]( y",
      "\\*", "\\_", "\\#", "\\\\", "~~s~~", "a.b!c-d", "(p)", "{c}", "a|b", "#tag", "+1", "=", "<b>x</b>", "&amp;", "&", "δέλτα", "🐧", "\n", "\n\n", "  ", "\t"]
HTML = ["<b>b</b>", "<strong>s</strong>", "<i>i</i>", "<em>e</em>", "<u>u</u>", "<s>s</s>", "<del>d</del>", "<tg-spoiler>x</tg-spoiler>",
        "<span class=\"tg-spoiler\">sp</span>", "<span>plain</span>", "<a href=\"http://e.com?a=1&amp;b=2\">l</a>", "<a>n</a>", "<code>c&lt;</code>",
        "<pre><code class=\"language-py\">x = 1\n  y</code></pre>", "<pre>a  \n b</pre>", "<blockquote>q</blockquote>", "<blockquote expandable>e</blockquote>",
        "<h1>H1</h1>", "<h3>H3</h3>", "<title>T</title>", "<p>para</p>", "<div>div</div>", "<br>", "<br/>", "</br>", "<hr>", "<ul><li>a</li><li>b</li></ul>",
        "<ol><li>one</li><li>two</li></ol>", "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>", "<img alt=\"pic\">",
        "<script>bad()</script>", "<font>f</font>", "&nbsp;", "&nbsp", "&emsp;", "&quot", "&apos;", "&lt;x&gt;", "a_b", "*lit*", "#1", "\n", "\n\n", " ", "text"]
TXT = ["plain", "a_b", "*x*", "<b>", "&", "#h", "\\", "[x](y)", "\n", "\t", "  ", "é", "1. no"]

def rand_doc(frags, k):
    return "".join(rng.choice(frags) + rng.choice(["", " ", "", "\n"]) for _ in range(rng.randint(1, k)))

for body in corpus:
    for mdv in ("v1", "v2"):
        add_dialect(mdv, "markdown", body)
    add_dialect("v1", "html", body)
for _ in range(300):
    add_dialect(rng.choice(["v1", "v2"]), "markdown", rand_doc(MD, 14))
for _ in range(250):
    add_dialect("v1", "html", rand_doc(HTML, 14))
for _ in range(20):
    add_dialect("v1", "text", rand_doc(TXT, 6))

# ── send vectors ────────────────────────────────────────────────────────────

def capture(url, title, body, fmt):
    calls = []

    def fake_post(url_, data=None, headers=None, files=None, **kw):
        method = url_.rsplit("/", 1)[-1]
        entry = {"method": method}
        if files:
            entry["form"] = {k: str(v) for k, v in (data or {}).items()}
            entry["files"] = sorted(files)
        else:
            entry["json"] = json.loads(data) if data else None
        calls.append(entry)
        r = requests.Response()
        r.status_code = 200
        r._content = json.dumps(OWNER if method == "getUpdates" else {"ok": True}).encode()
        return r

    with mock.patch("requests.post", fake_post):
        a = Apprise()
        if not a.add(url):
            return None
        ok = a.notify(title=title, body=body, body_format=NotifyFormat(fmt))
    return {"url": url, "title": title, "body": body, "format": fmt, "result": bool(ok), "requests": calls}

send = []
seen = set()

def add_send(url, title, body, fmt):
    key = (url, title, body, fmt)
    if key in seen:
        return
    seen.add(key)
    entry = capture(url, title, body, fmt)
    if entry is not None:
        send.append(entry)

print(f"{len(dialect)} dialect vectors", file=sys.stderr)
for url, title, body, fmt in recorded_calls:
    add_send(url, title, body, fmt)
print(f"{len(send)} recorded sends", file=sys.stderr)

BASE = f"tgram://{TOKEN}/12345"
URLS = [BASE, BASE + "?format=markdown", BASE + "?format=markdown&mdv=v2", BASE + "?format=html", BASE + "?format=text"]
TITLES = ["", "Title", "# Head *x*", "<b>T</b> & _u_", "Alert: disk_full!"]
spacing = json.load(open(os.path.join(HERE, "..", "..", "conversion", "testdata", "telegram_spacing.json")))
for url in URLS:
    for fmt, body in (("markdown", spacing["md"]), ("html", spacing["html"]), ("text", spacing["html_text"])):
        add_send(url, "", body, fmt)
        add_send(url, "Report", body, fmt)
for _ in range(140):
    fmt = rng.choice(["text", "html", "markdown"])
    frags = {"text": TXT + MD[:10], "html": HTML, "markdown": MD}[fmt]
    add_send(rng.choice(URLS), rng.choice(TITLES), rand_doc(frags, 10), fmt)

# Several chats, topics and switches.
for fmt, body in (("text", "a & b"), ("markdown", "**x** y_z"), ("html", "<p>p</p><b>b</b>")):
    add_send(f"tgram://{TOKEN}/12345/@chan:7/-100/l2g?topic=3&silent=yes&preview=yes", "T", body, fmt)
    add_send(f"tgram://{TOKEN}/?to=id1,id2:9&format=markdown&mdv=2", "T", body, fmt)

# Long bodies: split and truncate after dialect conversion.
LONG = [
    ("markdown", "**" + "x" * 4990 + "**TAIL SHOULD NOT BE BOLD"),
    ("markdown", "." * 5000),
    ("markdown", "*_a_ test " + "x" * 5000),
    ("markdown", "Line with snake_case and a [link](http://e.com/a_b) here.\n" * 90),
    ("html", "<h1>Title</h1>" + "<p><strong>bold</strong> <em>word</em></p>" * 200),
    ("html", "<b>" + "x" * 4990 + "</b>TAIL_SHOULD_NOT_BE_BOLD"),
    ("html", "<pre>" + "line.with.dots-and-dashes_under " * 160 + "</pre>"),
    ("text", ("word & <tag> " * 20 + "\n") * 25),
]
print(f"{len(send)} short sends", file=sys.stderr)
long_bodies = []
for fmt, body in LONG:
    long_bodies.append(body)
    urls = [BASE + "?overflow=split", BASE + "?overflow=truncate", BASE + "?format=markdown&mdv=v2&overflow=split", BASE + "?format=markdown&mdv=v2&overflow=truncate"]
    if fmt == "markdown":
        urls += [BASE + "?format=markdown&overflow=split"]
    for url in urls:
        before = len(send)
        add_send(url, "Long", body, fmt)
        for entry in send[before:]:
            entry["body"] = {"long": len(long_bodies) - 1}
add_send(BASE + "?format=markdown&mdv=v2&overflow=upstream", "", "." * 5000, "markdown")

out = {
    "long_bodies": long_bodies,
    "dialect": [{"mdv": m, "format": f, "body": b, "out": o} for (m, f, b), o in sorted(dialect.items())],
    "send": send,
}
with open(OUT, "w") as f:
    json.dump(out, f, ensure_ascii=False, indent=0, sort_keys=True)
print(f"{len(out['dialect'])} dialect vectors, {len(send)} send vectors", file=sys.stderr)

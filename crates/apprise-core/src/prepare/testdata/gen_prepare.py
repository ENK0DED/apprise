"""Record what upstream hands a plugin's send() for many size/format settings.

Usage: python -I gen_prepare.py OUT.json [--passthrough]

--passthrough sends every case with no input format (upstream
`notify(body_format=None)`), so the body passes through unconverted.
"""
import json, random, sys
from apprise import Apprise, AppriseAsset, NotifyFormat
from apprise.common import OverflowMode
from apprise.plugins import NotifyBase

out = sys.argv[1]
passthrough = "--passthrough" in sys.argv[2:]
rng = random.Random(4242)

# Dialects the Rust test plugin reproduces exactly.
DIALECTS = {
    None: None,
    "bold": lambda body, fmt: body.replace("**", "*") if fmt == NotifyFormat.MARKDOWN else body,
    "entities": lambda body, fmt: body.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"),
}

def make_plugin(cfg):
    attrs = {
        "service_name": "Probe", "protocol": "probe", "secure_protocol": "probes",
        "body_maxlen": cfg["body_maxlen"], "title_maxlen": cfg["title_maxlen"],
        "body_max_line_count": cfg["body_max_line_count"],
        "overflow_amalgamate_title": cfg["amalgamate"],
        "notify_format": tuple(NotifyFormat(f) for f in cfg["formats"]) if len(cfg["formats"]) > 1 else NotifyFormat(cfg["formats"][0]),
        "overflow_mode": OverflowMode(cfg["overflow_mode"]),
        "request_rate_per_sec": 0,
    }
    calls = []
    def send(self, body, title="", notify_type=None, **kw):
        calls.append({"title": title, "body": body, "body_format": getattr(kw.get("body_format"), "value", kw.get("body_format"))})
        return True
    def url(self, *a, **k):
        return "probe://"
    attrs["send"] = send
    attrs["url"] = url
    if cfg["dialect"]:
        fn = DIALECTS[cfg["dialect"]]
        attrs["dialect_convert"] = lambda self, body, body_format=None, *a, **k: fn(body, body_format)
    cls = type("Probe", (NotifyBase,), attrs)
    kwargs = {}
    for key in ("format", "overflow", "emojis"):
        if cfg.get(key) is not None:
            kwargs[key] = cfg[key]
    return cls(asset=AppriseAsset(interpret_emojis=cfg["asset_emojis"]), **kwargs), calls

WORDS = ["alpha", "beta", "gamma", "δέλτα", "🐧🐧", "**bold**", "*it*", "`code`", "[l](<https://x.io>)", "<b>b</b>", "&amp;",
         "\n", "\n\n", "  ", "line\r\n", ":smile:", "\\n", "C:\\Users", "end.", "x" * 30, "# head", "- item", "é" * 12]
def rand_text(k):
    return "".join(rng.choice(WORDS) + rng.choice(["", " "]) for _ in range(rng.randint(0, k)))

cases = []
TITLES = ["", "Title", "  # Heading-ish title  ", "Ünïcødé tïtle 🐧", "T" * 40, "x" * 200]
for _ in range(1800):
    formats = rng.choice([["text"], ["html"], ["markdown"], ["text", "html", "markdown"], ["markdown", "text"], ["html", "text"]])
    cfg = {
        "body_maxlen": rng.choice([1, 5, 12, 30, 64, 140, 200, 32768]),
        "title_maxlen": rng.choice([0, 0, 10, 50, 250]),
        "body_max_line_count": rng.choice([0, 0, 0, 1, 2, 10]),
        "amalgamate": rng.choice([False, False, True]),
        "formats": formats,
        "overflow_mode": rng.choice(["upstream", "truncate", "split", "split"]),
        "dialect": rng.choice([None, None, "bold", "entities"]),
        "format": rng.choice([None, None, None, "text", "html", "markdown"]),
        "overflow": rng.choice([None, None, "upstream", "truncate", "split"]),
        "emojis": rng.choice([None, None, True, False]),
        "asset_emojis": rng.choice([None, True]),
        "interpret_escapes": rng.choice([False, False, True]),
        "input_format": rng.choice(["text", "html", "markdown"]),
        "title": rng.choice(TITLES),
        "body": rand_text(rng.choice([3, 10, 40, 120])) or "x",
    }
    if passthrough:
        # Drawn above all the same, so both sets share their other settings.
        cfg["input_format"] = None
    plugin, calls = make_plugin(cfg)
    a = Apprise(asset=AppriseAsset(interpret_emojis=cfg["asset_emojis"]))
    a.add(plugin)
    a.notify(body=cfg["body"], title=cfg["title"], body_format=cfg["input_format"], interpret_escapes=cfg["interpret_escapes"])
    cfg["calls"] = calls
    cases.append(cfg)

with open(out, "w") as f:
    json.dump(cases, f, ensure_ascii=False, indent=0)
print(len(cases), sum(len(c["calls"]) for c in cases), file=sys.stderr)

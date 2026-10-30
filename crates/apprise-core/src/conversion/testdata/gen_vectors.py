"""Build conversion test vectors from upstream Apprise.

Usage: python -I gen_vectors.py recorded.json APPRISE_GO_TESTDATA OUT_FULL OUT_SMALL

Every expected value is computed by upstream Apprise; the Rust tests feed
the same inputs to the port and compare.
"""
import html as pyhtml, json, random, sys
from apprise import NotifyFormat
from apprise.conversion import (
    convert_between, html_to_markdown, html_to_text, markdown_to_html, text_to_html, text_to_markdown,
    commonmark_headings_to_bold, commonmark_decode_backslash_escapes, commonmark_escape_link_url,
    commonmark_repair_chunk, commonmark_sections, commonmark_can_open_emphasis, commonmark_can_close_emphasis,
    commonmark_lookahead_closer_widths, commonmark_pick_emphasis_sentinel, split_dialect_chunk,
    truncate_dialect_chunk, HTMLTagReducer, commonmark_index_backtick_runs, commonmark_match_emphasis,
    commonmark_emphasis_run, commonmark_render_emphasis_markers,
)
from apprise.utils.format import smart_split

recorded, go, out_full, out_small = sys.argv[1:5]
rec = json.load(open(recorded))
rng = random.Random(20261030)

def load(name):
    return json.load(open(f"{go}/{name}"))

md_inputs = [x["Input"] for x in load("anchor_markdown.json")] + [x["input"] for x in load("cliq_markdown.json")]
html_inputs = [x["Input"] for x in load("telegram_html_reduce.json")]
html_inputs += [x["input"] for x in load("format_conversion.json") if x["source"] == "html"]
md_inputs += [x["input"] for x in load("format_conversion.json") if x["source"] == "markdown"]
for r in rec.get("convert_between", []):
    a = r["args"]
    if a["from_format"] == "html":
        html_inputs.append(a["content"])
    elif a["from_format"] == "markdown":
        md_inputs.append(a["content"])
for r in rec.get("commonmark_headings_to_bold", []):
    md_inputs.append(r["args"]["body"])
for r in rec.get("commonmark_repair_chunk", []):
    md_inputs.append(r["args"]["text"])

# Random fragments in the shapes notifications use.
HTML_FRAGS = ["<p>", "</p>", "<b>", "</b>", "<i>", "</i>", "<strong>", "</strong>", "<em>", "</em>", "<br>", "<br/>",
    "<hr>", "<ul>", "</ul>", "<ol>", "<ol start=\"3\">", "</ol>", "<li>", "</li>", "<blockquote>", "</blockquote>",
    "<pre>", "</pre>", "<code>", "</code>", "<a href=\"https://x.io/a_b?c=1&d=2\">", "<a href='javascript:x'>", "</a>",
    "<h1>", "</h1>", "<h3>", "</h3>", "<table>", "</table>", "<tr>", "</tr>", "<td>", "</td>", "<th>", "</th>",
    "<div>", "</div>", "<span class=\"x\">", "</span>", "<script>x<y</script>", "<style>p{}</style>", "<img alt=\"pic\">",
    "&amp;", "&lt;", "&gt;", "&nbsp;", "&#39;", "&copy", "&bogus;", "q&a", "<", ">", "\n", "\n\n", "  ", "\t", " ",
    "text", "more words", "a*b_c", "1. item", "- dash", "# hash", "`tick`", "[x](y)", "é", "🐧", " ", "|", "---",
    "<!-- c -->", "<!DOCTYPE html>", "<head><title>T</title></head>", "<body>", "</body>", "<u>", "</u>", "<s>", "</s>"]
MD_FRAGS = ["**", "*", "_", "__", "***", "`", "``", "```", "\n", "\n\n", " ", "  ", "\t", "# ", "## ", "> ", "- ", "* ",
    "1. ", "2) ", "    ", "[", "]", "(", ")", "](<", ">)", "](", "<https://e.com/a_b>", "<", ">", "\\", "\\*", "\\_",
    "&", "&amp;", "word", "more text", "x", "é", "🐧", "|", "| a | b |", "|---|---|", "---", "===", "~~", "!", "![i](u)",
    "[l](https://x.io/a*b)", "<b>", "</b>", "<div>", "</div>", "a_b_c", "foo*bar*", "\r\n", ":", ".", "https://x.io"]

def rand_doc(frags, k):
    return "".join(rng.choice(frags) for _ in range(rng.randint(1, k)))

html_inputs += [rand_doc(HTML_FRAGS, 18) for _ in range(1500)]
md_inputs += [rand_doc(MD_FRAGS, 22) for _ in range(1500)]

def dedupe(xs, cap=4000):
    seen, out = set(), []
    for x in xs:
        if x not in seen and len(x) <= cap:
            seen.add(x); out.append(x)
    return out

html_inputs = dedupe(html_inputs)
md_inputs = dedupe(md_inputs)

V = {}
def add(name, args, result):
    V.setdefault(name, []).append({"args": args, "result": result})

for h in html_inputs:
    add("html_to_text", {"content": h}, html_to_text(h))
    add("html_to_markdown", {"content": h}, html_to_markdown(h))
    add("html_tag_reducer", {"content": h}, HTMLTagReducer().reduce(h))
for m in md_inputs:
    add("text_to_html", {"content": m}, text_to_html(m))
    add("text_to_markdown", {"content": m}, text_to_markdown(m))
    add("commonmark_headings_to_bold", {"body": m}, commonmark_headings_to_bold(m))
    add("commonmark_decode_backslash_escapes", {"text": m}, commonmark_decode_backslash_escapes(m))
    add("commonmark_escape_link_url", {"url": m}, commonmark_escape_link_url(m))
    add("commonmark_pick_emphasis_sentinel", {"body": m}, commonmark_pick_emphasis_sentinel(m))
    add("commonmark_sections", {"markdown": m, "name_maxlen": 7, "value_maxlen": 11}, list(commonmark_sections(m, 7, 11)))
    add("commonmark_lookahead_closer_widths", {"next_chunk": m, "boundary_prev_ch": "a", "boundary_next_ch": None},
        commonmark_lookahead_closer_widths(m, boundary_prev_ch="a"))
    # A full emphasis render pass, as plugins drive it.
    body = m
    sentinel = commonmark_pick_emphasis_sentinel(body)
    out, delims, i, n = [], [], 0, len(body)
    while i < n:
        if body[i] in "*_":
            i = commonmark_emphasis_run(body, i, n, delims, out, sentinel)
        else:
            out.append(body[i]); i += 1
    add("render_emphasis", {"body": body}, commonmark_render_emphasis_markers("".join(out), delims, ("<b>", "</b>"), ("<i>", "</i>"), sentinel))
    for width in (1, 3, 9, 25):
        pieces = [m[i:i + width] for i in range(0, len(m), width)] or [""]
        state, trace = {}, []
        for i, piece in enumerate(pieces):
            nxt = pieces[i + 1] if i + 1 < len(pieces) else None
            fixed, state = commonmark_repair_chunk(piece, state, next_chunk=nxt)
            trace.append([piece, nxt, fixed, {k: v for k, v in state.items()}])
        add("repair_chain", {"width": width, "input": m}, trace)

for m in md_inputs[:600] + html_inputs[:300]:
    for fmt in ("text", "html", "markdown"):
        for limit in (0, 1, 3, 11, 27, 60):
            add("smart_split", {"text": m, "limit": limit, "body_format": fmt}, smart_split(m, limit, NotifyFormat(fmt)))

# Dialect converters the Rust tests can reproduce exactly.
CONVERTERS = {
    "identity": lambda s: s,
    "entities": lambda s: pyhtml.escape(s, quote=False),
    "bold": lambda s: s.replace("**", "*"),
    "upper_tags": lambda s: s.replace("<b>", "<B>").replace("</b>", "</B>"),
}
for m in md_inputs[:500]:
    for limit in (1, 3, 11, 27):
        for cname in ("identity", "entities", "bold"):
            fn = CONVERTERS[cname]
            add("split_dialect", {"body": m, "limit": limit, "converter": cname, "body_format": "markdown"},
                split_dialect_chunk(m, limit, fn, NotifyFormat.MARKDOWN))
            add("truncate_dialect", {"body": m, "limit": limit, "converter": cname, "body_format": "markdown"},
                truncate_dialect_chunk(m, limit, fn, NotifyFormat.MARKDOWN))
for h in html_inputs[:300]:
    for limit in (1, 3, 11, 27):
        for cname, fmt in (("upper_tags", "html"), ("identity", "html"), ("entities", "text")):
            fn = CONVERTERS[cname]
            add("split_dialect", {"body": h, "limit": limit, "converter": cname, "body_format": fmt},
                split_dialect_chunk(h, limit, fn, NotifyFormat(fmt)))
            add("truncate_dialect", {"body": h, "limit": limit, "converter": cname, "body_format": fmt},
                truncate_dialect_chunk(h, limit, fn, NotifyFormat(fmt)))

chars = [None, " ", "\t", "\n", "a", "Z", "1", "*", "_", "!", ".", "(", ")", " ", "　", "é", "€", "“", "🐧", " ", "-", "\\"]
for d in "*_":
    for p in chars:
        for q in chars:
            add("can_emphasis", {"delim": d, "prev": p, "next": q},
                [commonmark_can_open_emphasis(d, p, q), commonmark_can_close_emphasis(d, p, q)])

# Harvested upstream test calls (commonmark helpers with explicit indices).
for name in ("commonmark_repair_chunk", "commonmark_scan_angle_dest", "commonmark_scan_paren_dest",
             "commonmark_scan_autolink_dest", "commonmark_scan_delimiter_run", "html_adjust", "html_tag_adjust",
             "markdown_adjust", "html_tag_end", "smart_split", "commonmark_sections", "markdown_to_html",
             "convert_between", "commonmark_headings_to_bold"):
    for r in rec.get(name, []):
        V.setdefault("rec_" + name, []).append(r)

with open(out_full, "w") as f:
    json.dump(V, f, ensure_ascii=False, sort_keys=True)

# A small, committed subset: every named group, a bounded sample of each.
small = {}
srng = random.Random(7)
for name, rows in V.items():
    rows = list(rows)
    srng.shuffle(rows)
    keep = []
    budget = 60000
    for r in rows:
        size = len(json.dumps(r, ensure_ascii=False))
        if size > 2500:
            continue
        if budget - size < 0 or len(keep) >= 150:
            break
        budget -= size
        keep.append(r)
    small[name] = keep
with open(out_small, "w") as f:
    json.dump(small, f, ensure_ascii=False, sort_keys=True, indent=0)
for name, rows in sorted(V.items()):
    print(name, len(rows), len(small[name]), file=sys.stderr)

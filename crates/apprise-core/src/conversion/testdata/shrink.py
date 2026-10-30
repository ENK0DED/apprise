"""Shrink markdown inputs where the Rust port differs from upstream.

Usage: python -I shrink.py TEST_BINARY md_full.json [max_cases]
"""
import json, os, subprocess, sys, tempfile
from apprise.conversion import markdown_to_html
binary, vectors = sys.argv[1], sys.argv[2]
limit = int(sys.argv[3]) if len(sys.argv) > 3 else 30
tmp = tempfile.mkdtemp()
fin, fout = os.path.join(tmp, "in.json"), os.path.join(tmp, "out.json")

def rust(inputs):
    json.dump(inputs, open(fin, "w"))
    env = dict(os.environ, APPRISE_MD_IN=fin, APPRISE_MD_OUT=fout)
    subprocess.run([binary, "--ignored", "--exact", "conversion::vectors::dump_markdown_conversions", "-q"], env=env, capture_output=True)
    return json.load(open(fout))

rows = json.load(open(vectors))["markdown_to_html"]
inputs = [r["args"]["content"] for r in rows if r["args"]["extensions"] is None]
outs = rust(inputs)
bad = [i for i, o in zip(inputs, outs) if o != markdown_to_html(i)]
print(f"{len(bad)} failing of {len(inputs)}", file=sys.stderr)
bad.sort(key=len)
results = []
for text in bad[:limit]:
    cur = text
    changed = True
    while changed:
        changed = False
        n = len(cur)
        cands = []
        for size in (n // 2, n // 4, 8, 4, 2, 1):
            if size < 1:
                continue
            for start in range(0, n, size):
                c = cur[:start] + cur[start + size:]
                if c and c not in cands:
                    cands.append(c)
        outs = rust(cands)
        for c, o in zip(cands, outs):
            if o != markdown_to_html(c):
                cur = c
                changed = True
                break
    results.append(cur)
seen = set()
for r in sorted(results, key=len):
    if r in seen: continue
    seen.add(r)
    print(json.dumps(r, ensure_ascii=False))
    print("  py:", json.dumps(markdown_to_html(r), ensure_ascii=False))
    print("  rs:", json.dumps(rust([r])[0], ensure_ascii=False))

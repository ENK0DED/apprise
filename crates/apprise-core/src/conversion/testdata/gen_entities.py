"""Generate crates/apprise-core/src/conversion/entities.rs from Python's html module."""
import html, html.entities, sys
out = sys.argv[1]
names = sorted(html.entities.html5.items())
def lit(s):
    r = []
    for ch in s:
        o = ord(ch)
        if ch == '"': r.append('\\"')
        elif ch == '\\': r.append('\\\\')
        elif 0x20 <= o < 0x7f: r.append(ch)
        else: r.append('\\u{%x}' % o)
    return '"' + ''.join(r) + '"'
inv = sorted(html._invalid_charrefs.items())
cps = sorted(html._invalid_codepoints)
# Compress invalid code points into ranges
ranges = []
for c in cps:
    if ranges and ranges[-1][1] + 1 == c: ranges[-1][1] = c
    else: ranges.append([c, c])
with open(out, 'w') as f:
    f.write('//! HTML5 character references, generated from Python\'s `html.entities.html5`\n')
    f.write('//! and the `html` module\'s invalid character-reference tables.\n')
    f.write('//! Regenerate with `testdata/gen_entities.py` rather than editing by hand.\n\n')
    f.write('/// Named references, sorted by name. Names include their trailing `;`\n')
    f.write('/// where the standard requires one, so `amp` and `amp;` are both listed.\n')
    f.write('pub(crate) static HTML5: &[(&str, &str)] = &[\n')
    for k, v in names:
        f.write(f'  ({lit(k)}, {lit(v)}),\n')
    f.write('];\n\n')
    f.write('/// Numeric references the standard maps to another character.\n')
    f.write('pub(crate) static INVALID_CHARREFS: &[(u32, char)] = &[\n')
    for k, v in inv:
        f.write(f"  (0x{k:x}, '{lit(v)[1:-1]}'),\n" if v not in ("'",) else f"  (0x{k:x}, '\\''),\n")
    f.write('];\n\n')
    f.write('/// Code points a numeric reference may not produce (inclusive ranges).\n')
    f.write('pub(crate) static INVALID_CODEPOINTS: &[(u32, u32)] = &[\n')
    for a, b in ranges:
        f.write(f'  (0x{a:x}, 0x{b:x}),\n')
    f.write('];\n')
print(len(names), len(inv), len(ranges), max(len(k) for k,_ in names))

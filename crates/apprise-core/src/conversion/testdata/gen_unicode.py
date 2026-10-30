"""Generate the punctuation/symbol table used by CommonMark flanking rules."""
import sys, unicodedata
out = sys.argv[1]
ranges = []
for c in range(sys.maxunicode + 1):
    if unicodedata.category(chr(c))[0] in ("P", "S"):
        if ranges and ranges[-1][1] + 1 == c: ranges[-1][1] = c
        else: ranges.append([c, c])
with open(out, "w") as f:
    f.write("//! Unicode punctuation and symbol ranges (general categories `P*` and\n")
    f.write(f"//! `S*`), generated from Python's `unicodedata` (Unicode {unicodedata.unidata_version}).\n")
    f.write("//! Regenerate with `testdata/gen_unicode.py` rather than editing by hand.\n\n")
    f.write("/// Inclusive code point ranges, sorted.\n")
    f.write("pub(crate) static PUNCT_OR_SYMBOL: &[(u32, u32)] = &[\n")
    for i in range(0, len(ranges), 6):
        f.write("  " + " ".join(f"(0x{a:x}, 0x{b:x})," for a, b in ranges[i:i+6]) + "\n")
    f.write("];\n")
print(len(ranges), unicodedata.unidata_version)

# Python regex classes used by the Markdown port: \w and \d.
import re as _re
def _ranges(pred):
    rs = []
    for c in range(sys.maxunicode + 1):
        if pred(chr(c)):
            if rs and rs[-1][1] + 1 == c: rs[-1][1] = c
            else: rs.append([c, c])
    return rs
_word = _ranges(lambda ch: _re.match(r"\w", ch) is not None)
_digit = _ranges(lambda ch: _re.match(r"\d", ch) is not None)
with open(out, "a") as f:
    for name, rs, doc in (("WORD", _word, "Python's `\\w` (str patterns)."), ("DECIMAL", _digit, "Python's `\\d` (str patterns).")):
        f.write(f"\n/// {doc}\n")
        f.write(f"pub(crate) static {name}: &[(u32, u32)] = &[\n")
        for i in range(0, len(rs), 6):
            f.write("  " + " ".join(f"(0x{a:x}, 0x{b:x})," for a, b in rs[i:i+6]) + "\n")
        f.write("];\n")
print(len(_word), len(_digit))

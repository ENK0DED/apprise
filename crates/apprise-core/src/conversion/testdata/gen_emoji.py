"""Expand upstream's EMOJI_MAP regex keys into plain names for emoji.rs."""
import re, sys
try:
    import re._parser as sre_parse
    from re._constants import LITERAL, SUBPATTERN, BRANCH, MAX_REPEAT, MIN_REPEAT, IN
except ImportError:
    import sre_parse
    from sre_constants import LITERAL, SUBPATTERN, BRANCH, MAX_REPEAT, MIN_REPEAT, IN
from apprise.emojis import EMOJI_MAP

def expand(items):
    results = [""]
    for op, av in items:
        if op is LITERAL:
            results = [r + chr(av) for r in results]
        elif op is SUBPATTERN:
            sub = expand(av[3])
            results = [r + s for r in results for s in sub]
        elif op is BRANCH:
            alts = [s for branch in av[1] for s in expand(branch)]
            results = [r + s for r in results for s in alts]
        elif op in (MAX_REPEAT, MIN_REPEAT):
            lo, hi, sub = av
            assert (lo, hi) == (0, 1), (lo, hi)
            subs = expand(sub)
            results = [r + s for r in results for s in [""] + subs]
        else:
            raise ValueError((op, av))
    return results

out = sys.argv[1]
rows = []
for pattern, emoji in EMOJI_MAP.items():
    for name in expand(sre_parse.parse(pattern)):
        assert name.startswith(":") and name.endswith(":"), name
        rows.append((name[1:-1].lower(), emoji))
# Check each expansion matches its own pattern only as a full match.
for pattern, emoji in EMOJI_MAP.items():
    for name in expand(sre_parse.parse(pattern)):
        assert re.fullmatch(pattern, name, re.IGNORECASE)

def lit(s):
    return '"' + "".join(c if 0x20 <= ord(c) < 0x7f and c not in '"\\' else ("\\" + c if c in '"\\' else "\\u{%x}" % ord(c)) for c in s) + '"'

with open(out, "w") as f:
    f.write("// Generated from upstream apprise/emojis.py by testdata/gen_emoji.py; do not edit.\n")
    f.write("// Each upstream regex key is expanded to every name it matches, in the\n")
    f.write("// map's order, so an earlier key wins when two keys match the same name.\n")
    f.write("pub(crate) static EMOJIS: &[(&str, &str)] = &[\n")
    for name, emoji in rows:
        f.write(f"  ({lit(name)}, {lit(emoji)}),\n")
    f.write("];\n")
print(len(EMOJI_MAP), len(rows), max(len(n) for n, _ in rows), file=sys.stderr)
print(sorted(set(c for n,_ in rows for c in n)), file=sys.stderr)

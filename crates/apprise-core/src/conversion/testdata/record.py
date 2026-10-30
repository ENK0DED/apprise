"""Run upstream test functions and record every conversion call they make."""
import importlib.util, inspect, itertools, json, sys, os, traceback
STUB = os.path.join(os.path.dirname(os.path.abspath(__file__)), "stub")
sys.path.insert(0, STUB)
import apprise
import apprise.conversion as conv
from apprise.conversion import format as cformat, html as chtml, commonmark as ccm, dialect as cdialect
from apprise.utils import format as uformat
from apprise import NotifyFormat

MAX_INPUT = 3000
records = {}

def jsonable(v):
    if isinstance(v, NotifyFormat):
        return v.value
    if isinstance(v, (str, int, bool, type(None))):
        return v
    if isinstance(v, float):
        return v
    if isinstance(v, (list, tuple)):
        return [jsonable(x) for x in v]
    if isinstance(v, dict):
        return {str(k): jsonable(x) for k, x in v.items()}
    raise TypeError(type(v))

def too_big(v):
    if isinstance(v, str):
        return len(v) > MAX_INPUT
    if isinstance(v, (list, tuple)):
        return any(too_big(x) for x in v)
    if isinstance(v, dict):
        return any(too_big(x) for x in v.values())
    return False

def wrap(name, fn):
    def wrapper(*args, **kwargs):
        try:
            sig = inspect.signature(fn).bind(*args, **kwargs)
            sig.apply_defaults()
            argd = dict(sig.arguments)
            argd.pop("record_atoms", None)
            before = jsonable(argd)
        except TypeError:
            before = None
        result = fn(*args, **kwargs)
        try:
            if "record_atoms" in kwargs and kwargs["record_atoms"] is not None:
                return result
            if before is None or too_big(argd):
                return result
            entry = {"args": before, "result": jsonable(result)}
            if "budget" in argd and argd["budget"] is not None:
                entry["budget_after"] = jsonable(argd["budget"])
            key = json.dumps(entry["args"], sort_keys=True)
            bucket = records.setdefault(name, {})
            if key not in bucket and len(bucket) < 4000:
                bucket[key] = entry
        except TypeError:
            pass
        return result
    wrapper.__wrapped__ = fn
    return wrapper

NAMES = ["html_to_text", "html_to_markdown", "markdown_to_html", "text_to_html", "text_to_markdown",
         "convert_between", "commonmark_headings_to_bold", "commonmark_decode_backslash_escapes",
         "commonmark_escape_link_url", "commonmark_repair_chunk", "commonmark_sections",
         "commonmark_scan_angle_dest", "commonmark_scan_paren_dest", "commonmark_scan_autolink_dest",
         "commonmark_can_open_emphasis", "commonmark_can_close_emphasis",
         "commonmark_lookahead_closer_widths", "commonmark_pick_emphasis_sentinel",
         "commonmark_scan_delimiter_run", "smart_split", "html_adjust", "html_tag_adjust",
         "markdown_adjust", "html_tag_end"]

def install(extra_modules):
    originals = {}
    for mod in [cformat, chtml, ccm, cdialect, uformat, conv] + extra_modules:
        for name in NAMES:
            if hasattr(mod, name):
                orig = getattr(mod, name)
                orig = getattr(orig, "__wrapped__", orig)
                originals.setdefault(name, orig)
    wrappers = {n: wrap(n, f) for n, f in originals.items()}
    for mod in list(sys.modules.values()):
        mname = getattr(mod, "__name__", "") or ""
        if not (mname.startswith("apprise") or mname.startswith("test_")):
            continue
        for name, orig in originals.items():
            if getattr(mod, name, None) is orig:
                setattr(mod, name, wrappers[name])

def run_tests(path):
    spec = importlib.util.spec_from_file_location(os.path.basename(path)[:-3], path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    install([mod])
    ok = fail = 0
    for name, fn in list(vars(mod).items()):
        if not name.startswith("test_") or not callable(fn):
            continue
        params = getattr(fn, "_params", [])
        combos = [{}]
        for names, values in params:
            names = [n.strip() for n in names.split(",")] if isinstance(names, str) else list(names)
            new = []
            for c in combos:
                for v in values:
                    vals = v if len(names) > 1 else (v,)
                    if hasattr(vals, "values") and not isinstance(vals, tuple):
                        vals = tuple(vals.values)
                    d = dict(c); d.update(zip(names, vals)); new.append(d)
            combos = new
        required = [p for p in inspect.signature(fn).parameters if p not in (combos[0] if combos else {})]
        if required:
            continue
        for c in combos:
            try:
                fn(**c); ok += 1
            except Exception as e:
                fail += 1
    print(f"{os.path.basename(path)}: {ok} ok, {fail} failed", file=sys.stderr)

if __name__ == "__main__":
    out = sys.argv[1]
    for path in sys.argv[2:]:
        run_tests(path)
    data = {name: list(b.values()) for name, b in sorted(records.items())}
    with open(out, "w") as f:
        json.dump(data, f, ensure_ascii=False, indent=0, sort_keys=True)
    for name, b in sorted(data.items()):
        print(name, len(b), file=sys.stderr)

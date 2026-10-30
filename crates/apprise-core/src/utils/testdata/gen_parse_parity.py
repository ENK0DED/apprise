"""Generate the parse parity vectors in parse_parity.json from upstream Apprise.

Run with an interpreter that has upstream apprise installed:
    python -I gen_parse_parity.py > parse_parity.json
"""
import json, sys
from apprise.utils.parse import parse_url, parse_urls, parse_list, parse_bool
from apprise.url import URLBase

URLS = [
    "http://hostname", "http://5t4m59hl-34343.euw.devtunnels.ms", "http://hostname/",
    "http://hostname:", "http://hostname:invalid", "http://hostname:0", "http://hostname:-92",
    "http://hostname:65535", "http://hostname:1", "http://hostname:4.2", "http://hostname:0123",
    "http://[2001:db8:002a:3256:adfe:05c0:0003:0006]:8080", "http://[::1]:80/", "http://[2001:DB8::1]:80",
    "http://[2001:db8::1]", "http://[fe80::1%25eth0]:80", "http://[1.2.3.4]:80", "http://[v1.fe]/",
    "http://[::1]x:80", "http://a[::1]:80", "http://[foo", "http://foo]", "http://us[er]@host/",
    "http://hostname/?-KeY=Value", "http://hostname/?+KeY=Value", "http://hostname/?:kEy=vALUE",
    "http://hostname/?+KeY=ValueA&-kEy=ValueB&KEY=Value%20+C&:colon=y", "http://hostname////",
    "http://hostname:40////", "HTTP://HoStNaMe:40/test.php", "HTTPS://user@hostname/test.py",
    "  HTTPS://///user@@@hostname///test.py  ", "HTTPS://user:password@otherHost/full///path/name/",
    "HTTPS://hostname/a/path/ending/with/slash/?key=value",
    "mailto://user:password@otherHost/lead2gold@gmail.com?from=test@test.com&name=Chris%20Caron&format=text",
    "http://user:pass.with.?question@host", "test://", "test://:@/", "crazy://:@//_/@^&/jack.json",
    "json://localhost", "json://localhost/", "json://localhost//a//b//",
    "json://u:p@h:80/a%2Fb/c d/~x?A=1&b=+x+&c", "tgram://123:abc/lead/", "tgram://123456789:abcdefg_hijklmnop/lead2gold/",
    "tgram://123456789:AAF0abc-def/%23chan/", "json://a@b@c/x", "json://u:pa:ss@h", "dbus://", "json://h:abc",
    "json://host/path/#chan", "json://host/path/%23chan", "json://h/a,b;c/@x!$&()*+=", "json://h/a;b/c;d",
    "json://h/a/b;c", "json://h;x/a", "json://h?x=1;y=2&z", "json://h/~user/", "json://h/%7Euser", "json://h:70000",
    "json://h?", "json://h? ", "json://?x=1", "json:/host", "json:\\\\host\\path", "json://h/a\\\\b\\c/",
    "json://h/a?x=/", "json://h?x=/", "json://h/%zz/%41%2f", "json://h/café/ü?n=é",
    "json://h/%E2%82%AC/%ff", "json://h?a=%E2%82%AC&b=%ff", "json://h?=x&&a&b=c=d", "json://h?A=1&a=2",
    "json://h?a+b=1&+c+d=2", "json://h?%20sp=1&-%20m=2", "json://h?k=%20v%20", "json://h/path?x=1#frag",
    "json://h#frag/x", "json://h/\tp\na\rth", "json://h?x=1\n", "json://h?x=1\ny", "json://h\n?x=1",
    "json://user@:80/", "json://@h/", "json://:@h/", "json://u:@h/", "json://:p@h/",
    "mailtos://PASSWORD@hostname?user=admin@mail-domain.com", "mailtos://u:p@h?user=other",
    "mailtos://u@h?pass=secret", "mailtos://u:p@h?password=new", "json://h?port=8080", "json://h:80?port=abc",
    "json://h:80?port=", "json://localhost:8080/path?Format=HTML", "napi://a/b/c@d/e",
    "discord://host?url=https://localhost", "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#general",
    "irc://host/#chan", "irc://host/chan#x", "json://host/a%2Cb/c%3Bd/a%40b", "json://host/a'b(c)*d+e=f&g$h!i:j",
    "json://host/%5B%5D/%7Bx%7D/a|b/a^b/a`b", "json://host/a\"b<c>d", "json://host/ ",
    "xml://user:pass@host:8000/path/to/xml.php?:key=value&+Header=abc&-Remove", "kodi://user:pass@host:8080",
    "pover://u@t/?priority=high&tags=a,b", "ntfy://localhost/topic?tag=cool,rust&tags=x",
    "json://host/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z",
    "**bold** and [link](https://example.com)", "text with [unclosed bracket",
    "json://h/?x=%3Fy%3D1", "json://h/a%3Fb", "json://h:80:90/", "json://[::1]:80:90/",
    "json://host:080", "json://host:+80", "json://host:8_0", "json://host: 80", "json://host:80a",
    "json://hö st/", "json://h?k=v&k=w", "json://H?K=V",
    "whatsapp://t@id/?:2=second&:1=first&z=1&a=2&Z=3;m=4&+B=1&+a=2&-y&-x",
]

def norm(r):
    if r is None:
        return None
    return {
        "schema": r["schema"], "user": r["user"], "password": r["password"],
        "host": r["host"], "port": r["port"], "fullpath": r["fullpath"],
        "qsd": r["qsd"], "qsd+": r["qsd+"], "qsd-": r["qsd-"], "qsd:": r["qsd:"],
        # JSON objects lose their order in serde_json; keep it explicitly
        "order": {k: list(r[k]) for k in ("qsd", "qsd+", "qsd-", "qsd:")},
    }

out = {"parse_url": []}
for u in URLS:
    # url_to_dict() swaps "/#" for "/%23" before any plugin parses the URL
    u_ = u.replace("/#", "/%23")
    loose = URLBase.parse_url(u_, verify_host=False)
    strict = URLBase.parse_url(u_)
    out["parse_url"].append({
        "url": u,
        "loose": norm(loose),
        "strict_host": None if strict is None else strict["host"],
        "strict_ok": strict is not None,
    })

PU = [
    "", ",  ,, , ,,, ", ",", "this is not a parseable url at all", "windows://", "windows:// gnome://",
    "discord://host?url=https://localhost",
    "mailgun://noreply@sandbox.mailgun.org/apikey/?to=test@example.com,test2@example.com,, abcd@example.com, mailgun://noreply@sandbox.another.mailgun.org/apikey/?to=hello@example.com,,hmmm@example.com,, abcd@example.com, ,, windows://",
    "http://hostname?password=,abcd,", "schema1://hostname?password=,abcd,, schema2://hostname?password=,abcd,",
    "json://a/;json://b/", "json://a/ ; json://b/", "json://a/; json://b/", "json://a/,;  json://b/",
    "json://a/ ,; json://b/", "json://a/;json://b/;mailto://c/", "json://a/?x=1;2", "json://u:p;w@h/",
    "json://a/;b", "json://a/?url=x;y,z", "json://a/?x=1;2;json://u:p;w@h/", "https://example.com" + " " * 500,
    "json://a\njson://b", "json://a\n\njson://b\n", "json://a  \n  xml://b", "json://a\nnot a url\njson://b",
    "JSON://A, XML://B", "json://a,xml://b", "json://a," + " " * 40 + "xml://b", "json://a" + ";" * 33 + "xml://b",
    "json://a, " + "x" * 33 + "://b", "json://a, " + "x" * 32 + "://b", "x json://a", "json://a xjson:/b",
    "[json://a] [xml://b]", "json://a\txml://b", "json://a\r\nxml://b", "1://a 2://b",
]
out["parse_urls"] = [{"input": s, "output": parse_urls(s)} for s in PU]

PL = [
    "", ".mkv, .iso, .avi", "a b  c", "c,b,a,a", "[a];b , c\td\ne", "tag1, tag2 ,TAG1", "2:endpoint, other",
    ",,,", "x", "a　b",
]
out["parse_list"] = [{"input": s, "output": parse_list(s)} for s in PL]

PB = ["Enabled", "Disabled", "Allow", "Deny", "Yes", "YES", "Always", "No", "NO", "NEVER", "TrUE", "tRUe",
      "FAlse", "F", "T", "0", "1", "True", "OhYeah", "", "on", "off", "of", "active", "enable", "y", "n",
      "ye", "yesss", "nah", "2", "-1", " yes", "random"]
out["parse_bool"] = [{"input": s, "default_false": parse_bool(s, False), "default_true": parse_bool(s, True)} for s in PB]

json.dump(out, sys.stdout, indent=1, ensure_ascii=False, sort_keys=True)

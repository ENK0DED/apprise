"""Generate config_parity.json: configurations run through upstream Apprise.

Run with an interpreter that has upstream apprise installed:
    python -I gen_config_parity.py > config_parity.json
Each case records, per loaded service, its tags (lowercased, as upstream's
AppriseTag stores them). Only plugins the Rust port has are used.
"""
import json, sys
from apprise import AppriseConfig, AppriseAsset
from apprise.config.base import ConfigBase

CASES = [
    ("text", "json://localhost\nxml://localhost\n"),
    ("text", "# c\n; c\n  tagA, tagB = json://localhost\n=xml://localhost\n"),
    ("text", "groupA, groupB = tagB, tagC\ngroupC = groupA\ngroupD = groupE\ngroupE = tagD\ntagA=json://localhost/?A\ntagB=json://localhost/?B\ntagC=json://localhost/?C\ntagD=json://localhost/?D\n"),
    ("text", "mygrouptag=mytagA\nmytagA=json://localhost\nother=json://localhost\n"),
    ("text", "2:endpoint=json://localhost\nalerts:3=json://localhost\n"),
    ("text", "json://localhost?tag=foo\nbar=ntfy://localhost/topic?tags=emoji\n"),
    ("text", "json://localhost\n!!! bad line\n"),
    ("text", "not a url\njson://localhost\n"),
    ("text", "grp=tag\ntag=json://localhost/?1\nTAG=json://localhost/?2\n"),
    ("text", "unknown://host\njson://localhost\n"),
    ("text", "https://discord.com/api/webhooks/0123456789/abcdefg\nd=https://mattermost.example.com/hooks/abc\nhttps://example.com/hooks/abc\n"),
    ("yaml", "urls:\n  - json://localhost\n  - xml://localhost\n"),
    ("yaml", "version: 2\nurls:\n  - json://localhost\n"),
    ("yaml", "urls:\n  - json://localhost\n  bad: [\n"),
    ("yaml", "tag: admin, devops\nurls:\n  - json://localhost\n  - xml://localhost:\n      tag: local\n"),
    ("yaml", "tags:\n  - a\n  - b\nurls:\n  - json://localhost:\n      tags: c\n"),
    ("yaml", "urls:\n  - json://localhost:\n      tag: a\n      tags: b\n"),
    ("yaml", "urls:\n  - json://localhost:\n    - to: a\n      tag: one\n    - to: b\n    - not-a-mapping\n    tag: shared\n"),
    ("yaml", "groups:\n  admins: tagA, tagB\n  devops:\n    - tagX\n    - tagY\n  everyone:\n    - admins\n    - devops\nurls:\n  - json://localhost:\n      tag: tagA\n  - json://localhost:\n      tag: tagY\n  - json://localhost\n"),
    ("yaml", "groups:\n  - g1, g2: t1\n  - g3:\n    - t1: ignored\n  - nonsense\nurls:\n  - json://localhost:\n      tag: t1\n"),
    ("yaml", "urls:\n  - json://localhost:\n      tag: alerts:3\n  - json://localhost:\n      tag: 2:alerts\n"),
    ("yaml", "urls:\n  - format: markdown\n    json://localhost:\n    tag: sib\n"),
    ("yaml", "urls:\n  - json://localhost:\n    tag: sib\n"),
    ("yaml", "urls:\n  - json://a:\n      tag: x\n      tag: y\n"),
    ("yaml", "base: &base\n  tag: shared\nurls:\n  - json://a:\n      <<: *base\n  - json://b: *base\n"),
    ("yaml", "urls:\n  - ntfy://localhost/topic?tags=emoji:\n      tag: mine\n"),
    ("yaml", "urls:\n  - unknown://host\n  - 42\n  - json://localhost\n"),
    ("yaml", "urls:\n  - https://discord.com/api/webhooks/0123456789/abcdefg:\n      tag: d\n"),
    ("yaml", "template:\n  - t\nurls:\n  - ${t}://localhost\n  - json://localhost:\n      tag: ${t}\n  - json://localhost/${other}\n"),
    ("yaml", "template:\n  - t: path\nurls:\n  - json://localhost/${t}:\n      tag: tt\n"),
    ("yaml", "urls: json://localhost\n"),
    ("yaml", "- json://localhost\n"),
]

out = []
for fmt, content in CASES:
    asset = AppriseAsset()
    services, _ = ConfigBase.config_parse(content, asset=asset, config_format=fmt)
    resolved = []
    for s in services:
        tags = sorted(str(t) for t in s.tags)
        resolved.append(tags)
    out.append({"format": fmt, "content": content, "services": resolved})
json.dump(out, sys.stdout, indent=1)

# Conversion test vectors

`vectors.json` holds inputs together with the output upstream Apprise
(Python, 2.1.0 / master) produced for each. `conversion/vectors.rs` feeds
the same inputs to the Rust port and compares.

The inputs come from three places:

- calls recorded while running upstream's own `tests/test_conversion.py`
  and `tests/test_utils_format.py` (groups prefixed `rec_`),
- the corpora apprise-go pinned against upstream
  (`internal/notify/testdata/*.json`),
- seeded random documents built from HTML and Markdown fragments that
  notifications commonly contain.

The generator scripts live next to this file. Regenerate with:

```
python -I record.py recorded.json upstream/tests/test_conversion.py upstream/tests/test_utils_format.py
python -I gen_vectors.py recorded.json apprise-go/internal/notify/testdata vectors_full.json vectors.json
```

`record.py` needs the `stub/pytest.py` stand-in on its path; it runs the
upstream test functions directly and records every conversion call they
make. `vectors_full.json` (about 80,000 vectors) is too large to commit;
run it with
`APPRISE_CONVERSION_VECTORS=vectors_full.json cargo test -p apprise-core --lib -- --include-ignored conversion::vectors`.

`shrink.py` minimises inputs where the port and upstream disagree. Build
the test binary (`cargo test -p apprise-core --lib --no-run`), then run
`python -I shrink.py target/debug/deps/apprise_core-<hash> vectors_full.json`;
it drives the ignored `dump_markdown_conversions` test.

`gen_entities.py` and `gen_unicode.py` generate `entities.rs` and
`unicode_tables.rs` from Python's `html`, `re` and `unicodedata` modules;
`gen_emoji.py` generates `utils/emoji_table.rs` from `apprise/emojis.py`.

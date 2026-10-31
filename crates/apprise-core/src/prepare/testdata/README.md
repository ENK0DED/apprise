# Message preparation test vectors

`prepare.json` holds randomly generated plugin settings (limits, formats,
`overflow_amalgamate_title`, a dialect, URL options) and notifications,
together with every `send()` call upstream Apprise made for them.
`prepare/tests.rs` replays each case through `prepare_contexts` with an
equivalent plugin and compares the pieces.

Regenerate the full set (about 1,800 cases) with
`python -I gen_prepare.py prepare_full.json`, then run it with
`APPRISE_PREPARE_VECTORS=prepare_full.json cargo test -p apprise-core --lib -- --include-ignored prepare::`.
The committed file is a 350-case sample of it.

`passthrough.json` is the same kind of sample for content with no input
format (`notify(body_format=None)`), from
`python -I gen_prepare.py passthrough_full.json --passthrough`: 100 cases
whose calls differ from the declared-format run and 20 whose calls do not.

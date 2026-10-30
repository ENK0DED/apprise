# Contributing to Apprise

Thank you for your interest in contributing to this Rust port of Apprise!

We welcome bug reports, feature requests, documentation improvements, and new
notification plugins. Please follow the guidelines below to help us review and
merge your contributions smoothly.

Upstream [Apprise](https://github.com/caronc/apprise) is the specification:
a plugin here should accept the same URLs and send the same requests as its
upstream counterpart. Issues that only affect this port belong in
[this repository](https://github.com/ENK0DED/apprise/issues).

---

## Quick Checklist Before You Submit

- Your code is formatted and passes clippy:
  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets
  ```

- Your changes are covered by tests, and the test suites pass (see
  [Running Tests](#running-tests)).

- A new plugin follows the [plugin conventions](#adding-or-changing-a-plugin).
- Your commit message is descriptive.

---

## Local Development Setup

### System Requirements

- Rust >= 1.96 (install via [rustup](https://rustup.rs/))
- [Bun](https://bun.sh/) (for NAPI bindings and JS tests)
- `git`

### One-Time Setup

```bash
git clone https://github.com/ENK0DED/apprise.git
cd apprise

# Verify Rust toolchain
cargo check

# Install JS dependencies (for NAPI bindings)
cd crates/apprise-napi
bun install
```

---

## Running Tests

The core library has optional Cargo features, so test it with the default
features, with none, and with the ones that are off by default:

```bash
cargo test -p apprise-core                             # default features
cargo test -p apprise-core --no-default-features       # every optional feature off
cargo test -p apprise-core --features desktop,blink1   # adds the default-off features
cargo test -p apprise-cli                              # the command line tool
```

While working on one plugin, run just its tests:
```bash
cargo test -p apprise-core --lib notify::gotify
```

For the NAPI bindings:
```bash
cd crates/apprise-napi
bun run build:debug
bun test
```

`bun run build:debug` regenerates `index.js` and `index.d.ts`; commit them
when the binding's API or the package version changed.

CI runs `cargo test --workspace --exclude apprise-napi`, the
`--no-default-features` core tests and the lint steps, builds the bindings for
every platform they ship for, and tests them on Linux, macOS and Windows. It
does not test `desktop,blink1`, so run that one locally.

---

## Linting & Formatting

```bash
cargo fmt --all                          # Format Rust code (rustfmt.toml: 2 spaces, width 160)
cargo clippy --workspace --all-targets   # Lint Rust code, tests included
cd crates/apprise-napi && bun lint       # Lint JS/TS
```

---

## Project Structure

- `crates/apprise-core` -- Core notification library; plugins live in `src/notify/`
- `crates/apprise-cli` -- CLI binary (`apprise`)
- `crates/apprise-napi` -- Node.js native binding (NAPI-RS), published as `@enk0ded/apprise`
- `packaging/man` -- the man page

---

## Adding or Changing a Plugin

Start from an existing plugin such as `notify/gotify.rs` or `notify/prowl.rs`
(`prowl.rs` has complete send tests), and from the upstream plugin and its
tests (`apprise/plugins/<name>.py`, `tests/test_plugin_<name>.py`).

- **The plugin file**: `crates/apprise-core/src/notify/<name>.rs` (a
  directory with a `mod.rs` for large ones, such as `matrix/` or `email/`).
  It holds a struct with `from_url(&ParsedUrl) -> Option<Self>`, returning
  `None` wherever upstream rejects the URL, `static_details()`, and an
  `impl Notify` that overrides the limits and formats upstream's class sets.
- **Registration**, kept in alphabetical order:
  - `pub mod <name>;` in `notify/mod.rs`;
  - `collect!(<name>::<Type>::static_details());` in `all_service_details()`
    in `notify/registry.rs`;
  - `reg!(<name>::<Type>::from_url, "<schema>", ...);` in `build_registry()`
    in `notify/registry.rs`, with every schema alias upstream accepts;
  - a rule in `NATIVE_URLS` (`notify/registry.rs`) if upstream recognises
    the service's own `https://` URLs.
- **Common URL options** (`format`, `overflow`, `emojis`, `store`, `tz`,
  `retry`, `wait`, `optional`, `redirect`, `cto`, `rto`, `verify`) are parsed
  for every plugin in `notify/options.rs`; don't parse them again. Routing
  tags come from `url.tags()`.
- **HTTP**: build clients with `build_client(verify)`, send
  `User-Agent: APP_ID`, and wrap every fixed service address in
  `service_base("https://api.example.com")` so tests can redirect it.
- **Secrets**: never log tokens, passwords or keys at any level; log request
  errors with `crate::error::loggable_http_error(&e)`.
- **Heavy dependencies** go behind a Cargo feature, as `email`, `amqp` and
  `matrix-e2ee` do.

### Tests

Tests live in a `#[cfg(test)] mod tests` at the bottom of the plugin file.
Cover every valid and invalid URL from upstream's tests, the exact request
each send makes (method, path, headers and payload), and failure handling
(non-2xx responses, bad JSON, error codes in a 200 response).

Never call real services. Run sends against a local `wiremock::MockServer`
inside `crate::notify::with_test_base(server.uri(), svc.send(&ctx))`:

```rust
let server = MockServer::start().await;
Mock::given(method("POST"))
  .and(path("/publicapi/add"))
  .respond_with(ResponseTemplate::new(200))
  .expect(1)
  .mount(&server)
  .await;
assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
```

Match mock requests by method and path, and give GET mocks a path too; never
rely on the position or total count of received requests. Other processes can
send stray requests to local ports and break such tests. Raw-socket fakes
(SMTP, IRC, MQTT, ...) should likewise ignore connections that do not speak
their protocol.

---

## How to Contribute

1. **Fork the repository** and create a new branch.
2. Make your changes.
3. Run the checks listed above.
4. Submit a pull request (PR) to the `master` branch.

GitHub Actions will run tests and lint checks on your PR automatically.

---

## Thank You

Your contributions make Apprise better for everyone -- thank you!

See [ACKNOWLEDGEMENTS.md](./ACKNOWLEDGEMENTS.md) for a list of contributors.

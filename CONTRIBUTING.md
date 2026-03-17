# Contributing to Apprise

Thank you for your interest in contributing to Apprise!

We welcome bug reports, feature requests, documentation improvements, and new
notification plugins. Please follow the guidelines below to help us review and
merge your contributions smoothly.

---

## Quick Checklist Before You Submit

- Your code passes all lint checks:
  ```bash
  cargo clippy
  cargo fmt -- --check
  ```

- Your changes are covered by tests:
  ```bash
  cargo test
  ```

- You followed the plugin template (if adding a new plugin).
- You included the BSD 2-Clause license header.
- Your commit message is descriptive.

---

## Local Development Setup

### System Requirements

- Rust >= 1.94 (install via [rustup](https://rustup.rs/))
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

```bash
cargo test                  # Run all tests
cargo test -p apprise-core  # Run core library tests only
```

For the NAPI bindings:
```bash
cd crates/apprise-napi
bun run build:debug
bun test
```

---

## Linting & Formatting

```bash
cargo fmt                # Format Rust code
cargo clippy             # Lint Rust code
cd crates/apprise-napi && bun lint  # Lint JS/TS
```

---

## Project Structure

- `crates/apprise-core` -- Core notification library (127+ plugins)
- `crates/apprise-cli` -- CLI binary
- `crates/apprise-napi` -- Node.js native binding (NAPI-RS)

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

<h1 align="center">
<img  src="assets/logo-github.png" />
</h1>

# redis-rover

A Redis terminal-ui client written in rust 🦀

`redis-rover` (`rrover`) is a keyboard-driven TUI client for Redis. It lets you browse keyspaces, inspect key metadata, filter by pattern, and monitor server info — all from the terminal. Built with [ratatui](https://github.com/ratatui-org/ratatui) and powered by async I/O via Tokio.

**Features:**

- Browse and filter Redis keys with type-colored badges
- Paginate through large keyspaces via SCAN
- Live Redis server info in the footer
- Fully configurable key bindings, colors, and styles via a config file (JSON5, YAML, TOML, INI)

---

## Installation

**From crates.io:**

```bash
cargo install redis-rover
```

**Prebuilt binaries** for macOS (x86\_64, arm64), Linux (x86\_64, aarch64), and Windows are available on the [GitHub releases page](https://github.com/danik-tro/redis-rover/releases).

---

## Usage

```bash
rrover
```

Connects to Redis at `localhost:6379` by default.

---

## Development

**Prerequisites:**

- Rust 1.91 (the toolchain is pinned via `rust-toolchain.toml` — `rustup` will pick it up automatically)
- A running Redis instance on `localhost:6379`

**Common commands:**

```bash
# Build
cargo build

# Run
cargo run

# Run tests
cargo test

# Lint
cargo clippy

# Check formatting
cargo fmt --check
```

**Pre-commit hooks:**

This repo uses [prek](https://github.com/j178/prek) (a Rust-based drop-in for the `pre-commit` framework) to run `cargo fmt` and `cargo clippy` before every commit.

```bash
# Install prek (macOS)
brew install prek

# Or via cargo
cargo install prek

# Install the git hooks
prek install

# Run all hooks manually
prek run --all-files
```

---

## Contributing

1. Fork the repository and create a branch from `master`
2. Install the pre-commit hooks (`prek install`) — they enforce formatting and lints locally
3. Make your changes and ensure `prek run --all-files` passes
4. Open a pull request — CI will run `cargo fmt`, `cargo clippy`, `cargo build`, and `cargo nextest` automatically

Please keep commits focused and PRs scoped to a single concern.

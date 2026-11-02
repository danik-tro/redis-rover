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
- Live metrics dashboard (`m`): ops/sec, memory, CPU, network, hit rate and clients charts, pause/reset, cluster node count
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

Connects to Redis at `localhost:6379` by default. Use `-u/--url` to pick a server and `--token` to type a password without it showing up in your shell history.

### TLS

Use a `rediss://` URL to connect over TLS. Server certificates are checked against the bundled Mozilla root certificates, so most managed services need no extra flags.

```bash
# Public CA (ElastiCache, Upstash, Azure, ...)
rrover -u rediss://my-cache.example.com:6379 --token

# Private CA
rrover -u rediss://redis.internal:6379 --tls-ca ca.pem

# Mutual TLS (client certificate)
rrover -u rediss://redis.internal:6379 --tls-ca ca.pem --tls-cert client.crt --tls-key client.key

# Skip certificate checks (local or dev servers only)
rrover -u rediss://localhost:6379 --insecure
```

| Flag | Meaning |
|---|---|
| `--tls-ca <PATH>` | PEM CA bundle used to verify the server. Replaces the bundled root certificates. |
| `--tls-cert <PATH>` / `--tls-key <PATH>` | PEM client certificate and private key for mutual TLS. Must be given together. |
| `--insecure` | Do not verify the server certificate. Cannot be combined with `--tls-ca`. |

The TLS flags only work with `rediss://` URLs.

| Provider | URL | Extra flags |
|---|---|---|
| AWS ElastiCache / MemoryDB (in-transit encryption) | `rediss://<endpoint>:6379` with `--token`, or `rediss://<user>:<password>@<endpoint>:6379` for ACL users | none |
| Upstash | `rediss://default:<password>@<name>.upstash.io:6379` | none |
| Azure Cache for Redis | `rediss://:<access-key>@<name>.redis.cache.windows.net:6380` | none |
| Redis Cloud | `rediss://default:<password>@<host>:<port>` | `--tls-ca redis_ca.pem`, and `--tls-cert`/`--tls-key` if mutual TLS is on |
| GCP Memorystore | `rediss://<ip>:6378` | `--tls-ca server-ca.pem` |
| Self-hosted with a self-signed certificate | `rediss://<host>:<port>` | `--tls-ca ca.pem` |

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

<p align="center">
<img src="https://raw.githubusercontent.com/danik-tro/redis-rover/master/assets/banner.svg" alt="redis-rover: a terminal UI for Redis" width="100%" />
</p>

# redis-rover

A Redis terminal-ui client written in rust 🦀

`redis-rover` (`rrover`) is a keyboard-driven TUI client for Redis. It lets you browse keyspaces, inspect and edit values, filter by pattern, and monitor server metrics — all from the terminal. Built with [ratatui](https://github.com/ratatui-org/ratatui) and powered by async I/O via Tokio.

![rrover keyspace view](https://raw.githubusercontent.com/danik-tro/redis-rover/master/assets/screenshot.png)

**Features:**

- Browse and filter Redis keys with type-colored badges; module types (`json`, `stream`, `MBbloom--`, ...) show their real name
- Paginate through large keyspaces via SCAN, and through large values page by page (no `HGETALL` freezes). A page turn costs the same on page 1 and page 10,000, even with 10M keys swept
- View strings, lists, sets, hashes and sorted sets; add, edit and delete keys and items, with a confirm step on delete
- RedisJSON documents pretty-printed; a document above 1 MiB opens as a preview (its top-level fields with type and size, or the first 100 items) instead of freezing the pane
- Switch between logical databases (`s`) and see the key count of the current one
- `--read-only` mode for browsing production safely
- TLS (`rediss://`), including private CAs and mutual TLS
- Live metrics dashboard (`m`): ops/sec, memory, CPU, network, hit rate and clients charts, pause/reset, cluster summary
- Pub/Sub tab (`b`): subscribe to channels and patterns, a live log you can pause, message detail with pretty JSON, publish, per-subscription stats and notifications that don't spam
- Fully configurable key bindings, colors, and styles via a config file (JSON5, JSON, YAML, TOML, INI)

---

## Installation

**From crates.io:**

```bash
cargo install redis-rover --locked
```

Requires Rust 1.97 or newer.

**Prebuilt binaries** for macOS (x86\_64, arm64), Linux (x86\_64, aarch64), and Windows are available on the [GitHub releases page](https://github.com/danik-tro/redis-rover/releases).

---

## Usage

```bash
rrover
```

Connects to Redis at `localhost:6379` by default. Use `-u/--url` to pick a server and `--token` to type a password without it showing up in your shell history.

| Flag | Meaning |
|---|---|
| `-u, --url <URL>` | Redis URL. Default `redis://localhost:6379`. A path picks the starting DB, e.g. `redis://localhost:6379/3`. |
| `--token` | Prompt for the password (input is hidden). |
| `--read-only` | Block every write. See [Read-only mode](#read-only-mode). |
| `--tls-ca`, `--tls-cert`, `--tls-key`, `--insecure` | TLS options. See [TLS](#tls). |
| `-t, --tick-rate <FLOAT>` / `-f, --frame-rate <FLOAT>` | Ticks and frames per second. Default 4. A frame is drawn only when something on screen changed, so an idle key list redraws a few times a minute. |

### Read-only mode

```bash
rrover -u rediss://prod-cache:6379 --token --read-only
```

With `--read-only`, every write is refused: add, edit, delete and TTL changes. The UI shows a notice instead of opening the edit form, the footer shows an `RO` badge and hides the write keys, and the background runner rejects any write that still gets through. Use it whenever you point `rrover` at production.

### Databases

Press `s` to open the DB picker. Move with `j`/`k` (or the arrow keys), or type the DB number, then press `Enter`. The footer shows the current DB and its key count, e.g. `db 3 · 120 keys`. You can also start on a DB with the URL path (`redis://host:6379/3`).

### RedisJSON

JSON keys show their document pretty-printed and coloured, like JSON strings. A document larger than `keyspace.json_preview_bytes` (1 MiB by default, measured with `MEMORY USAGE`) isn't read whole. The pane shows a preview instead:

- an object lists its top-level fields with their type and size, 100 per page (`n`/`p`). `Enter` on a field opens it, and the same limit applies again, so a large field opens as its own preview. `Esc` goes back one level.
- an array shows its first 100 items.
- any other value shows only its type and size.

The footer reads e.g. `14.21 MiB preview · fields 1–100 of 50,000 · n/p page · Enter opens`. `y` still copies the whole document, and asks first above 10 MiB. Previews use `$` paths and need RedisJSON 2 or later. With RedisJSON 1.x a document under the limit opens as usual, and a larger one shows only its size.

### Cluster

In cluster mode `rrover` connects to a single node. SCAN only sees that node's keys, so the key list is partial. A warning banner above the key table says so, and the footer key count reads `(this node)`. Cluster mode has only DB 0, so the DB picker is disabled. Full cluster browsing is planned after 0.1.0.

### Pub/Sub

Press `b` (from the keyspace or the dashboard) to open the Pub/Sub tab. `b` or `Esc` goes back.

- **Subscribe**: `a` opens a one-field popup. Text with `*`, `?` or `[` subscribes to a pattern (`PSUBSCRIBE`), anything else to a channel; `Tab` flips the kind. The left list also shows the server's other channels (`PUBSUB CHANNELS` with subscriber counts, `r` refreshes them); `Enter` on one subscribes to it. `d` unsubscribes, `c` clears the messages and stats.
- **Log**: the right pane shows the selected subscription's messages (time, concrete channel, one-line preview; binary payloads as `[binary N B]` plus hex). `Tab` moves the focus between the list and the log. The log follows the newest message; `Space` or `k` pauses it, and the title counts what arrived since (`paused · +128 new`). `Space` again, or `j` past the last row, follows again. Each subscription keeps the last 1,000 messages (8 MiB): if the paused rows are evicted, the view moves to the oldest message still kept.
- **Detail**: `Enter` in the log opens the message: channel, pattern, time, size (and `truncated (original …)` when it was cut at 64 KiB), and the payload in the same viewer as string values: JSON pretty-printed, `w` wrap, arrows pan, binary as a hex dump. `Esc` closes it.
- **Copy**: `y` copies the selected message's payload (binary as hex; the toast says `truncated` when the stored payload was cut), `Shift-y` its channel.
- **Stats** under the log: message count, bytes, average and largest size, rate over the last second and minute, time of the last message, and for patterns the five busiest channels.
- **Publish**: `Shift-p` opens a popup with a channel (prefilled from the selection) and a multi-line payload; `Ctrl-s` sends, and a toast says how many subscribers got it. Disabled with `--read-only`.
- **Notifications**: off by default, `Shift-n` toggles them per subscription. The first message toasts, later ones only bump the unread badge (per subscription, and `PubSub N` in the footer outside the tab) until the subscription has been quiet for 30 s. A subscription above 10 msg/s for 10 s is muted (`🔕`) until you press `Shift-n` again.

Subscriptions use their own connection, keep collecting while you're in other views, and resubscribe after a lost connection (the log marks the gap). They last for the session only. In cluster mode `PUBSUB CHANNELS` lists the connected node's channels only. Buffer sizes and notification thresholds are set in the `pubsub` section of the config.

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

## Keybindings

Press `?` in the app for the full list. Defaults:

**Keyspace**

| Key | Action |
|---|---|
| `j` / `k` | Scroll down / up |
| `n` / `p` | Next / previous page (of keys, or of the open value) |
| `Enter` | Open the selected value (or a field of a JSON preview) / confirm |
| `Esc` | Close popup / leave the value pane |
| `f` / `l` | Set / clear the filter pattern (glob, e.g. `user:*`) |
| `r` | Refresh keyspace |
| `a` | Add a new key |
| `i` | Add an item to the open collection |
| `e` | Edit the value, or the selected item |
| `d` | Delete the key, or the selected item (asks first) |
| `t` | Set TTL |
| `s` | Switch database |
| `m` | Open the metrics dashboard |
| `b` | Open Pub/Sub |
| `?` | Toggle help |
| `Ctrl-c` | Quit |

**Dashboard**

| Key | Action |
|---|---|
| `m` / `Esc` | Back to the keyspace |
| `Space` | Pause / resume |
| `r` | Reset history |
| `Tab` / `→` / `l` | Next chart |
| `Shift-Tab` / `←` / `h` | Previous chart |
| `a` | Show all charts |
| `b` | Open Pub/Sub |
| `?` | Toggle help |

**Pub/Sub**

| Key | Action |
|---|---|
| `b` / `Esc` | Back (`Esc` closes the message detail first) |
| `j` / `k` | Move in the focused pane (`k` in the log pauses it) |
| `Tab` | Switch focus between the list and the log |
| `Space` | Pause / follow the log |
| `Enter` | List: subscribe to a server channel. Log: open the message |
| `y` / `Shift-y` | Copy the message payload / its channel |
| `w`, `←` / `→` | Wrap / pan the message detail |
| `a` | Add a subscription (channel or pattern) |
| `d` | Unsubscribe |
| `c` | Clear messages and stats |
| `Shift-p` | Publish |
| `Shift-n` | Toggle notifications |
| `r` | Refresh server channels |

### Configuration

Key bindings, colors, styles and limits can be overridden in `config.json5` (or `.json`, `.yaml`, `.toml`, `.ini`) in the config directory. `rrover --version` prints where that is; set `REDIS_ROVER_CONFIG` to use another directory. See [`.config/config.json5`](.config/config.json5) for the defaults and their format.

For example, to preview RedisJSON documents only above 4 MiB:

```json5
{
  "keyspace": { "json_preview_bytes": 4194304 },
}
```

---

## Development

**Prerequisites:**

- Rust 1.97 (the toolchain is pinned via `rust-toolchain.toml` — `rustup` will pick it up automatically)
- A running Redis instance on `localhost:6379`
- Docker, for the integration tests only

**Common commands:**

```bash
# Build
cargo build

# Run
cargo run

# Run tests (no Docker or Redis needed)
cargo test

# Integration tests against a real Redis 7 in Docker
cargo test -- --ignored --test-threads=1

# Golden frames: rewrite tests/golden/*.txt after an intended UI change
UPDATE_GOLDEN=1 cargo test --test golden

# Benchmarks (no Redis needed)
cargo bench
scripts/bench.sh --quick    # writes target/bench-summary.md; compare with benches/baseline.md

# Lint
cargo clippy --all-targets --all-features -- -D warnings -W clippy::all -W clippy::pedantic

# Check formatting
cargo fmt --check
```

The integration tests (`tests/integration.rs`) start `redis:7-alpine` with [testcontainers](https://crates.io/crates/testcontainers), one container per test, so they don't touch your own Redis. They cover SCAN paging (including paging to page 1,000 over 1M keys, each page under 100 ms), DB switching, `--read-only`, value edits, Pub/Sub reconnects and TLS, and run in CI on every push, PR and nightly.

If a change touches the keyspace or rendering, run the benches before and after and compare with `benches/baseline.md` (the sol 01 baseline plus an "After sol 02" section). `scripts/bench.sh` without `--quick` also runs the 10M-key case, which takes a few minutes and about 2 GB of RAM.

Golden-frame tests (`tests/golden.rs`) draw the main screens from fixed data and compare their text with `tests/golden/<name>.txt`. A UI change makes them fail with the rows that differ; if the change is intended, rewrite the files with `UPDATE_GOLDEN=1` and review their diff in the PR.

The crate has a library target only so benches and tests can reach internal types. It is not a public API.

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
4. Open a pull request — CI will run `cargo fmt`, `cargo clippy`, `cargo build`, `cargo nextest` and the Docker integration tests automatically

Please keep commits focused and PRs scoped to a single concern.

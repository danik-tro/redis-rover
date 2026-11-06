# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First public release. Requires Rust 1.97 or newer.

### Added

- Keyspace browser: glob filter, client-side paging over one SCAN sweep, loading state.
- Value viewer for string, list, set, hash and sorted set keys, with bounded paging
  for large values instead of loading them whole.
- Add, edit and delete keys and collection items, and set TTLs. Deletes ask for
  confirmation.
- Real type names: RedisJSON keys show as `json`, streams as `stream`, and other module
  types (e.g. `MBbloom--`) by their own name. Their values show "viewer not supported yet".
- DB switching: `s` opens a DB picker, and the footer shows `db N · K keys`.
- `--read-only` flag: every write is refused, and the footer shows an `RO` badge.
- Cluster warning: in cluster mode a banner says SCAN only sees the connected node, and
  the footer key count is marked `(this node)`.
- TLS for `rediss://` URLs: bundled Mozilla roots, `--tls-ca`, `--tls-cert`/`--tls-key`
  for mutual TLS, and `--insecure`.
- `--token` prompts for the password, so it stays out of shell history.
- Live metrics dashboard (`m`): ops/sec, memory, CPU, network, hit rate and clients
  charts, pause / reset, cluster summary.
- Pub/Sub tab (`b`): subscribe to channels and glob patterns on a dedicated connection,
  list the server's channels (`PUBSUB CHANNELS` / `NUMSUB` / `NUMPAT`) and subscribe
  with `Enter`, and publish to any channel (`delivered to N subscribers`; disabled with
  `--read-only`). Each subscription keeps a bounded buffer (1,000 messages / 8 MiB,
  payloads cut at 64 KiB) and keeps collecting in the other views.
- Pub/Sub log: follows the newest message; `Space` or `k` pauses it (`paused · +N new`),
  `Tab` switches focus between the list and the log. `Enter` opens a message detail
  with channel, pattern, time, size and the payload pretty-printed as JSON or as a hex
  dump. `y` / `Shift-y` copy the payload / channel.
- Pub/Sub stats panel: message count, bytes, average and largest size, 1 s and 1 min
  rates, last message time, and the busiest concrete channels of a pattern.
- Pub/Sub resilience and notifications: reconnects with backoff and resubscribes
  (the log marks the gap); opt-in toasts per subscription (`Shift-n`) with a cooldown
  and an auto-mute for noisy subscriptions; unread badges per subscription and in the
  footer.
- Toasts now render above every mode and popup.
- `?` help overlay and a key-hint footer that follow the configured key bindings.
- Configurable key bindings, colors and styles (JSON5, JSON, YAML, TOML, INI).

### Fixed

- `cargo install` and release builds failed to compile (the panic handler used an old
  `human-panic` API).
- The config environment variables are `REDIS_ROVER_CONFIG`, `REDIS_ROVER_DATA` and
  `REDIS_ROVER_LOGLEVEL`, as the README says (they were `RROVER_*` before).

### Development

- Criterion benches without Redis (`cargo bench`): keyspace sweeps at 100k / 1M / 10M keys
  and rendering the key table and a 5 MB value. `scripts/bench.sh` records time and memory in
  `benches/baseline.md`.
- Integration tests against `redis:7-alpine` in Docker (`cargo test -- --ignored`), also run
  in CI on push, PR and nightly. Plain `cargo test` still needs neither Docker nor Redis.
- The crate now has a library target for the benches and tests; it is not a public API.

[0.1.0]: https://github.com/danik-tro/redis-rover/releases/tag/v0.1.0

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
- `?` help overlay and a key-hint footer that follow the configured key bindings.
- Configurable key bindings, colors and styles (JSON5, JSON, YAML, TOML, INI).

[0.1.0]: https://github.com/danik-tro/redis-rover/releases/tag/v0.1.0

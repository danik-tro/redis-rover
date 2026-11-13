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
- RedisJSON preview: a document above `keyspace.json_preview_bytes` (1 MiB) is no longer
  read whole. Objects list their top-level fields with type and size (100 per page, `Enter`
  opens a field, `Esc` goes back), arrays show their first 100 items. A 50 MB document
  previews in about 70 ms instead of freezing the pane. Needs RedisJSON 2 (`$` paths).
- Connection profiles: a `profiles` section in the config (`url`, `environment`, `db`,
  `read_only`, `tls`, `password_command`), used with `rrover <name>` or `--profile <name>`.
  `--db` picks the starting DB. An unknown profile exits with code 3 and lists the defined
  ones.
- `password_command` in a profile: the password comes from a command (`op`, `pass`,
  `security`). It runs before the TUI starts and its output is never logged. If it fails,
  `rrover` shows its stderr and exits with code 3.
- Environments (`local`, `staging`, `prod`, `unknown`), set by a profile or inferred
  from the host. The footer badge names the environment (`PROD · RO`), and a line in its
  colour runs along the top of the screen. The colours are configurable with
  `colors.env_*`.
- A preview of the exact Redis command before every write (`HSET "user:1" "name" "Ada"`,
  quoted like `redis-cli`). `y` runs it, and `n` / `Esc` cancels. `safety.preview`
  (`writes`, `destructive`, `off`) picks which writes are previewed; `prod` previews
  every write.
- On `prod`, deleting a key needs its name typed first.
- `Shift-r` toggles read-only during the session (asks first on `prod`).
- Sort the key list (`o`): a menu picks SCAN order, key, type, TTL or size, and picking
  the active column again reverses it. Metadata of keys never shown on a page is filled
  in the background; keys still unknown sort last.
- Type filter (`Shift-t`): show only some types. One type uses `SCAN … TYPE` on Redis 6+;
  older servers and several types are filtered on the client.
- Tree view (`v`): keys grouped by a separator (`keyspace.separator`, `:` by default),
  with group counts; `←` / `→` fold and unfold, `Enter` toggles a group.
- Load all (`Shift-a`): loads the rest of the keyspace in the background with a progress
  counter; `Esc` stops it. Above 2M keys it asks first, with a memory estimate.
- Go to the open key (`g o`): back to the last key opened with `Enter`, unfolding its
  groups, offering to clear a filter that hides it, and loading keys until it appears.
- `--print-target` prints the resolved target and where each setting came from (password
  masked), without connecting. `--probe` connects and reports PING, server version, role,
  cluster and RESP3 support. A pre-6.0 server reports `RESP3: no` and still exits 0.

### Changed

- `prod` and `unknown` servers (any host that isn't loopback or a unix socket, unless a
  profile says otherwise) start read-only. `--writable` lifts it for one run, and
  `--read-only` still wins. A plain `rrover` on localhost is writable as before.
- With no `--url` or profile, `rrover` connects to `$REDIS_URL` when it is set.
- Writes show their commands in a popup first (see Added). Deleting a key or an item
  confirms once, in that popup, instead of in the old "Delete …?" box, which is now used
  only with `safety.preview = "off"`.
- The footer's ` RO ` badge is now the environment badge (` LOCAL `, ` PROD · RO `, …).
- A partial `colors` table in the user config no longer fails to load.

- Paging deep into a big keyspace is as fast as the first page: a page turn no longer
  copies every key swept so far (52.95 ms → 3.3 µs at 1M keys, the same at 10M).
- The keyspace sweep uses about half the memory: each key name is stored once
  (1M keys: 108 MiB → 61 MiB; 10M keys: 1.2 GiB → 821 MiB).
- The footer builds its text once per `INFO` sample instead of on every frame, and a loaded
  page of keys is moved into the key table instead of copied.
- A slow value no longer holds up everything behind it: values load in their own task on
  their own connection, so page turns and writes go ahead while a value is loading. Moving
  through keys quickly cancels the fetches of the keys you passed, so only the last value is
  shown.
- The screen is redrawn only when something on it changed, not at every frame tick. An
  idle key list draws about 6 frames a minute (the footer's CPU % at most every 10 s); the
  dashboard redraws once per sample and the Pub/Sub tab on each new message and every
  second. `frames_drawn` is logged at debug level once a minute.

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
- `page_turn_deep` now times a page turn at 100k / 1M / 10M keys, and a `render/footer` bench
  was added. `benches/baseline.md` keeps the sol 01 baseline next to the after-sol-02 numbers;
  `scripts/bench.sh` now writes to `target/bench-summary.md` so a run never overwrites it.
- New integration tests: paging to page 1,000 over 1M keys, a filter change while a page is
  loading, deleted keys clamping the page, and a failed DB switch.
- `vergen` is gone: `build.rs` gets the git describe and build date with plain std, so the
  ~35 `gix` crates (and their security advisories) are no longer in the build tree.
- Golden-frame tests (`tests/golden.rs`): twelve screens drawn from fixed data and compared
  as text with `tests/golden/*.txt`. After an intended UI change, rewrite them with
  `UPDATE_GOLDEN=1 cargo test --test golden` and review the diff.
- Internal restructuring with no behaviour change: the largest files are split into module
  directories (`src/app/`, `widgets/keyspace/`, `widgets/pubsub/`, `widgets/dashboard/`,
  `redis_client/types/`), widgets draw from a per-frame snapshot instead of locking shared
  state, and the UI reads the time through an injected clock.
- Benches compile as one codegen unit (`[profile.bench]`), so moving code between modules
  doesn't shift their results.

[0.1.0]: https://github.com/danik-tro/redis-rover/releases/tag/v0.1.0

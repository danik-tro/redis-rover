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
  types (e.g. `MBbloom--`) by their own name. Values of those other types show "viewer
  not supported yet".
- DB switching: `s` opens a DB picker, and the footer shows `db N · K keys`.
- `--read-only` flag: every write is refused, and the footer shows an `RO` badge.
- Redis Cluster: `redis-cluster://` URLs and auto-detection, one keyspace swept over every
  master, reads and writes routed by slot, cross-slot renames refused with the slot numbers,
  a rescan after a failover or resharding, a per-node dashboard, Slowlog and MONITOR, and
  sharded Pub/Sub and live updates on the slot owner. If the nodes can't be reached, the
  session stays on one node, with a banner and a `(this node)` key count.
- Redis Sentinel: `redis+sentinel://` URLs and `sentinel` profile fields. Every connection
  follows the master through a failover, with the master named in the footer and a toast.
- TLS for `rediss://` URLs: bundled Mozilla roots, `--tls-ca`, `--tls-cert`/`--tls-key`
  for mutual TLS, and `--insecure`.
- `--token` prompts for the password, so it stays out of shell history.
- Live metrics dashboard (`m`): ops/sec, memory, CPU, network, hit rate and clients
  charts, pause / reset, cluster summary.
- MONITOR screen (`g m`): a live command log with a glob filter, pause (`+N new`), clear,
  full-line detail and a rate sparkline. It asks before starting because it costs server
  CPU (typed `monitor` on Prod), stops when you leave the screen unless
  `monitor.keep_running` is set, and samples above `monitor.max_rate` (`sampling 1/N`).
- Slowlog screen (`g s`): `SLOWLOG GET 128` with time / duration sort, full arguments, the
  two settings (or `unknown`), and `SLOWLOG RESET` through the write preview.
- Sharded Pub/Sub (Redis 7+): shard subscriptions and `SPUBLISH` in the Pub/Sub tab, with
  badges, notifications, stats and resubscribe after a lost connection. On older servers
  the option is dimmed with `needs Redis 7`.
- Read-only Streams view: entries newest first with paging on every server with streams,
  entry detail, consumer groups, consumers and pending entries. `y` copies as JSON.
- `g m`, `g s`, `g p` and `g d` jump between the server screens from any of them.
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
- Mouse (`ui.mouse`, on by default): click selects a key or value row, double-click opens
  a key, the wheel scrolls the pane under the pointer, and a bottom tab strip and popup
  buttons are clickable. Buttons press the popup's own key, and a typed confirm can't be
  confirmed with a click. Hold `Shift` for the terminal's own selection.
- Themes (`ui.theme`): `dark`, `light`, `high-contrast`, `monochrome` or a theme file whose
  omitted colors stay as in `dark`. Colors map to the terminal's depth (truecolor, 256,
  16), and `NO_COLOR` turns them off, with reverse video for selections and badges.
- Session restore: the filter, type filter, sort, tree folds, selected key, DB, pane split
  and subscription names come back per target, from a private `session.json` that never
  holds a value or a credential. `--no-restore` or `ui.restore = false` turns it off.
- Narrow terminals: compact key columns below 120 columns, one pane at a time below 70, a
  minimal footer below 50, and a `?` help that stacks its rows and pages.
- A hint bar (`ui.hints`) with the keys for the current focus, built from the help tables
  and the live key bindings, shortened by whole hints.
- Resizable panes: `Ctrl-←` / `Ctrl-→` (`GrowLeft` / `GrowRight`), 25 to 75 %.
- `docs/keybindings.md`, generated from the default config. A test fails when it is stale.
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
- Live updates of the open key: changes from other clients show up without a keypress,
  with the changed rows highlighted for 1.5 s. On Redis 6+ it uses `CLIENT TRACKING` on
  one extra connection; older servers, or ones that refuse it, are polled every
  `live.poll_interval_ms` (2 s). The value pane's title shows `● live`, `◌ polling 2s`
  or `✕ not live (reason)`, and a lost connection reconnects with backoff and fetches the
  key again. A deleted or expired key stays on screen, dimmed, with `key deleted ·
  HH:MM:SS`, and the cursor stays. `live.mode` (`auto`, `tracking`, `poll`, `off`) picks
  the mode or turns it off, and `--probe` reports what the server supports.
- Power edits, each previewed like every write:
  - Rename a key (`F2`): `RENAMENX`, keeping the TTL. A name that is taken is asked
    about again, naming the key it would replace, before `RENAME` overwrites it (on
    `prod` that name is typed).
  - Copy a key (`c`, `Tab` picks the target DB): `COPY … [DB n]` on Redis 6.2+, `DUMP` +
    `RESTORE` with the TTL before that. A taken name is asked about like a rename.
  - TTL as a duration: `90`, `90s`, `15m`, `2h30m`, `1d`, `+10m` / `-5m` (shift the
    current TTL, read under `WATCH`), `persist`. `0` says it deletes the key first.
  - Rename a hash field or a set / zset member (`F2` inside the value), keeping a zset
    score, in one script that refuses a name already there. Where `EVAL` is refused, plain
    commands that never overwrite run instead, with a warning.
  - Mark keys (`Space`, `Shift-m` for the page, `Shift-u` clears; `●` on the rows, `N
    marked` in the title) and delete them together with `d`: `UNLINK` in chunks of 500,
    the preview lists ten and `…and N more`, and `prod` asks for the count to be typed.
  - Edit in `$VISUAL` / `$EDITOR` / `vi` (`Shift-e`), in a private temp file that is
    removed on every way out, up to 10 MiB: a string value (the `SET` preview shows the
    size change); a whole hash, list, set or zset as JSON, written back as only what
    changed in one `MULTI` (a changed list is replaced and keeps its TTL); or, inside a
    value, the selected list element, hash field value, set member or zset score.
  - A rename or copy across cluster slots is refused up front.
- Undo / redo in every text input (`Ctrl-U` / `Ctrl-R`), listed in the help.
- `--print-target` prints the resolved target and where each setting came from (password
  masked), without connecting. `--probe` connects and reports PING, server version, role,
  cluster and RESP3 support. A pre-6.0 server reports `RESP3: no` and still exits 0.
- Tested server list: Redis 5, 6.0, 6.2, 7.2 and 8, Valkey 8, KeyDB, Dragonfly and Redis
  Stack, feature by feature, in `docs/compatibility.md`. It is generated from real runs.
- Static Linux builds (musl, x86\_64 and aarch64), an install script for macOS and Linux
  that checks the sha256, an experimental PowerShell installer, and a Homebrew formula.

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
- New `connection` config section: `response_timeout_ms` (5 s; copying a whole value, load
  all and the metadata backfill get six times that, at least 30 s) and
  `connect_timeout_ms` (5 s).
- The write preview of a STRING edit shows `SET … "KEEPTTL"`.
- The TTL popup takes durations (see Added); an empty or unreadable TTL no longer
  removes the TTL, it says what it accepts. `0` now means `EXPIRE … 0`, and `persist`
  removes the TTL.
- The help shows function keys as `F2` instead of `F(2)`, flows into columns in a short
  window, and pages with `n` / `p` when it still doesn't fit.
- A write of several commands runs in one `MULTI` / `EXEC`, and the preview shows it. A
  LIST element is edited or deleted by a short script (`EVAL`) that first checks the
  element is still the one shown; without scripting (an ACL) the check is a separate
  `LINDEX`. A new STRING key is created with `SET … NX`.
- `rrover <profile> --url <other>`: when `--url` replaces a profile's `url`, the
  profile's `environment` and `read_only: false` apply only if the new host doesn't call
  for more protection. `rrover local --url redis://prod-db:6379` now starts as `UNKNOWN`
  and read-only (it was `LOCAL` and writable). `--print-target` names the source.
- The config and data directories are named after this project
  (`io.github.danik-tro.redis-rover` on macOS, `danik-tro\redis-rover` on Windows; no
  change on Linux). An existing directory under the old name keeps being used until
  the new one exists. `rrover --version` prints both paths.

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
- A write (edit, delete, TTL, new key, add / remove an element) no longer rescans the
  keyspace: the list takes that one key's change and stays where it is, on any page, and
  a running `Shift-a` load goes on. A new key is shown on its page, at the end of what is
  loaded. `r` still rescans, to pick up what other clients wrote.
- Holding `j` / `k` on the key list fetches the value once the cursor rests (about
  100 ms), not for every row passed. A single press fetches at once, as before.
- A filter that matches few keys of a large keyspace is scanned in slices of at most 50
  `SCAN` steps or 200 ms. Writes, a DB switch and a new filter run between slices, the
  Keys title shows `scanning… ~2.1M checked`, and the `COUNT` hint grows (up to 10,000)
  while steps find nothing.
- The text viewer draws only the lines on screen. A frame of a wrapped 5 MB value took
  78 ms and takes about 0.1 ms; the wrapped rows are counted once per value and width.

### Fixed

- A broken config file (unknown theme, command or key, bad syntax) stops the app with
  its message and exit code 3, as `--print-target` does. It used to crash with a panic
  report.
- The help lists a command's keys in a stable order, shortest first (#62).
- At 80 columns the key table shows key names, and its bottom border no longer cuts the
  page range (#59).
- The key list, values and writes could stop responding for the rest of the session after
  a burst of key presses while a request was slow (holding `j` during a long SCAN). No
  request is dropped any more.
- Editing a STRING keeps its TTL (`SET … KEEPTTL`; `PTTL` + `SET … PX` before Redis 6.0).
  It used to make the key permanent.
- A STRING that isn't valid UTF-8 can no longer be opened in the editor, where saving
  would have overwritten its bytes.
- Pasted text is typed into the focused text field only. On the key list or in a `y` / `n`
  question it used to run as key presses (`d`, then `y`, deleted the selected key).
- Commands no longer time out after 500 ms. See `connection` under Changed.
- A server that refuses `MEMORY USAGE` (an ACL, a managed service) still lists its keys;
  the size column shows `—`.
- The log file is created readable by its owner only (0600), and it no longer contains
  written values or Pub/Sub payloads.
- Key names, collection members and channel names that aren't valid UTF-8 no longer
  fail the key list ("Failed to fetch keys") or the value. They are shown with
  `redis-cli` escapes (`\xff\xfe`), and such a key can be opened, given a TTL and
  deleted. A binary element can be deleted but not edited.
- A LIST edit or delete no longer changes the wrong element after another client pushed
  or popped ("the list changed at this index, reload it"), and a lost connection can no
  longer leave a delete marker in a list or drop a SET member that was being replaced.
- Control and invisible characters in a key name (line breaks, escape sequences, bidi
  overrides, zero-width spaces) are shown as escapes (`\n`, `\x1b`, `\u{202e}`) in the
  key list, the value tables, the write preview and the typed confirm on Prod, so two
  different names never look the same.
- `--probe` and startup errors escape text from the server, which could otherwise send
  escape sequences to the terminal.
- A mistyped key in `keybindings` is reported as a configuration error (exit code 3)
  instead of a crash, and a missing user config is no longer logged as an error.
- `cargo install` and release builds failed to compile (the panic handler used an old
  `human-panic` API).
- The config environment variables are `REDIS_ROVER_CONFIG`, `REDIS_ROVER_DATA` and
  `REDIS_ROVER_LOGLEVEL`, as the README says (they were `RROVER_*` before).
- A wrapped text longer than 65,535 rows can be scrolled to its end.
- SCAN cursors are 64-bit on every platform (a 32-bit build could not hold one).
- A malformed `--url` no longer prints its password in the error (#92).
- Live updates work on Dragonfly, which dropped other clients' changes with `NOLOOP`.
- Copying a key to another DB works on servers whose `COPY` has no `DB` option.

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
- A release runs only if its tag matches `Cargo.toml` and the compatibility suite passes.
  It stays a draft until every archive is uploaded and the crate is published.

[0.1.0]: https://github.com/danik-tro/redis-rover/releases/tag/v0.1.0

# Benchmark baseline

Written by `scripts/bench.sh`. Synthetic data, no Redis. Sol 02 and later are
judged against these numbers (a regression above 10 % needs an explanation in
the PR).

| | |
|---|---|
| Date | 2026-11-06T23:14Z |
| Commit | `8cd8221 (+ uncommitted changes)` |
| Machine | Apple M4 Pro, 48 GiB RAM, arm64 |
| OS | macOS 26.3.1 |
| Toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Profile | `cargo bench` (release) |

Time is criterion's median with its 95 % confidence interval. Memory is
counted by a wrapping allocator in one extra run outside the timing:
**retained** = heap still held afterwards, **peak** = highest heap above the
start, **allocated** = every byte allocated, freed or not.

| Bench | Time (median, 95 % CI) | 16 ms frame budget | Memory | What memory means |
|---|---|---|---|---|
| `merge_batch/100k` | 4.09 ms (4.08 ms – 4.09 ms) |  | retained 9.0 MiB · peak 9.5 MiB · allocated 20.5 MiB | retained = whole sweep |
| `merge_batch/1M` | 74.23 ms (73.99 ms – 75.19 ms) |  | retained 108.0 MiB · peak 133.8 MiB · allocated 239.0 MiB | retained = whole sweep |
| `merge_batch/10M` | 1.23 s (1.22 s – 1.24 s) |  | retained 1.2 GiB · peak 1.2 GiB · allocated 2.6 GiB | retained = whole sweep |
| `page_turn_deep/1M` | 52.95 ms (52.65 ms – 53.81 ms) |  | retained 103.5 MiB · peak 103.5 MiB · allocated 103.5 MiB | allocated = one snapshot clone |
| `apply_sweep/1M` | 18.39 ms (18.16 ms – 18.62 ms) |  | retained -103.5 MiB · peak 0 B · allocated 0 B | retained < 0: the old sweep is freed |
| `render/key_table_100` | 132.53 µs (132.22 µs – 133.08 µs) | ✅ ≤ 16 ms | retained 0 B · peak 120.8 KiB · allocated 150.2 KiB | allocated = one frame |
| `render_text/5MB_plain_wrap` | 78.83 ms (78.51 ms – 79.21 ms) | ❌ 5× over | retained 0 B · peak 10.2 MiB · allocated 10.3 MiB | allocated = one frame |
| `render_text/5MB_plain_nowrap` | 6.08 ms (5.90 ms – 6.26 ms) | ✅ ≤ 16 ms | retained 0 B · peak 10.2 MiB · allocated 10.2 MiB | allocated = one frame |
| `render_text/5MB_json_wrap` | 188.03 ms (187.66 ms – 189.05 ms) | ❌ 12× over | retained 0 B · peak 134.7 MiB · allocated 134.7 MiB | allocated = one frame |
| `text_cache/5MB_plain` | 4.38 ms (4.36 ms – 4.51 ms) |  | retained 17.5 MiB · peak 17.5 MiB · allocated 25.5 MiB | retained = the cached lines |
| `text_cache/5MB_json` | 137.07 ms (134.47 ms – 138.51 ms) |  | retained 169.4 MiB · peak 250.8 MiB · allocated 475.9 MiB | retained = the cached lines |

## What each bench does

- `merge_batch/N`: build a sweep from N key names (~20 bytes each), merged in
  SCAN-sized batches (`scan_count` = 100, the default) with `merge_batch`.
- `page_turn_deep/1M`: what `RedisEvent::FetchKeys` does before any I/O today:
  `keyspace_state.lock().clone()` of a 1M-key sweep, then `next_page()`. The
  clone is dropped inside the timing. This is the number sol 02 must bring down.
- `apply_sweep/1M`: write an advanced 1M-key sweep back into the shared state.
- `render/key_table_100`: one frame of `KeySpaceWidget` with 100 keys on a
  120×40 `TestBackend`.
- `render_text/*`: one frame of a 5 MB value in the text viewer (`TextView`),
  with wrap on and off, plain text and pretty JSON.
- `text_cache/*`: building the viewer's lines for a 5 MB value (`TextCache`),
  done once when the value loads, not per frame.

## Sol 02 in progress

Measured on the same machine as the baseline above, during sol 02. The
baseline table is still the reference; the full "after sol 02" run replaces
this section when the sol is done.

### Page turn without the clone (#45)

`page_turn_deep/N` now times what one `n` does under the keyspace lock:
`next_page`, take a `SweepRequest`, merge one 100-key SCAN answer
(`apply_result`), copy the page's names. Commit `cb26f56`, still the old
`HashSet<String>` sweep store.

| Bench | Baseline (sol 01) | Now | Memory now |
|---|---|---|---|
| `page_turn_deep/100k` | – | 10.20 µs (8.21 µs – 11.60 µs) | allocated 5.5 KiB |
| `page_turn_deep/1M` | 52.95 ms (one full clone) | 10.47 µs (7.81 µs – 12.36 µs) | allocated 5.9 KiB |
| `page_turn_deep/10M` | – | 27.85 µs (27.23 µs – 28.50 µs) | allocated 6.3 KiB |
| `merge_batch/1M` | 74.23 ms | 74.55 ms (74.17 ms – 74.88 ms) | retained 108.0 MiB |
| `merge_batch/10M` | 1.23 s | 1.25 s (1.24 s – 1.27 s) | retained 1.2 GiB |

### Choosing the sweep store (#46)

A standalone program merged synthetic key names in 100-key batches into
each candidate store. It used the same counting allocator as the benches,
built in release mode, and timed with the median of 7 runs. "Slowest
batch" is the longest single 100-key merge, which is the longest time the
keyspace lock is held.

| Store | Names | 1M retained | 1M merge | 1M slowest batch | 10M retained | 10M merge | 10M slowest batch |
|---|---|---|---|---|---|---|---|
| current: `Vec<String>` + `HashSet<String>` | ~20 B | 108.0 MiB | 78.2 ms | 34.6 ms | 1249.3 MiB | 1302.2 ms | 466.3 ms |
| A: `Vec<String>` + `HashTable<u32>` | ~20 B | 52.7 MiB | 49.4 ms | 27.1 ms | 757.4 MiB | 784.1 ms | 302.3 ms |
| **A2: `Vec<String>` + `HashTable<(u32, u32)>`** | ~20 B | **60.7 MiB** | **21.0 ms** | **5.1 ms** | **821.4 MiB** | **243.3 ms** | **33.2 ms** |
| B: `Vec<String>` + `HashMap<u64, u32>` | ~20 B | 76.7 MiB | 22.9 ms | 4.1 ms | 949.4 MiB | 251.2 ms | 31.9 ms |
| C: `Vec<Arc<str>>` + `HashSet<Arc<str>>` | ~20 B | 82.2 MiB | 103.3 ms | 41.8 ms | 903.6 MiB | 1709.6 ms | 428.4 ms |
| current | ~30 B | 131.9 MiB | 94.8 ms | – | – | – | – |
| A | ~30 B | 65.0 MiB | 60.4 ms | – | – | – | – |
| A2 | ~30 B | 73.0 MiB | 22.0 ms | – | – | – | – |
| B | ~30 B | 89.0 MiB | 23.8 ms | – | – | – | – |
| C | ~30 B | 95.8 MiB | 125.2 ms | – | – | – | – |

The AC2 limit is 60 % of the baseline: 64.8 MiB at 1M with ~20 B names, or
79.1 MiB with ~30 B names. **A2 is the choice:**

- **Memory:** 56 % of the baseline with ~20 B names, 55 % with ~30 B names.
- **Speed:** merges are 3.7× faster than today, and the worst lock hold at
  10M drops from 466 ms to 33 ms.
- **Why not A:** A is smaller, but each table resize re-hashes every key
  name (302 ms under the lock at 10M). A2 keeps a 32-bit hash beside each
  index, so resizing never touches the names.
- **Why not B or C:** both miss AC2.
- **Collisions:** every lookup compares the 32-bit hash, then the full name.

At 10M the saving is smaller (66 %), because spare capacity in
`scanned: Vec<String>` (24 B per slot, grown to 2^24 slots) is the same for
every store.

### Sweep store A2 in place (#46)

`cargo bench --bench keyspace` with `RROVER_BENCH_LARGE=1`, after
`KeyspaceState::seen` became a `HashTable<SeenSlot { index: u32, hash: u32 }>`.

| Bench | Baseline (sol 01) | Now | Memory baseline → now |
|---|---|---|---|
| `merge_batch/100k` | 4.09 ms | 1.35 ms (1.34 ms – 1.35 ms) | retained 9.0 MiB → 5.7 MiB |
| `merge_batch/1M` | 74.23 ms | 23.42 ms (23.11 ms – 23.58 ms) | retained 108.0 MiB → **60.7 MiB (56 %)** |
| `merge_batch/10M` | 1.23 s | 293.22 ms (289.14 ms – 297.37 ms) | retained 1.2 GiB → 821.4 MiB |
| `page_turn_deep/100k` | – | 3.58 µs (3.29 µs – 3.77 µs) | allocated 3.9 KiB |
| `page_turn_deep/1M` | 52.95 ms | **3.49 µs** (3.20 µs – 3.70 µs) | allocated 103.5 MiB → 4.1 KiB |
| `page_turn_deep/10M` | – | 4.31 µs (4.23 µs – 4.45 µs) | allocated 4.3 KiB |

- **AC1:** a page turn at 1M is about 15,000× faster than the baseline.
  100k, 1M and 10M are within 1.24× of each other (the old store gave
  2.7×, because of cache misses in a 400 MB hash set).
- **AC2:** the 1M sweep keeps 56 % of the baseline memory, under the 60 %
  limit.

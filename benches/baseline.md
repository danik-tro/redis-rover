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

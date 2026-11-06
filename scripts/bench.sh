#!/usr/bin/env bash
# Run the benches and write a summary to benches/baseline.md.
#
#   scripts/bench.sh            all benches, including 10M keys (slow, ~2 GB RAM)
#   scripts/bench.sh --quick    skip 10M
#   OUT=path scripts/bench.sh   write the summary somewhere else
#
# Times come from criterion (target/criterion/**/new/), memory from the
# benches' counting allocator (target/criterion/rrover-memory/).
set -euo pipefail

cd "$(dirname "$0")/.."

large=1
if [[ "${1:-}" == "--quick" ]]; then
    large=0
    shift
fi
out="${OUT:-benches/baseline.md}"
target="${CARGO_TARGET_DIR:-target}"

command -v python3 >/dev/null || { echo "bench.sh needs python3" >&2; exit 1; }

started=$(date +%s)
rm -rf "$target/criterion/rrover-memory"

RROVER_BENCH_LARGE=$large cargo bench --bench keyspace --bench render -- "$@"

commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
if [[ -n "$(git status --porcelain -- src benches Cargo.toml Cargo.lock 2>/dev/null)" ]]; then
    commit="$commit (+ uncommitted changes)"
fi
case "$(uname -s)" in
    Darwin)
        cpu=$(sysctl -n machdep.cpu.brand_string)
        mem_bytes=$(sysctl -n hw.memsize)
        os="macOS $(sw_vers -productVersion)"
        ;;
    *)
        cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//')
        mem_bytes=$(( $(grep MemTotal /proc/meminfo | awk '{print $2}') * 1024 ))
        os="$(uname -sr)"
        ;;
esac

python3 - "$target" "$out" "$started" "$commit" "$cpu" "$mem_bytes" "$os" \
    "$(uname -m)" "$(rustc -V)" "$(date -u +%Y-%m-%dT%H:%MZ)" "$large" <<'PY'
import json, os, sys
from pathlib import Path

(target, out, started, commit, cpu, mem_bytes, os_name, arch, rustc, date,
 large) = sys.argv[1:]
started = int(started)
crit = Path(target) / "criterion"

BUDGET_NS = 16e6  # 16 ms per frame

ORDER = [
    "merge_batch/100k", "merge_batch/1M", "merge_batch/10M",
    "page_turn_deep/1M", "apply_sweep/1M",
    "render/key_table_100",
    "render_text/5MB_plain_wrap", "render_text/5MB_plain_nowrap",
    "render_text/5MB_json_wrap",
    "text_cache/5MB_plain", "text_cache/5MB_json",
]
FRAME = ("render/", "render_text/")

def fmt_ns(ns):
    for unit, size in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
        if ns >= size:
            return f"{ns / size:.2f} {unit}"
    return f"{ns:.0f} ns"

def fmt_bytes(n):
    sign = "-" if n < 0 else ""
    n = abs(n)
    for unit, size in (("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)):
        if n >= size:
            return f"{sign}{n / size:.1f} {unit}"
    return f"{sign}{n} B"

times = {}
for bench in crit.glob("**/new/benchmark.json"):
    if bench.stat().st_mtime < started:
        continue  # left over from an older run
    meta = json.loads(bench.read_text())
    est = json.loads((bench.parent / "estimates.json").read_text())
    times[meta["full_id"]] = (
        est["median"]["point_estimate"],
        est["median"]["confidence_interval"]["lower_bound"],
        est["median"]["confidence_interval"]["upper_bound"],
    )

memory = {}
for f in (crit / "rrover-memory").glob("*.json"):
    m = json.loads(f.read_text())
    memory[m["id"]] = m

ids = [i for i in ORDER if i in times or i in memory]
ids += sorted(set(times) - set(ORDER))

rows = []
for bid in ids:
    t = times.get(bid)
    m = memory.get(bid)
    time = f"{fmt_ns(t[0])} ({fmt_ns(t[1])} – {fmt_ns(t[2])})" if t else "–"
    if bid.startswith(FRAME) and t:
        budget = "✅ ≤ 16 ms" if t[0] <= BUDGET_NS else f"❌ {t[0] / BUDGET_NS:.0f}× over"
    else:
        budget = ""
    if m:
        mem = (f"retained {fmt_bytes(m['retained'])} · peak {fmt_bytes(m['peak'])}"
               f" · allocated {fmt_bytes(m['allocated'])}")
        note = m["note"]
    else:
        mem, note = "–", ""
    rows.append(f"| `{bid}` | {time} | {budget} | {mem} | {note} |")

if large != "1":
    rows.insert(2, "| `merge_batch/10M` | not run (`--quick`) | | | |")

text = f"""# Benchmark baseline

Written by `scripts/bench.sh`. Synthetic data, no Redis. Sol 02 and later are
judged against these numbers (a regression above 10 % needs an explanation in
the PR).

| | |
|---|---|
| Date | {date} |
| Commit | `{commit}` |
| Machine | {cpu}, {int(mem_bytes) / (1 << 30):.0f} GiB RAM, {arch} |
| OS | {os_name} |
| Toolchain | `{rustc}` |
| Profile | `cargo bench` (release) |

Time is criterion's median with its 95 % confidence interval. Memory is
counted by a wrapping allocator in one extra run outside the timing:
**retained** = heap still held afterwards, **peak** = highest heap above the
start, **allocated** = every byte allocated, freed or not.

| Bench | Time (median, 95 % CI) | 16 ms frame budget | Memory | What memory means |
|---|---|---|---|---|
""" + "\n".join(rows) + """

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
"""
Path(out).write_text(text)
print(f"wrote {out}")
PY

#!/usr/bin/env bash
# Run tests/compat.rs against every image in tests/compat/images.txt, one at a
# time, and write one result file per image.
#
#   scripts/compat.sh [label ...]     only these labels
#   OUT=dir scripts/compat.sh         results directory (default target/compat)
#   RROVER_COMPAT_RECORD=1            don't compare with expected.json
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
out="${OUT:-target/compat}"
mkdir -p "$out"
status=0
while read -r label image; do
    case "$label" in '' | \#*) continue ;; esac
    if [[ $# -gt 0 && " $* " != *" $label "* ]]; then
        continue
    fi
    echo "== $label ($image)"
    if ! RROVER_TEST_IMAGE="$image" RROVER_COMPAT_LABEL="$label" RROVER_COMPAT_OUT="$out" \
        cargo test --locked --test compat -- --ignored --test-threads=1 --nocapture compat_matrix \
        2>&1 | grep -E "^$label |differs|expected .*, got|^test result"; then
        status=1
    fi
done < tests/compat/images.txt
exit "$status"

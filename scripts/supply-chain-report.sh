#!/usr/bin/env bash
# Builds report.md from the cargo-deny / cargo-audit output of the CI
# "Supply chain" job. Reads DENY_CODE / AUDIT_CODE (exit codes) and
# deny.txt / audit.txt. Always exits 0: these checks are informational.
set -uo pipefail

DENY_CODE="${DENY_CODE:-1}"
AUDIT_CODE="${AUDIT_CODE:-1}"
OUT="${OUT:-report.md}"
# GitHub comments are capped at 65,536 characters; keep each section well under half.
MAX_BYTES=30000

section() { # title, file, exit code
  local title="$1" file="$2" code="$3" icon="✅"
  [[ "$code" != 0 ]] && icon="⚠️"
  echo "<details><summary>${icon} <b>${title}</b> (exit ${code})</summary>"
  echo
  echo '```text'
  if [[ -s "$file" ]]; then
    head -c "$MAX_BYTES" "$file"
    (( $(wc -c < "$file") > MAX_BYTES )) && printf '\n… output truncated, see the run log'
  else
    echo "(no output)"
  fi
  echo
  echo '```'
  echo "</details>"
  echo
}

failed=()
[[ "$DENY_CODE" != 0 ]] && failed+=("cargo-deny")
[[ "$AUDIT_CODE" != 0 ]] && failed+=("cargo-audit")

{
  echo "## Supply chain report"
  echo
  if (( ${#failed[@]} == 0 )); then
    echo "✅ No findings from cargo-deny (bans, licenses, sources) or cargo-audit (advisories)."
  else
    echo "⚠️ Findings from: **${failed[*]}**."
  fi
  echo
  section "cargo deny check bans licenses sources" deny.txt "$DENY_CODE"
  section "cargo audit" audit.txt "$AUDIT_CODE"
  echo "<sub>These checks don't block merge."
  if [[ -n "${GITHUB_RUN_ID:-}" ]]; then
    echo "[Run log](${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID})"
  fi
  echo "</sub>"
} > "$OUT"

for tool in ${failed[@]+"${failed[@]}"}; do
  echo "::warning title=${tool}::${tool} reported findings (non-blocking); see the job summary."
done

clean=false
(( ${#failed[@]} == 0 )) && clean=true
[[ -n "${GITHUB_OUTPUT:-}" ]] && echo "clean=${clean}" >> "$GITHUB_OUTPUT"
exit 0

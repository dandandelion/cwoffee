#!/usr/bin/env bash
# memory.sh — the memory gate from PLAN.md §8.
#
# Measures PSS (proportional set size), NOT RSS. RSS double-counts pages shared
# between processes, which makes a multi-process Chromium app look worse than it
# is and gives nobody a defensible number. PSS is the fair metric.
#
#   usage: ./scripts/memory.sh [pattern] [pss_limit_mb]
#   exit:  0 under budget, 1 over budget, 2 no matching process
#
# Example:
#   ./scripts/memory.sh cowry 400
#   ./scripts/memory.sh qoder 0      # 0 = report only, never fail

set -uo pipefail

PATTERN="${1:-cowry}"
LIMIT_MB="${2:-400}"

shopt -s nullglob
pids=( $(pgrep -f -- "$PATTERN" 2>/dev/null) )

# never measure ourselves
pids=( "${pids[@]/$$/}" )

if [ "${#pids[@]}" -eq 0 ]; then
  echo "memory.sh: no processes match '$PATTERN'" >&2
  exit 2
fi

total_pss=0
total_rss=0
count=0

printf "%-8s %-28s %10s %10s\n" "PID" "COMM" "PSS" "RSS"
printf "%-8s %-28s %10s %10s\n" "-------" "----------------------------" "--------" "--------"

for p in "${pids[@]}"; do
  rollup="/proc/$p/smaps_rollup"
  [ -r "$rollup" ] || continue

  pss=$(awk '/^Pss:/ {s+=$2} END {print s+0}' "$rollup" 2>/dev/null)
  rss=$(awk '/^Rss:/  {s+=$2} END {print s+0}' "$rollup" 2>/dev/null)
  comm=$(tr -d '\0' < "/proc/$p/comm" 2>/dev/null || echo "?")

  total_pss=$(( total_pss + ${pss:-0} ))
  total_rss=$(( total_rss + ${rss:-0} ))
  count=$(( count + 1 ))

  printf "%-8s %-28s %8s MB %8s MB\n" "$p" "${comm:0:28}" \
    "$(awk -v v="$pss" 'BEGIN{printf "%.1f", v/1024}')" \
    "$(awk -v v="$rss"  'BEGIN{printf "%.1f", v/1024}')"
done

pss_mb=$(awk -v v="$total_pss" 'BEGIN{printf "%.1f", v/1024}')
rss_mb=$(awk -v v="$total_rss" 'BEGIN{printf "%.1f", v/1024}')

echo "------------------------------------------------------------"
printf "processes: %s\n" "$count"
printf "PSS total: %s MB   <-- the metric we gate on\n" "$pss_mb"
printf "RSS total: %s MB   (inflated by shared pages; for comparison only)\n" "$rss_mb"

if [ "$LIMIT_MB" -le 0 ] 2>/dev/null; then
  exit 0
fi

if awk -v a="$pss_mb" -v b="$LIMIT_MB" 'BEGIN{exit !(a>b)}'; then
  echo "FAIL: PSS ${pss_mb} MB > budget ${LIMIT_MB} MB" >&2
  echo "      See PLAN.md §9 — virtualization is the first thing to check." >&2
  exit 1
fi

echo "OK: PSS ${pss_mb} MB <= budget ${LIMIT_MB} MB"
exit 0

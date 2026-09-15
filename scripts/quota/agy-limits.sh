#!/usr/bin/env bash
# Print Antigravity (agy) five-hour / weekly usage via `agy -p /usage`.
# Ops helper — mirrors Antigravity harness five-hour probe parsing.
# Usage:
#   agy-limits
#   agy-limits --json
set -euo pipefail

JSON=0
if [[ "${1:-}" == "--json" ]]; then
  JSON=1
fi

if ! command -v agy >/dev/null; then
  echo "error: agy not found on PATH" >&2
  exit 1
fi

raw="$(agy -p /usage </dev/null 2>/dev/null || true)"
if [[ -z "$raw" ]]; then
  echo "error: agy /usage returned no output" >&2
  exit 2
fi

export AGY_LIMITS_JSON="$JSON"
export AGY_LIMITS_RAW="$raw"
python3 - <<'PY'
import json, os, sys

raw = os.environ["AGY_LIMITS_RAW"]
want_json = os.environ.get("AGY_LIMITS_JSON") == "1"

rows = []
for line in raw.splitlines():
    line = line.rstrip("\n")
    if not line.strip():
        continue
    cols = [c.strip() for c in line.split("\t")] if "\t" in line else line.split()
    if len(cols) < 3:
        continue
    label = " ".join(cols[1:-2]) if len(cols) >= 4 else cols[1]
    pct = None
    reset = None
    for c in cols:
        if c.endswith("%"):
            try:
                pct = int(c[:-1])
            except ValueError:
                pass
        if "T" in c and c.endswith("Z"):
            reset = c
    if pct is None:
        continue
    low = label.lower()
    kind = "fiveHour" if "five hour" in low else ("weekly" if "week" in low else "other")
    rows.append({
        "group": cols[0],
        "label": label,
        "kind": kind,
        "remainingPercent": pct,
        "usedPercent": max(0, 100 - pct),
        "resetsAt": reset,
    })

five = next((r for r in rows if r["kind"] == "fiveHour" and ("claude" in r["group"].lower() or "gpt" in r["group"].lower())), None)
if five is None:
    five = next((r for r in rows if r["kind"] == "fiveHour"), None)

if want_json:
    print(json.dumps({"fiveHour": five, "rows": rows, "raw": raw}, indent=2, sort_keys=True))
else:
    print("Antigravity (agy) usage")
    for r in rows:
        print(f"  [{r['group']}] {r['label']}: remaining {r['remainingPercent']}%  resets {r['resetsAt'] or '?'}")
PY

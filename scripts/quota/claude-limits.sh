#!/usr/bin/env bash
# Print Claude Code five-hour ("Current session") usage via `claude -p /usage`.
# Usage:
#   claude-limits
#   claude-limits --json
set -euo pipefail

JSON=0
if [[ "${1:-}" == "--json" ]]; then
  JSON=1
fi

if ! command -v claude >/dev/null; then
  echo "error: claude not found on PATH" >&2
  exit 1
fi

raw="$(claude -p /usage --output-format json </dev/null 2>/dev/null || true)"
if [[ -z "$raw" ]]; then
  echo "error: claude /usage returned no output" >&2
  exit 2
fi

export CLAUDE_LIMITS_JSON="$JSON"
export CLAUDE_LIMITS_RAW="$raw"
python3 - <<'PY'
import json, os, re, sys

raw = os.environ["CLAUDE_LIMITS_RAW"]
want_json = os.environ.get("CLAUDE_LIMITS_JSON") == "1"

text = raw
try:
    obj = json.loads(raw)
    if isinstance(obj, dict) and isinstance(obj.get("result"), str):
        text = obj["result"]
except Exception:
    pass

session = None
week = None
session_reset = None
week_reset = None
for line in text.splitlines():
    if "Current session" in line:
        m = re.search(r"(\d+)\s*%", line)
        if m:
            session = int(m.group(1))
        if "resets " in line:
            session_reset = line.split("resets ", 1)[1].strip()
    if "Current week" in line:
        m = re.search(r"(\d+)\s*%", line)
        if m:
            week = int(m.group(1))
        if "resets " in line:
            week_reset = line.split("resets ", 1)[1].strip()

if session is None:
    print("error: could not parse Current session from claude /usage", file=sys.stderr)
    sys.exit(2)

payload = {
    "fiveHour": {"usedPercent": session, "resetsAtText": session_reset},
    "weekly": {"usedPercent": week, "resetsAtText": week_reset} if week is not None else None,
    "raw": text,
}
if want_json:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    print("Claude Code usage")
    print(f"  five-hour (session): used {session}%" + (f"  resets {session_reset}" if session_reset else ""))
    if week is not None:
        print(f"  weekly:             used {week}%" + (f"  resets {week_reset}" if week_reset else ""))
PY

#!/usr/bin/env bash
# Print Codex ChatGPT rate-limit usage via app-server (same source as /status).
# In-repo copy for OpenResearch ops + harness probing.
# Requires: logged-in Codex (`codex login`), `codex` + `python3` on PATH.
# Usage:
#   codex-limits
#   codex-limits --json
set -euo pipefail

JSON=0
if [[ "${1:-}" == "--json" ]]; then
  JSON=1
fi

if ! command -v codex >/dev/null; then
  echo "error: codex not found on PATH" >&2
  exit 1
fi
if ! command -v python3 >/dev/null; then
  echo "error: python3 required" >&2
  exit 1
fi

export CODEX_LIMITS_JSON="$JSON"
python3 - <<'PY'
import json, os, select, subprocess, sys, time

want_json = os.environ.get("CODEX_LIMITS_JSON") == "1"

proc = subprocess.Popen(
    ["codex", "app-server", "--stdio"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)

def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()

def read_until(pred, timeout=30.0):
    deadline = time.time() + timeout
    buf = ""
    while time.time() < deadline:
        if proc.poll() is not None:
            err = proc.stderr.read() if proc.stderr else ""
            raise RuntimeError(f"app-server exited {proc.returncode}: {err.strip()}")
        r, _, _ = select.select([proc.stdout], [], [], 0.5)
        if not r:
            continue
        line = proc.stdout.readline()
        if not line:
            continue
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if pred(msg):
            return msg
    raise TimeoutError("timed out waiting for app-server response")

try:
    send({
        "method": "initialize",
        "id": 0,
        "params": {
            "clientInfo": {
                "name": "codex_limits_script",
                "title": "Codex limits script",
                "version": "0.1.0",
            }
        },
    })
    read_until(lambda m: m.get("id") == 0)
    send({"method": "initialized", "params": {}})
    send({"method": "account/rateLimits/read", "id": 1, "params": {}})
    resp = read_until(lambda m: m.get("id") == 1)
finally:
    try:
        proc.stdin.close()
    except Exception:
        pass
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()

if "error" in resp:
    print(json.dumps(resp["error"], indent=2), file=sys.stderr)
    sys.exit(2)

result = resp.get("result") or {}
if want_json:
    print(json.dumps(result, indent=2, sort_keys=True))
    sys.exit(0)

by_id = result.get("rateLimitsByLimitId") or {}
single = result.get("rateLimits")
rows = list(by_id.values()) if by_id else ([single] if single else [])

def fmt_ts(ts):
    if ts is None:
        return "?"
    try:
        return time.strftime("%Y-%m-%d %H:%M:%S %Z", time.localtime(int(ts)))
    except Exception:
        return str(ts)

def pct(window):
    if not window:
        return None
    return window.get("usedPercent")

if not rows:
    print("no rate limit data (API-key-only auth may not expose ChatGPT limits)")
    credits = result.get("rateLimitResetCredits")
    if credits is not None:
        print(f"reset credits: {credits}")
    sys.exit(0)

print("Codex rate limits (ChatGPT plan)")
for row in rows:
    lid = row.get("limitId") or row.get("limitName") or "default"
    name = row.get("limitName") or ""
    reached = row.get("rateLimitReachedType")
    primary = row.get("primary") or {}
    secondary = row.get("secondary") or {}
    label = f"{lid}" + (f" ({name})" if name and name != lid else "")
    print(f"\n[{label}]")
    if primary:
        print(
            f"  primary:   used {pct(primary)}%  "
            f"window {primary.get('windowDurationMins')}m  "
            f"resets {fmt_ts(primary.get('resetsAt'))}"
        )
    if secondary:
        print(
            f"  secondary: used {pct(secondary)}%  "
            f"window {secondary.get('windowDurationMins')}m  "
            f"resets {fmt_ts(secondary.get('resetsAt'))}"
        )
    if reached:
        print(f"  reached:   {reached}")

credits = result.get("rateLimitResetCredits")
if isinstance(credits, dict):
    print(f"\nreset credits available: {credits.get('availableCount')}")
PY

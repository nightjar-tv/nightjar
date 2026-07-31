#!/usr/bin/env bash
# Supervisor: keep dp_byte_serve up, run measurement phases separately.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
export NIGHTJAR_BASE="${NIGHTJAR_BASE:-http://127.0.0.1:18097}"
export NIGHTJAR_SESSION_BASE="${NIGHTJAR_SESSION_BASE:-http://127.0.0.1:8096}"
export PYTHONUNBUFFERED=1
OUT="$ROOT/notes/client-arch/bakeoff-runs"
mkdir -p "$OUT"
LOG=/tmp/bakeoff-phased.log
: > "$LOG"
: > /tmp/bakeoff-request-pattern.jsonl

ensure_dp() {
  if ! curl -sf -o /dev/null -I "$NIGHTJAR_BASE/items/20175/stream"; then
    pkill -f dp_byte_serve.py 2>/dev/null || true
    sleep 0.2
    nohup python3 "$ROOT/apps/engine-bakeoff/tool/dp_byte_serve.py" >/tmp/bakeoff-dp.log 2>&1 &
    sleep 0.5
  fi
}

ensure_dp
echo "=== phase Part A mpv ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path("/Users/gmacarthur/Documents/GitHub/nightjar/apps/engine-bakeoff/tool")))
import cli_engine_measure as m
sample = json.loads(m.SAMPLE.read_text())
report = {
  "baseUrl": m.BASE,
  "part_a_mpv": m.run_latency("mpv", sample),
  "request_patterns_after_mpv": m.summarize_request_patterns(),
}
Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs/part_a_mpv.json").write_text(json.dumps(report, indent=2))
print("wrote part_a_mpv.json", report["part_a_mpv"]["warm_far_seek"])
PY

ensure_dp
echo "=== phase Part A vlc (capped) ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path("/Users/gmacarthur/Documents/GitHub/nightjar/apps/engine-bakeoff/tool")))
import cli_engine_measure as m
sample = json.loads(m.SAMPLE.read_text())
# Use only first 10 latency ids for VLC HTTP (slow attach)
sample = dict(sample)
sample["latency_item_ids"] = sample["latency_item_ids"][:10]
report = {
  "baseUrl": m.BASE,
  "part_a_vlc": m.run_latency("vlc", sample),
  "request_patterns_after_vlc": m.summarize_request_patterns(),
}
Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs/part_a_vlc.json").write_text(json.dumps(report, indent=2))
print("wrote part_a_vlc.json", report["part_a_vlc"]["warm_far_seek"])
PY

echo "=== phase T4 mpv file:// ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path("/Users/gmacarthur/Documents/GitHub/nightjar/apps/engine-bakeoff/tool")))
import cli_engine_measure as m
sample = json.loads(m.SAMPLE.read_text())
r = m.run_t4("mpv", sample)
Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs/t4_mpv.json").write_text(json.dumps(r, indent=2))
print("t4_mpv", r["failure_rate"], "disq", r["disqualified"], "ok", r["ok"], "fail", r["fail"])
PY

echo "=== phase T4 vlc file:// ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path("/Users/gmacarthur/Documents/GitHub/nightjar/apps/engine-bakeoff/tool")))
import cli_engine_measure as m
sample = json.loads(m.SAMPLE.read_text())
r = m.run_t4("vlc", sample)
Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs/t4_vlc.json").write_text(json.dumps(r, indent=2))
print("t4_vlc", r["failure_rate"], "disq", r["disqualified"], "ok", r["ok"], "fail", r["fail"])
PY

ensure_dp
echo "=== phase Part B ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path("/Users/gmacarthur/Documents/GitHub/nightjar/apps/engine-bakeoff/tool")))
import cli_engine_measure as m
sample = json.loads(m.SAMPLE.read_text())
r = {"part_b_mpv": m.run_part_b("mpv", sample), "abr_signals": m.abr_signals()}
Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs/part_b.json").write_text(json.dumps(r, indent=2))
print("part_b done", len(r["part_b_mpv"]["runs"]))
PY

echo "=== phase Part B starve 60s ===" | tee -a "$LOG"
python3 -u "$ROOT/apps/engine-bakeoff/tool/partb_starve.py" 2>&1 | tee -a "$LOG" || true

echo "=== merge report ===" | tee -a "$LOG"
python3 -u - <<'PY' 2>&1 | tee -a "$LOG"
import json
from pathlib import Path
out = Path("/Users/gmacarthur/Documents/GitHub/nightjar/notes/client-arch/bakeoff-runs")
report = {
  "url_resolution_note": "Part A: dp_byte_serve from DB (Nightjar stream is BROWSER_V0-gated). T4: file:// decode.",
  "abr_signals": json.loads((out/"part_b.json").read_text())["abr_signals"] if (out/"part_b.json").exists() else {},
}
for name in ["part_a_mpv","part_a_vlc","t4_mpv","t4_vlc","part_b"]:
  p = out / f"{name}.json"
  if p.exists():
    report[name] = json.loads(p.read_text())
starve = out / "partb-starve.json"
if starve.exists():
  report["part_b_starve"] = json.loads(starve.read_text())
(out/"bakeoff-report.json").write_text(json.dumps(report, indent=2))
print("wrote", out/"bakeoff-report.json")
PY

echo DONE | tee -a "$LOG"

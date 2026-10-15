#!/bin/sh
# Builds a data dir for snapshots of screen 2 (the agent view with the editor
# review and the browser): the agents and the space from assets/, Ember's
# directory as a git repository whose last run changed run.py since your last
# review, a report page in its artifacts, and an events file that replays the
# screen 1 state plus the run.
#
#   assets/scenarios/screen2-fixture.sh /tmp/screen2
#   target/debug/meerora snapshot screen2.png --data-dir /tmp/screen2 \
#       --events /tmp/screen2/screen2.jsonl --view agent:ember --tab editor
#
# The target directory must not exist yet.
set -eu

out=${1:?usage: screen2-fixture.sh DIR}
[ -e "$out" ] && { echo "$out exists already" >&2; exit 1; }
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
cp -r "$here/agents" "$here/spaces" "$out/"
out=$(cd "$out" && pwd)
ember="$out/agents/ember"

g() { git -C "$ember" -c user.name=meerora -c user.email=agents@meerora.local -c commit.gpgsign=false "$@"; }

g init -q -b main
printf 'artifacts/\nlogs/\n' > "$ember/.gitignore"
mkdir -p "$ember/memory" "$ember/artifacts" "$ember/pipeline"
cat > "$ember/memory/notes.md" <<'EOF'
# Pipeline bench

- Batches 1 and 2 ran with one timeout each.
- `make bench BATCH=n` reruns a batch.
EOF
cat > "$ember/config.toml" <<'EOF'
[bench]
batches = [1, 2, 3, 4, 5]
retries = 3
timeout = "30s"
data_dir = "samples/"
EOF
cat > "$ember/run.py" <<'EOF'
"""Runs the pipeline benchmark batch by batch."""

import logging
import time

from .io import load_samples
from .model import BatchResult, Config, Measurement
from . import pipeline

log = logging.getLogger("bench")


def run_batch(batch: int, cfg: Config) -> BatchResult:
    samples = load_samples(cfg.data_dir, batch)
    results = []
    for s in samples:
        t0 = time.perf_counter()
        out = pipeline.process(s)
        dt = time.perf_counter() - t0
        results.append(Measurement(s.id, dt, out.status))
    return BatchResult(batch, results)


def run_all(cfg: Config) -> list[BatchResult]:
    return [run_batch(b, cfg) for b in cfg.batches]
EOF
printf 'def load_samples(data_dir, batch):\n    return []\n' > "$ember/pipeline/io.py"
g add -A
g commit -q -m "run 2026-10-14 19:12: Set up the pipeline bench"
# You reviewed everything up to here.
g update-ref refs/meerora/reviewed HEAD

# Ember's next run: count timeouts and pass the retries through.
cat > "$ember/run.py" <<'EOF'
"""Runs the pipeline benchmark batch by batch."""

import logging
import time

from .io import load_samples
from .model import BatchResult, Config, Measurement
from . import pipeline

log = logging.getLogger("bench")


def count_timeouts(results):
    return sum(r.status == "timeout" for r in results)


def run_batch(batch: int, cfg: Config) -> BatchResult:
    samples = load_samples(cfg.data_dir, batch)
    results = []
    for s in samples:
        t0 = time.perf_counter()
        out = pipeline.process(s, retries=cfg.retries)
        dt = time.perf_counter() - t0
        results.append(Measurement(s.id, dt, out.status))
    n = count_timeouts(results)
    if n:
        log.warning("batch %d: %d timeouts", batch, n)
    return BatchResult(batch, results)


def run_all(cfg: Config) -> list[BatchResult]:
    return [run_batch(b, cfg) for b in cfg.batches]
EOF
g add -A
g commit -q -m "run 2026-10-14 19:41: Fix the timeouts in batch 3, rerun it and show me the report"

cat > "$ember/artifacts/report.html" <<'EOF'
<!doctype html>
<html><head><meta charset="utf-8"><title>Batch 3 of 5 running</title>
<style>
body{margin:0;padding:18px 20px;background:#F6F5F1;color:#1D2230;font:14px system-ui,sans-serif}
.k{font:10px monospace;letter-spacing:.08em;color:#6B7385}
h1{font-size:20px;margin:4px 0 14px}
.cards{display:flex;gap:10px;margin-bottom:16px}
.card{flex:1;background:#fff;border:1px solid #E2E0D8;border-radius:10px;padding:10px 12px}
.card div{font-size:11px;color:#6B7385}.card b{font-size:20px}
.bars{display:flex;gap:12px;align-items:flex-end;height:120px;border-bottom:1px solid #D8D6CE;padding:0 4px}
.bar{flex:1;background:#8A93A6;border-radius:3px 3px 0 0}
.run{background:repeating-linear-gradient(135deg,#F2B45A 0 6px,#F6C77F 6px 12px)}
.todo{background:none;border:1px dashed #B9BBC4;height:110px}
.l{display:flex;gap:12px;padding:6px 4px 0;font:10px monospace;color:#6B7385}.l span{flex:1;text-align:center}
</style></head>
<body>
<div class="k">PIPELINE-BENCH · REPORT</div>
<h1>Batch 3 of 5 running</h1>
<div class="cards">
<div class="card"><div>Median per sample</div><b>1.84 s</b></div>
<div class="card"><div>Timeouts so far</div><b>0</b></div>
<div class="card"><div>Batch 3</div><b>62%</b></div>
</div>
<div style="font-weight:600;margin-bottom:10px">Median time per sample, by batch</div>
<div class="bars">
<div class="bar" style="height:100px"></div><div class="bar" style="height:104px"></div>
<div class="bar run" style="height:98px"></div><div class="bar todo"></div><div class="bar todo"></div>
</div>
<div class="l"><span>B1</span><span>B2</span><span>B3</span><span>B4</span><span>B5</span></div>
</body></html>
EOF

{
  grep -v '"tool":"browser' "$here/scenarios/screen1.jsonl"
  echo '{"type":"run_started","agent":"ember","run":"r2","trigger":"user","task":"Fix the timeouts in batch 3, rerun it and show me the report when it is running."}'
  echo '{"type":"message","agent":"ember","id":"m1","role":"user","text":"Fix the timeouts in batch 3, rerun it and show me the report when it is running.","done":true,"at":0}'
  echo '{"type":"message","agent":"ember","id":"m2","role":"agent","text":"Found it: retries were not passed to process(). I patched run.py and I am rerunning batch 3. The report is open in the browser.","done":true,"at":0}'
  echo "{\"type\":\"action\",\"agent\":\"ember\",\"tool\":\"browser.open\",\"summary\":\"file://$ember/artifacts/report.html\"}"
} > "$out/screen2.jsonl"
echo "wrote $out"

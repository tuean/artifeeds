#!/usr/bin/env bash
# Measure a process's CPU use over a window, as a percentage of ONE core.
#
#   ./scripts/measure_cpu.sh <pid> [seconds] [samples]
#
# `ps -o %cpu` is a decaying average that takes a minute to settle, so this
# samples cumulative CPU time instead and divides by wall time — the same number
# Activity Monitor shows, but available immediately and printable in a loop.
set -euo pipefail
pid=${1:?usage: measure_cpu.sh <pid> [seconds] [samples]}
window=${2:-8}
samples=${3:-3}

cpu_time() {
  # `ps -o time=` prints [[dd-]hh:]mm:ss.ss — seconds, however it is formatted.
  python3 - "$pid" <<'PY'
import subprocess, sys
pid = sys.argv[1]
out = subprocess.run(["ps", "-o", "time=", "-p", pid], capture_output=True, text=True).stdout.strip()
if not out:
    print("dead"); raise SystemExit(1)
days = 0
if "-" in out:
    d, out = out.split("-", 1); days = int(d)
parts = [float(p) for p in out.split(":")]
while len(parts) < 3:
    parts.insert(0, 0.0)
h, m, s = parts
print(days * 86400 + h * 3600 + m * 60 + s)
PY
}

echo "pid $pid  window ${window}s  samples $samples"
for i in $(seq 1 "$samples"); do
  before=$(cpu_time)
  sleep "$window"
  after=$(cpu_time)
  python3 -c "
b, a, w = $before, $after, $window
print(f'  sample $i: {(a-b)/w*100:6.1f}% of one core   ({(a-b):.2f}s cpu in {w}s)')
"
done

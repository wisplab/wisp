#!/usr/bin/env bash
# bench/results-suite/run.sh
#
# Run the documented benchmark suite against a live wisp-runtime
# daemon. Reports per-case p50 / p95 / p99 / mean for each of the
# six workload shapes in bench/results.md.
#
# Assumes the daemon is already running at $WISP_DAEMON_URL (default
# http://127.0.0.1:9000). For file_read measurements, the daemon
# must have been started with a $WISP_CAPABILITIES_JSON allowing
# file_read on /tmp/wisp-bench.
set -euo pipefail
cd "$(dirname "$0")"

DAEMON="${WISP_DAEMON_URL:-http://127.0.0.1:9000}"
N="${N:-50}"  # samples per case

# Quick reachability check.
if ! curl -sf "$DAEMON/healthz" >/dev/null 2>&1; then
  echo "FATAL: daemon not reachable at $DAEMON" >&2
  exit 1
fi

# Set up a workspace file for the file_read case (if web_fetch / file_read
# is configured the daemon will allow it; if not, the file_read case will
# return WispError -2 and the recorded latency reflects the bridge-deny
# path, not the actual read — note in results.md).
mkdir -p /tmp/wisp-bench
printf "hello from a bench file\n%s\n" "$(head -c 1024 /dev/urandom | base64 | head -c 512)" > /tmp/wisp-bench/data.txt

run_case() {
  local name="$1"
  local code="$2"
  local samples=()
  for i in $(seq 1 $N); do
    local resp
    resp=$(curl -s -X POST "$DAEMON/v1/eval" \
        -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'code': sys.argv[1]}))" "$code")")
    local us
    us=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['elapsed_us'])" "$resp")
    samples+=("$us")
  done
  python3 -c "
import sys, statistics
samples = sorted(int(x) for x in sys.argv[1:])
n = len(samples)
def pct(p):
    return samples[min(n-1, int(p*n/100))]
print(f'  {\"$name\":<25} '
      f'p50={pct(50)/1000:6.2f}  '
      f'p95={pct(95)/1000:6.2f}  '
      f'p99={pct(99)/1000:6.2f}  '
      f'mean={statistics.mean(samples)/1000:6.2f}  ms   '
      f'(n={n})')
" "${samples[@]}"
}

run_session_case() {
  local name="$1"
  # Create session, run N evals in it, delete.
  local sid
  sid=$(curl -s -X POST "$DAEMON/v1/session" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['session_id'])")
  local samples=()
  for i in $(seq 1 $N); do
    local resp
    resp=$(curl -s -X POST "$DAEMON/v1/session/$sid/eval" \
        -H 'Content-Type: application/json' \
        -d '{"code":"x = (x + 1) if \"x\" in dir() else 0\nprint(x)"}')
    local us
    us=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['elapsed_us'])" "$resp")
    samples+=("$us")
  done
  curl -s -X DELETE "$DAEMON/v1/session/$sid" >/dev/null
  python3 -c "
import sys, statistics
samples = sorted(int(x) for x in sys.argv[1:])
n = len(samples)
def pct(p):
    return samples[min(n-1, int(p*n/100))]
print(f'  {\"$name\":<25} '
      f'p50={pct(50)/1000:6.2f}  '
      f'p95={pct(95)/1000:6.2f}  '
      f'p99={pct(99)/1000:6.2f}  '
      f'mean={statistics.mean(samples)/1000:6.2f}  ms   '
      f'(n={n})')
" "${samples[@]}"
}

echo "=== wisp daemon bench suite (N=$N samples per case) ==="
echo "  daemon: $DAEMON"
echo

# Warm the daemon with a few calls so we measure steady-state.
for i in 1 2 3 4 5; do
  curl -s -X POST "$DAEMON/v1/eval" -H 'Content-Type: application/json' \
    -d '{"code":"print(2+2)"}' >/dev/null
done

echo "=== stateless /v1/eval ==="
run_case "print('hello')"        'print("hello")'
run_case "2+2"                   'print(2+2)'
run_case "json round-trip"       'import json; print(len(json.dumps(json.loads("[1,2,3,4,5]"))))'
run_case "regex findall"         'import re; print(len(re.findall(r"\\d+", "abc 123 def 456 ghi 789")))'
run_case "hashlib sha256"        'import hashlib; print(hashlib.sha256(b"hello world").hexdigest()[:16])'
run_case "numpy array.sum()"     'import numpy as np; print(np.arange(100).sum())'
run_case "numpy 100x100 matmul"  'import numpy as np; A=np.arange(10000).reshape(100,100).astype(float); print((A @ A).shape)'
run_case "numpy fft 64"          'import numpy as np; print(np.abs(np.fft.fft(np.arange(64.0)))[0])'
run_case "numpy linalg solve"    'import numpy as np; A=np.eye(5)*2.0; b=np.arange(5.0); print(np.linalg.solve(A, b))'
run_case "numpy random 1k"       'import numpy as np; print(np.random.default_rng(0).standard_normal(1000).mean())'
run_case "wisp.file_read"        'import wisp; print(len(wisp.file_read("/tmp/wisp-bench/data.txt")))'

echo
echo "=== session /v1/session/:id/eval (state-carry, N=$N evals in one session) ==="
run_session_case "in-session counter"

echo
echo "  daemon log tail:"
tail -3 /tmp/wisp-daemon.log 2>/dev/null || true

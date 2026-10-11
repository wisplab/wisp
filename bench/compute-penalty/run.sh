#!/usr/bin/env bash
# bench/compute-penalty/run.sh
#
# Runs kernels.py (a) natively on host python3 and (b) inside the wisp
# guest via a running daemon's /v1/eval, then prints the per-kernel
# penalty factor k = wasm_seconds / native_seconds.
#
# Usage:
#   LABEL=baseline-O2 ./run.sh        # daemon must be running on :9000
#
# Results are saved to results/$LABEL-{native,wasm}.json so different
# builds (baseline vs -msimd128 vs wisp-accel) can be compared later.
set -euo pipefail
cd "$(dirname "$0")"

DAEMON="${WISP_DAEMON_URL:-http://127.0.0.1:9000}"
LABEL="${LABEL:-unlabeled}"
mkdir -p results

if ! curl -sf "$DAEMON/healthz" >/dev/null 2>&1; then
  echo "FATAL: daemon not reachable at $DAEMON" >&2
  exit 1
fi

echo "==> native run (host python3)"
NATIVE_OUT=$(python3 kernels.py)
NATIVE_JSON=$(echo "$NATIVE_OUT" | grep '^WISP_BENCH_RESULT ' | sed 's/^WISP_BENCH_RESULT //')
echo "$NATIVE_JSON" > "results/$LABEL-native.json"

echo "==> wasm run (daemon /v1/eval, single call, in-guest timing)"
WASM_RESP=$(python3 - "$DAEMON" <<'EOF'
import json, sys, urllib.request
code = open("kernels.py").read()
req = urllib.request.Request(
    sys.argv[1] + "/v1/eval",
    data=json.dumps({"code": code, "timeout_ms": 600000}).encode(),
    headers={"Content-Type": "application/json"},
)
resp = json.load(urllib.request.urlopen(req, timeout=600))
print(json.dumps(resp))
EOF
)
WASM_JSON=$(python3 -c "
import json, sys
resp = json.loads(sys.argv[1])
if resp.get('rc') != 0:
    sys.stderr.write('guest rc=%s\nstderr:\n%s\n' % (resp.get('rc'), resp.get('stderr','')))
    sys.exit(1)
for line in resp['stdout'].splitlines():
    if line.startswith('WISP_BENCH_RESULT '):
        print(line[len('WISP_BENCH_RESULT '):]); break
else:
    sys.stderr.write('no WISP_BENCH_RESULT in stdout:\n%s\n' % resp['stdout'])
    sys.exit(1)
" "$WASM_RESP")
echo "$WASM_JSON" > "results/$LABEL-wasm.json"

echo
python3 - "$LABEL" <<'EOF'
import json, sys
label = sys.argv[1]
native = json.load(open(f"results/{label}-native.json"))
wasm = json.load(open(f"results/{label}-wasm.json"))
print(f"=== compute penalty k (wasm/native) — label: {label} ===")
print(f"  native: python {native['meta']['python']} numpy {native['meta']['numpy']} ({native['meta']['platform']})")
print(f"  wasm:   python {wasm['meta']['python']} numpy {wasm['meta']['numpy']} ({wasm['meta']['platform']})")
print()
print(f"  {'kernel':<22} {'native':>10} {'wasm':>10} {'k':>8}")
for name, nv in native["seconds"].items():
    wv = wasm["seconds"].get(name)
    if wv is None:
        continue
    print(f"  {name:<22} {nv*1000:>8.2f}ms {wv*1000:>8.2f}ms {wv/nv:>7.1f}x")
EOF

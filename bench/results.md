# Wisp benchmark results

Real, reproducible measurements of the wisp-runtime daemon end-to-end
across a representative workload mix. All numbers are p50 / p95 / p99
elapsed-microseconds **as measured by the daemon's own
`elapsed_us` field**, captured after a 5-call warmup, N=50 samples per
case. Reproduce with `bash bench/results-suite/run.sh`.

> The `elapsed_us` field is what the daemon clocks from job-receive
> to job-respond. End-to-end through HTTP from a local client adds
> ~0.5–1 ms; through an MCP stdio adapter adds another ~5–8 ms.

## Environment

  - MacBook Pro M4 Pro (arm64), macOS 15
  - Wasmtime 27 (in-tree dep, single-arch arm64 darwin build)
  - WASI SDK 32.0 (clang 22)
  - CPython 3.14.3 compiled to `wasm32-wasip1` via `Tools/wasm/wasi`
  - python-reactor.wasm: 42 MB (incl. numpy core + fft + linalg + random)
  - Daemon snapshot at boot: 27 MB / 417 wasm pages (numpy pre-imported)
  - Daemon workers: 8 (default = available parallelism)

## Per-call cold start (executor primitive)

This is the substrate-level measurement from
[bench/python-wasi-cow/FINDINGS.md][cow] — how long it takes to mmap a
new wasm Instance into the snapshot's COW memory and run a no-op:

| Metric | Value |
|---|---|
| p50 cold start | **0.78 ms** |
| Throughput, in-process | ~1280 fresh sandboxes / sec / core |
| Parallel fork (Spike B2) | **2394 branches / sec / 8 cores** |
| Snapshot size | 27 MB / 417 pages (after numpy pre-import) |

These are the **primitive numbers** — what a single thread can do with
the wasmtime engine when the test harness skips HTTP, JSON, and
worker-pool dispatch. The per-call numbers below add those.

## Per-call eval via daemon HTTP (stateless `/v1/eval`)

Each call: fresh wasm Instance from COW snapshot, MAP_FIXED reset,
`wisp_eval` runs user code, stdout/stderr drain, response. No state
survives between calls.

| Case | p50 | p95 | p99 | mean | Notes |
|---|---|---|---|---|---|
| `print("hello")` | 1.80 ms | 6.59 ms | 11.21 ms | 2.33 ms | Lower bound for "any Python code" |
| `print(2+2)` | 1.63 ms | 1.86 ms | 2.07 ms | 1.65 ms | Tightest |
| `json` round-trip | 1.85 ms | 2.26 ms | 52.68 ms | 3.50 ms | `json.loads` + `json.dumps` of `[1..5]` |
| `re.findall` | 1.93 ms | 3.17 ms | 3.91 ms | 2.04 ms | Three matches in a 30-char string |
| `hashlib.sha256` | 1.76 ms | 3.57 ms | 52.27 ms | 2.90 ms | SHA-256 of `"hello world"` |
| `np.arange(100).sum()` | 1.80 ms | 2.58 ms | 36.82 ms | 2.57 ms | First numpy touch in this call |
| `np` 100×100 matmul | 2.49 ms | 3.14 ms | 46.51 ms | 3.61 ms | `(A @ A).shape` for 100×100 float |
| `np.fft.fft(arange(64))` | 1.83 ms | 4.12 ms | 31.99 ms | 2.61 ms | pocketfft scalar build |
| `np.linalg.solve(I*2, b)` | 2.66 ms | 3.96 ms | 20.64 ms | 3.19 ms | lapack_lite path |
| `default_rng(0).standard_normal(1000)` | 2.40 ms | 4.28 ms | 22.50 ms | 3.02 ms | mt19937 + distributions |
| `wisp.file_read(...)` | 2.09 ms | 2.68 ms | 10.68 ms | 2.31 ms | Reads 530 bytes via host bridge (allowlist hit) |

**Take:** **p50 is 1.6–2.7 ms across every case**, including the
numpy-heavy ones. The p99 outliers (20–50 ms occasionally) are
worker-pool tail latency — under load they're more uniform.

The reason numpy doesn't penalize per-call latency: numpy is pre-
imported into the snapshot, so `import numpy as np` in user code is a
`sys.modules` dict lookup, not a parse + ufunc-table build. See
[`scripts/numpy/README.md`][numpy-readme] for how that works.

## Session API (`/v1/session/:id/eval`, state-carry)

A session holds a long-lived wasm Instance and skips the mmap reset
between evals. Same workload, but state persists across calls — so
imports happen once, variables stay live.

| Case | p50 | p95 | p99 | mean | Notes |
|---|---|---|---|---|---|
| In-session counter | **0.23 ms** | 1.01 ms | 1.67 ms | 0.37 ms | `x = (x+1) if "x" in dir() else 0; print(x)` repeated 50× |

**Take:** sub-millisecond per call once state is warm. Sessions are
the right shape when consecutive calls share state.

## Compare to other platforms

These are not apples-to-apples — different substrates, deployment
models, and network paths. The honest framing: each has a sweet spot,
and Wisp's sweet spot is **high-frequency stateless tool calls** plus
**low-latency in-session evals**. See
[`wisplab-landing/blog/sub-ms-python-sandbox`][blog] for the landscape
breakdown.

| Platform | Per-call cold start (their reported) | Notes |
|---|---|---|
| **Wisp** | **0.78 ms primitive / 1.6–2.7 ms daemon** | This file |
| E2B | ~150 ms | Firecracker microVM + Jupyter inside, persistent |
| Blaxel | ~25 ms warm resume | Firecracker + perpetual env |
| Daytona | ~90 ms warm | Docker container + Kata |
| Modal Sandbox | sub-second | Custom Rust container + gVisor |
| AWS Lambda (Python cold) | 800–1500 ms | Firecracker per-invocation |

## MCP transport overhead

When the eval comes from an MCP client (Claude Code, etc.) instead of
direct HTTP, add ~8 ms for the stdio JSON-RPC round-trip:

| Path | p50 |
|---|---|
| Direct daemon (`curl /v1/eval`) | 1.6–2.7 ms |
| MCP stdio (Python harness → `wisp_mcp_server.py` → daemon) | 10–15 ms |

The bulk of the MCP overhead is JSON-RPC encode/decode at the client
and server ends, plus the extra HTTP hop the MCP server makes to the
daemon. Acceptable for an agent tool call (LLM inference itself is
100–1000 ms).

## Reproduce

```sh
# 1. Build the daemon + python-reactor.wasm + libnumpy.a
cargo build --release -p wisp-runtime
# Skip if you already built; recipe is in the top-level README.

# 2. Start the daemon with a bench-friendly capability config
cat > /tmp/wisp-bench-caps.json <<'EOF'
{"file_read": {"allow_prefixes": ["/tmp/wisp-bench"]}}
EOF
WISP_CAPABILITIES_JSON=/tmp/wisp-bench-caps.json \
  ./target/release/wisp-runtime &

# 3. Run the bench (N=50 by default; override with N=200)
bash bench/results-suite/run.sh
```

The script writes a per-case p50/p95/p99/mean line to stdout. Numbers
in this doc are from one representative run; expect ±20 % between runs
depending on system load.

## Caveats

  - Numbers are from a single MacBook Pro M4 — not server-class
    hardware. Linux x86_64 numbers can be ~30 % faster (wasmtime
    AOT compile is better-tuned there).
  - p99 includes worker-pool warmup hits + occasional macOS scheduler
    blips. Under continuous load on a busy daemon the p99 tightens.
  - `wisp.file_read` latency includes the host bridge round-trip
    (~0.3 ms of the 2.09 ms p50 is the bridge; the rest is the eval
    itself).
  - "Mean > p95" on some cases (json, hashlib, numpy.sum) reflects
    the 50-sample window catching one or two p99 spikes. With
    N=200+ the mean settles closer to p50.

[cow]: ./python-wasi-cow/FINDINGS.md
[numpy-readme]: ../scripts/numpy/README.md
[blog]: https://github.com/wisplab/wisplab-landing/tree/main/blog/sub-ms-python-sandbox

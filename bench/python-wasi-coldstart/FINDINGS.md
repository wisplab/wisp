# Spike A — WASI Python cold-start with module pooling: findings

> **Run date**: 2026-05-02 afternoon. **wasmtime crate**: 27.0 in-process.
> **python.wasm**: 29 MB (CPython 3.14.3 from M0). **Hardware**: Apple Silicon
> M-series. **N**: 200 iterations after 20 warmup, repeated for 4 configs.

## TL;DR

In-process Wasmtime + cached Engine + cached Module → **WASI Python
cold-start drops from ~400 ms (subprocess CLI baseline from M0) to 39 ms p50
for `pass`, 67 ms p50 for `import json, re, math; json.dumps(...)`**.

This is *with* a fresh Python interpreter every call (no snapshot/restore
yet). The remaining ~38 ms is dominated by Python interpreter initialization
inside `_start`. Wasmtime itself contributes <1 ms of the total.

| Cold-start scenario | p50 |
|---|---|
| AWS Lambda Python with deps (published) | 800 ms+ |
| Modal cold (published) | 1500 ms |
| Modal warm pool (published) | 50 ms |
| **Wisp WASI Python, in-process, fresh interpreter** | **39 ms** |
| Wisp 5 ms target (requires snapshot/restore — Spike A2) | TBD |

We beat Modal's warm-pool number with a *cold* interpreter every call.
The 5 ms target needs the per-call sandbox primitive (memory snapshot/restore)
to skip Python init.

## Per-call decomposition

```
Engine created once:    1–3 ms (one-time)
Module compiled once:   1.7–1.9 s (one-time, JITs 29 MB WASM)

Per-call:
  Store::new                  0.06 ms p50   (per-tenant WASI ctx)
  Linker setup                0.07 ms p50
  Instance::instantiate       0.52 ms p50   (instance from cached module)
  Run _start                 38.78 ms p50   ← Python interpreter init
  ─────────────────────────────────────
  Total                      39.39 ms p50
```

`_start` time **is** Python init. CPython runs:
- Static type registration (~150 builtin types)
- `sys.modules` setup
- Import bootstrap chain (`importlib._bootstrap*`)
- Site / encodings init
- Then runs the user `-c` code and exits

`json + re + math` import adds ~28 ms (`Run _start` goes from 38 → 66 ms),
which matches the cost of importing three more frozen modules + a `json.dumps`
of a single dict.

## Configuration matrix

| Config | Script | p50 | p99 | mean |
|---|---|---|---|---|
| on-demand allocator | `pass` | 39.39 ms | 61.97 ms | 40.55 ms |
| on-demand allocator | imports + dumps | 67.01 ms | 97.62 ms | 69.18 ms |
| pooling allocator + CoW | `pass` | 37.26 ms | 60.92 ms | 39.22 ms |
| pooling allocator + CoW | imports + dumps | 62.68 ms | 86.18 ms | 64.13 ms |

**Pooling + memory_init_cow buys almost nothing** (37 vs 39 ms p50).
Reason: the .data section is small relative to the overall Python heap, and
the bottleneck is *runtime CPU work* the interpreter does on the .bss
(class init, module init, frozen import expansion), not page IO. CoW only
helps the .data side. To skip the .bss init we need linear-memory
snapshot/restore, not just CoW page sharing.

This is consistent with what we'd expect from the architecture: pooling
allocator's value shows up when the workload is many short, mostly-empty
WASM instances (e.g., a sandbox per HTTP request that does little). For
"fresh CPython every call" the dominant cost is the interpreter's own
init code.

## Two open optimization targets

### 1. Persistent module cache (one-time cost)

`Module::from_file` JIT-compiles the 29 MB python.wasm in 1.7–1.9 s.
Production should use `Engine::precompile_module` to write the JIT output
to disk; subsequent loads are then `Module::deserialize_file` at <50 ms.

This doesn't affect per-call cold start (the module is already cached after
first compile in this benchmark), but matters for real deployment where
processes restart.

### 2. Linear-memory snapshot/restore (per-call savings)

This is the load-bearing primitive for the <5 ms target. Plan:

1. Build a `wisppy.wasm` wrapper that on `_start` does Python init, then
   reads user code from a known location and executes it
2. Run `wisppy.wasm` once to a stable post-init state
3. Capture the linear memory contents (~30 MB)
4. For each call: create new Instance, copy snapshot bytes into linear memory,
   re-enter `_start` (which now skips init because it can detect the snapshot
   marker), run user code

The challenge isn't the memcpy (~1 ms for 30 MB). It's the re-entry: WASM
doesn't expose "save program counter and resume", so we need wrapper code
that's aware of the snapshot and re-routes execution. Two approaches:

- **A**: rebuild CPython with custom `_start` that has a fast path "is
  there a snapshot marker? then jump to user-code-runner" (clean but
  invasive — patches CPython)
- **B**: use Wasmtime's pre-init hook + linear memory mmap to avoid
  re-entering `_start` at all, treating the post-init state as the new
  init state (cleaner — uses Wasmtime's `Engine::cache_config` mechanism)

Spike A2 will explore approach B first.

## Comparison to commercial baselines

Numbers from published platform documentation and the M0 spike:

| Runtime | Cold start | Notes |
|---|---|---|
| AWS Lambda Python (with numpy) | 800–1500 ms | Provisioned concurrency excluded |
| Modal cold | 1000–2000 ms | Fresh interpreter per invocation |
| Modal warm pool | 50–200 ms | Pool keeps interpreter resident |
| Cloudflare Workers Python (Pyodide) | 200–500 ms | V8 isolate + Pyodide load |
| **Wisp pooled (this spike)** | **39 ms / 67 ms** | Fresh interpreter, no snapshot |
| Wisp + snapshot (Spike A2 target) | <5 ms | Pre-warmed memory image |

Already in the lead by a factor of 1.3× over Modal warm-pool with no
snapshot work. With snapshot, the gap should be ~10×.

## Reproduction

```bash
cd wisp/bench/python-wasi-coldstart
cargo build --release
./target/release/py-coldstart
```

Default `python.wasm` path is the M0 build under `runtime/cpython-wasi/...`.
Override with `PYTHON_WASM=...` env var.

## Where this goes in the roadmap

This spike validates the **module-pooling** primitive on a real Python
workload. The next two pieces:

- **Spike A2**: linear-memory snapshot/restore — the per-call sandbox
  primitive from `private/01-architecture.md` §5
- **First blog post**: "Running Python in Wasmtime, May 2026" — combine
  M0 (build), Spike 1 (Wasmtime substrate floor), Spike 2 (Pyodide
  ecosystem), and this spike (real-Python cold start) into one
  technical-credibility post for the wisplab.org/blog/ section

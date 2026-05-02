# Spike 1 — Wasmtime cold-start floor: findings

> **Run date**: 2026-05-02 (Day 2 of Wisp).
> **Wasmtime version**: 27.0.0.
> **Hardware**: Apple Silicon M-series (darwin), aarch64.
> **Build**: `cargo build --release` with `lto=true, codegen-units=1`.

## TL;DR

The **Wasmtime empty-instance cold-start floor is 3.12 μs p50**. With the engine
pre-warmed and the module pre-compiled, fresh Store + Instance + a single
function call costs ~3 μs at the median, ~7 μs at p99.

The Phase 0 thesis target is **<5 ms p50** for the WASM fast path. The Wasmtime
substrate alone leaves **~1600× of headroom** below that target. The runtime
itself will not be the bottleneck.

## Pre-warm cost (one-time per host)

| Phase | Time |
|---|---|
| `Engine::new(&Config)` | 7.2 ms |
| `Module::new(&engine, WAT)` | 29.0 ms (noop WAT, ~1KB) |

Both are paid once per host process, not per call. For a real Pyodide-sized
module (~30 MB compiled), expect module compile to be 500 ms – 2 s — also
paid once per host.

## Per-call decomposition

N=1000 post-warmup iterations, sorted percentiles:

| Phase | p50 (μs) | p99 (μs) | mean (μs) |
|---|---|---|---|
| `Store::new(&engine, wasi_ctx)` | 2.21 | 5.88 | 2.47 |
| `linker.instantiate(&store, &module)` | 0.46 | 0.92 | 0.51 |
| `inst.get_typed_func + .call` | 0.21 | 0.46 | 0.25 |
| **Total** | **3.12** | **7.38** | **3.44** |

The dominant phase is `Store::new` (~70% of total). A Store is the per-tenant
context that holds WASI state, host imports, and the WASM heap. Reducing this
further would require pooling Stores or stripping WASI from the hot path.

## What this does NOT measure

- **Pyodide on Wasmtime**: Pyodide's WASM is emscripten-built and does not
  drop into Wasmtime directly. See `NOTES.md` for the three paths forward
  (CPython-WASI, emscripten shims, custom Python+numpy WASI distro). This
  spike establishes the floor we'll add Pyodide to.
- **A real workload**: the WASM module here is a `noop` returning 42. Real
  workloads (numpy import, pandas merge, JSON parse) add their own time on
  top — those are measured separately in `bench/pyodide-compat`.
- **Cold module load**: this measurement assumes the module is already
  parsed + validated. First-time module load for a Pyodide-sized module
  is ~500 ms – 2 s (one-time per host).

## Implications for the thesis

1. **The 5 ms p50 WASM-path target is plausible.** Wasmtime substrate floor
   is 3 μs; we have 1600× headroom. Even if Pyodide-on-Wasmtime adds 4 ms
   per call (worst-case estimate from preliminary research), we land at
   <5 ms p50 with margin.
2. **`Store::new` dominates at this scale.** If we want to push below 1 μs
   per call (e.g., for batch dispatch where 10k calls/round-trip is desired),
   pooling Stores becomes worth investigating. For now, the substrate is
   well below target; no optimization needed yet.
3. **Module compilation is one-time.** The 29 ms compile cost on a noop
   module suggests the Pyodide-sized module compile will be in the
   hundred-ms range. With pre-warmed module pool, this never enters the
   hot path.
4. **WASI overhead is small but nonzero.** Building WASI context and tying
   it to a Store costs ~2 μs of the 3 μs total. For workloads that don't
   need filesystem access (pure compute, JSON parse), a WASI-less path
   could shave ~50% off this floor.

## Phase 0 gate

Original Phase 0 kill criterion (from `private/05-open-questions.md` Q1):

> "<10 ms p50 cold start with `import numpy; np.mean(...)` → ✅ thesis confirmed.
> 30 ms+ → ❌ thesis broken."

This spike doesn't yet measure that combined number — it measures the
Wasmtime substrate alone. But the substrate result (3.12 μs) is so far below
the 10 ms target that the question is no longer "can Wasmtime keep up?" but
"how much overhead does Pyodide add?" — which is a research question, not
a runtime-substrate question.

## Next steps

1. **Choose Path A or B from `NOTES.md`** based on Spike 2 findings (Pyodide
   ecosystem coverage is 22/22 = 100% match → Pyodide is worth integrating).
2. **Spike 1.5: Wasmtime + CPython-WASI baseline** (Path A, ~1 week of
   cross-compile work). Measures how much Python interpreter startup adds
   to the Wasmtime floor.
3. **Spike 1.6: Pyodide-on-Wasmtime via emscripten shims** (Path B,
   3–6 weeks). Measures the actual thesis target — `import numpy; np.mean(...)`
   in Wasmtime.
4. **Defer the full <5ms p50 measurement** to after one of the above paths
   is working. For now, the substrate floor confirms the architecture is
   not blocked at the Wasmtime level.

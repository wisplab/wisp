# Wisp Phase 0 Benchmarks

Two spikes designed to validate (or kill) the Wisp thesis before serious
engineering investment. Run on Day 2 (2026-05-02) on Apple Silicon, Wasmtime
27.0.0, Pyodide 0.28.x, CPython 3.14.3.

## Spike 1 — Wasmtime cold-start floor (`wasm-coldstart/`)

What's the irreducible per-instance Wasmtime overhead with the engine
pre-warmed and the module pre-compiled? Lower bound on any Python-on-Wasmtime
approach.

**Result**: 3.12 μs p50, 7.38 μs p99. Wasmtime is ~1600× below the 5ms target.
Substrate is not the bottleneck.

See [`wasm-coldstart/FINDINGS.md`](wasm-coldstart/FINDINGS.md) and
[`wasm-coldstart/NOTES.md`](wasm-coldstart/NOTES.md) (Pyodide-on-Wasmtime
research gap).

## Spike 2 — Pyodide pandas/sklearn compat (`pyodide-compat/`)

How much of the Python ecosystem actually works in Pyodide WASM in 2026?
Without this, the thesis is moot.

**Result**: 22/22 snippets match between Pyodide and CPython after fixing two
harness bugs. pandas 12/12, numpy 6/6, sklearn 4/4. Per-call hot-path
execution in Pyodide is 4–148 ms for the workloads we tested.

See [`pyodide-compat/FINDINGS.md`](pyodide-compat/FINDINGS.md).

## Phase 0 verdict (preliminary)

Both spikes return green. The original kill criterion (Q1 from
`private/05-open-questions.md`) was "<10 ms p50 cold start for
`import numpy; np.mean(...)`" — we don't yet have the combined number because
Pyodide-on-Wasmtime requires bridging emscripten ABI to Wasmtime, which is
the next ~3–6 weeks of work. But:

- Wasmtime substrate: 3 μs floor. Plenty of headroom.
- Pyodide ecosystem: 100% match on a 22-snippet representative corpus, with
  warm per-call execution times in the single-digit-to-low-double-digit-ms
  range.

Combined estimate: **<10 ms p50 is highly plausible**; <5 ms p50 is achievable
if we can amortize Pyodide module instantiation through pooling (which the
architecture already plans for).

## How to reproduce

```sh
# Spike 1
cd wasm-coldstart && cargo build --release && ./target/release/coldstart

# Spike 2
cd pyodide-compat && ./setup.sh && npm test
```

Both should run on macOS or Linux with Rust 1.88+, Node.js 20+, Python 3.13+.
Setup downloads ~150 MB total (Wasmtime crates + Pyodide WASM bundle +
pandas/sklearn wheels).

# Spike 1 — Wasmtime cold-start: research notes

## What this spike measures

The **per-instance cold-start floor in Wasmtime**, with the engine pre-warmed
and the WASM module pre-compiled. It uses a trivial `noop` WAT module so the
result is the irreducible Wasmtime overhead — Store creation, instantiation,
function lookup, and a single call.

This is the lower bound any Python-on-Wasmtime approach must add to.

## What it does NOT measure (yet)

The original Phase 0 plan was "Wasmtime + Pyodide cold-start for `import numpy;
np.mean(...)`". As of 2026 this is **not a one-line setup** — there's a real
research gap to close before we can run it. The gap is documented below; this
spike establishes the empty-instance baseline so we know how much budget is
left for everything we add on top.

## The Pyodide-on-Wasmtime gap

Pyodide's WASM artifact (`pyodide.asm.wasm`) is built with **emscripten**
targeting a JS host. It expects a runtime that provides:

- `env.*` imports for syscall trampolines (read/write/mmap/etc.)
- The emscripten heap and `Module` object semantics
- A JS-side filesystem (MEMFS) — Pyodide bundles a Python stdlib zip that gets
  mounted at startup
- TextEncoder / TextDecoder / various JS APIs that Python parts call into
- Async loop integration (`asyncio` runs through JS event loop)

Wasmtime supports **WASI**, not emscripten. WASI gives you POSIX-like
filesystem and stdio, but not the emscripten ABI. Dropping `pyodide.asm.wasm`
into Wasmtime fails immediately at module instantiation — missing imports.

There are three viable paths forward, in increasing effort:

### Path A — Use a CPython WASI build, no Pyodide

CPython 3.13+ has an official WASI cross-compile target. We can:

1. Cross-compile CPython 3.14 to `wasm32-wasi`
2. Bundle the stdlib as a zip mounted at WASI preopen `/lib/python3.14`
3. Run user code via a Python entrypoint inside the WASM
4. **No numpy / pandas** out of the box — those require their C extensions
   cross-compiled to WASI, which is non-trivial and largely undone in 2026

This gets us *Python in Wasmtime* but not the Pyodide ecosystem. Useful as a
floor measurement for "pure Python tool calls" (which is a large fraction of
agent workloads — file I/O, JSON, regex), but doesn't validate the full thesis.

**Effort**: ~1 week of cross-compile plumbing.

### Path B — Build emscripten compatibility shims for Wasmtime

Implement the subset of emscripten's ABI that Pyodide actually uses, in Rust,
on top of Wasmtime. Bytecode Alliance has discussed `wasi-emscripten-host` as
a Wasmtime extension; some pieces exist as community crates.

The risk is that Pyodide's emscripten dependency surface is broad and partially
undocumented. Every Pyodide minor version may add new shim requirements.

**Effort**: ~3–6 weeks. High maintenance burden afterward.

### Path C — Compile our own Python+numpy WASI distribution

Build CPython + numpy (+ optionally pandas/scikit-learn) as a single WASI
artifact, similar in spirit to Pyodide but targeting WASI directly instead of
emscripten. This is a real engineering project — Pyodide upstream has
considered it; some of the work is in `python-wasm` adjacent forks.

**Effort**: 1–3 months of focused work, maintenance ongoing.

## Recommendation for Phase 0

1. **Run this spike** to nail the Wasmtime baseline (μs-range expected).
2. **Run Spike 2 (Pyodide-via-Node)** to validate the *ecosystem coverage*
   claim independently of the runtime question. If Pyodide can't handle
   pandas/sklearn well, the runtime question is moot.
3. **If Spike 2 looks healthy**, choose Path A or B for the runtime side:
   - Path A if "pure-Python tool calls" coverage (>60% of agent workloads)
     is enough for the v0 launch
   - Path B if we want full Pyodide ecosystem on Wasmtime from day one

The choice between A and B depends on what Spike 2 shows about pandas/sklearn
WASM coverage. Defer until that data lands.

## What this spike does answer

- Engine creation cost (one-time)
- Module compilation cost (one-time per module)
- Per-instance Store + instantiation + call cost (the cold-start floor we'll
  add Pyodide / CPython-WASI on top of)
- Whether the Wasmtime substrate itself can hit the <5 ms p50 target for the
  empty case — if even an empty instance takes 10 ms, the thesis is dead and
  we don't need to bother with Pyodide research

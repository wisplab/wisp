# Wisp WASI Python Distribution — Roadmap

> The Path D mission: build a WASI-native Python distribution that ships with
> numpy / pandas / sklearn (and eventually a curated subset of the data-science
> stack), runs in any WASI runtime (Wasmtime, Wasmer, WasmEdge), and provides
> the substrate for the Wisp agent runtime on top.
>
> Reference: Pyodide solved this for **emscripten** (5+ years, multi-org,
> $millions of effort). We are doing the equivalent for **WASI**, with the
> benefit of (a) Pyodide's existing recipes to learn from, (b) modern wasi-sdk
> with much better C/C++ support than 2018-era emscripten, and (c) WASI
> Preview 2 + Component Model arriving in parallel.

---

## Why this is worth doing as OSS

The current Python-on-WASM landscape forces a choice:

| Path | Substrate | Ecosystem | Limitation |
|---|---|---|---|
| Pyodide | emscripten | Full | Requires JS host (V8/Spidermonkey/JSC) |
| python-wasi (community) | WASI | CPython only | No numpy/pandas |
| Cloudflare Workers Python | V8 isolates | Pyodide subset | V8-locked |

There is no "Pyodide for WASI". We build it.

Once shipped, this distribution is what every other "Python on WASM" project
either uses or competes with. That includes Wisp's own runtime, but also: any
edge platform that wants Python without V8, any embedded use case (game
engines, data systems, CLI tools), any agent framework that wants a sandboxed
Python for tool execution.

---

## Milestones

### M0 — CPython core in Wasmtime ✅ in progress (May 2026)

**Goal**: a `python.wasm` binary built from CPython 3.14 source that runs
hello-world under Wasmtime.

- [x] wasi-sdk-32 toolchain installed
- [x] CPython 3.14.3 source vendored
- [ ] Host CPython build (bootstrap dep)
- [ ] Cross-compile CPython to wasm32-wasip1
- [ ] Verify `wasmtime python.wasm -c 'print("hi")'` works
- [ ] Verify `import json, re, sys` works
- [ ] Document build process

**Effort**: 1–2 days (mostly waiting for builds). Foundation work.

**Risk**: low. CPython's WASI target is tier-2 supported upstream. Should
"just work" modulo a few configure-flag debugging.

### M1 — Build system for native Python extensions (Jun 2026)

**Goal**: a tool (`wisp-build` or similar) that takes an arbitrary Python
package's source tarball and produces a wasm32-wasip1 wheel, automating the
cross-compile flow.

This is the equivalent of Pyodide's `pyodide build`. Without it, every numpy
release means manual cross-compile work. With it, the package port effort
becomes "one recipe per package" instead of "hand-rebuild every release."

- [ ] Wrap wasi-sdk + CPython cross headers in a `setuptools`-like build env
- [ ] Test: build a trivial pure-Python package
- [ ] Test: build `cython` itself (we need it for numpy/pandas)
- [ ] Test: build a tiny C-ext package (e.g. `cffi`-style)
- [ ] Document recipe format

**Effort**: 2–3 weeks.

**Risk**: medium. CPython's distutils/setuptools cross-compile story is
fragile. Pyodide hit a lot of edge cases here.

### M2 — numpy port (Jul–Aug 2026)

**Goal**: `import numpy; np.array([1,2,3]).mean()` works in Wasmtime.

numpy's port is ~6 person-weeks of focused work based on Pyodide's experience.
Major sub-tasks:

- [ ] `meson` build system on WASI (numpy moved to meson 2024+)
- [ ] BLAS / LAPACK port — choose a WASI-friendly implementation
  - [ ] Option: pure-C OpenBLAS reference build (slow but portable)
  - [ ] Option: integrate `BLIS` (cleaner WASI port story)
  - [ ] Option: ship without optimized BLAS, accept perf hit (numpy works,
        linalg is slow)
- [ ] `numpy.random` C extensions
- [ ] `numpy.fft` (Pocketfft, mostly portable)
- [ ] `numpy._core` ufunc machinery
- [ ] Test against numpy's own test suite (subset)
- [ ] Compare correctness vs CPython numpy on Spike 2's 6 numpy snippets

**Threshold for M2 done**: 6/6 numpy snippets from Spike 2 corpus match.

**Effort**: 4–6 weeks.

**Risk**: medium-high. BLAS port is the spike. If we accept a slow reference
BLAS, M2 simpler but perf gap to native widens.

### M3 — pandas port (Sep–Oct 2026)

**Goal**: pandas DataFrame ops work on top of M2 numpy.

- [ ] Cython compilation under WASI (build M1's Cython works first)
- [ ] `pandas._libs` C extensions (groupby, hashtable, lib, missing, parsers)
- [ ] `pandas/io` (read_csv, read_excel, to_dict — note: read_csv depends on
      C parser performance)
- [ ] `dateutil` / `pytz` (mostly pure Python, easy)
- [ ] Test against Spike 2's 12 pandas snippets

**Threshold for M3 done**: 12/12 pandas snippets from Spike 2 corpus match.

**Effort**: 4–6 weeks.

**Risk**: medium. Pandas's Cython C extensions are less exotic than numpy's
C internals. The biggest unknown is build-time perf: pandas takes 5+ minutes
to build natively, may take 30+ minutes under WASI cross-compile.

### M4 — Distribution + loader (Nov 2026)

**Goal**: package the CPython + numpy + pandas + stdlib into a deployable
distribution with on-demand loading (à la Pyodide's package fetcher).

- [ ] Single-blob mode: ship `python-with-numpy-pandas.wasm` (~150 MB), all
      static, used for environments where size doesn't matter
- [ ] Modular mode: ship base `python.wasm` + on-demand fetched
      `numpy.wasm` / `pandas.wasm` (requires wasi-component-model dynamic
      linking, in flux as of 2026)
- [ ] Package metadata (PEP 621) describing what's included
- [ ] CLI tool: `wisp-pkg install numpy` for adding packages to a runtime

**Effort**: 3–4 weeks. Depends on WASI dynamic linking maturity.

**Risk**: high. WASI component model is the right answer but not all runtimes
implement it as of 2026. May need to ship the all-static blob first.

### M5 — sklearn port (Dec 2026)

**Goal**: scikit-learn's most-used 20 estimators work on top of M2 numpy + M3
pandas.

- [ ] Cython compilation (already done in M3)
- [ ] sklearn C extensions (mostly Cython, some pure C)
- [ ] Subset: linear_model, cluster.KMeans, preprocessing, model_selection,
      metrics
- [ ] Defer: ensemble (XGBoost/LightGBM are separate harder ports), neural_network

**Effort**: 3–4 weeks.

**Risk**: low-medium. Mostly Cython; should mostly Just Work after M3.

### M6 — v1.0 release (Q1 2027)

**Goal**: a documented, tested, publicly released WASI Python distribution
that we'd put our name on.

- [ ] CI: rebuild from scratch on every CPython release
- [ ] Public release on GitHub + crates.io / npm / PyPI
- [ ] Documentation site
- [ ] Paper or technical blog post explaining the architecture
- [ ] Outreach: WASI working group, CPython community, Pyodide community

---

## Stack rank: hardest things

1. **BLAS port for numpy** — multi-week unknown
2. **WASI dynamic linking for modular distribution** — ecosystem-level dependency
3. **scipy if we ever do it** — Fortran code, more wholesale port, deferred to v2
4. **Building Cython itself under WASI** — chicken-and-egg, needed for numpy/pandas
5. **Performance**: WASM has known perf gaps vs native (no SIMD on all platforms,
   no AVX-512, no GPU). Even after correctness, "is it fast enough?" is open.

## Stack rank: cheaper than expected

1. **CPython core** — already officially supported, just configure flags
2. **Pure Python packages** (pytz, dateutil, requests, pydantic, …) — trivially
   work once CPython works
3. **Pyodide recipes as reference** — we don't redo the patching work from
   scratch; we port their patches from emscripten to WASI. Many will apply
   with small mods.

---

## Dependencies on outside projects

- **CPython upstream** — for any ABI changes that affect WASI target
- **wasi-sdk** — keep current with LLVM upstream, ABI-compatible w/ Wasmtime
- **WASI Preview 2 / Component Model standardization** — for dynamic linking
- **Pyodide** — patches we'll cherry-pick + ports we'll learn from
- **numpy / pandas / sklearn upstream** — keeping our patches small enough
  to upstream eventually

---

## What this changes about Wisp

The original framing: "Wisp is an agent-native serverless runtime."

The Path D framing: "Wisp is a WASI Python distribution + an agent runtime
that uses it. Both OSS, both under the wisplab umbrella."

The distribution is the load-bearing artifact for both:
- For end-users running Wisp: the distribution gives them a Python they can
  actually deploy
- For other projects: the distribution becomes the de facto WASI Python and
  any project that wants Python in WASM can pick it up

This positions Wisp closer to Pyodide than to Modal — infrastructure-defining
rather than service-providing. The personal influence ceiling is much higher;
the timeline is also much longer.

---

## Realistic schedule

Original Phase 0 was "validate thesis in 13 weeks". The Path D pivot adds
~12 months of substrate work *before* we get to runtime/scheduler/SDK work.

| Phase | Scope | Months |
|---|---|---|
| M0 — CPython | Foundation | 1 |
| M1 — Build system | Foundation | 1 |
| M2 — numpy | Core ecosystem | 2 |
| M3 — pandas | Core ecosystem | 2 |
| M4 — Distribution | Foundation | 1 |
| M5 — sklearn | Core ecosystem | 1 |
| M6 — v1.0 | Release | 2 |
| | **subtotal** | **~10–12 months** |

After distribution v1.0 lands (Q1 2027), the original Wisp runtime work
(scheduler, SDK, sessions, smart router) starts on top. That's another
~6 months for v0.1 of the runtime.

**Total to "Wisp 0.1 with full ecosystem": Q3 2027.** ~16 months from now.

For a solo founder + AI on 13 hr/week, this is the limit of what's feasible.
Finding 1–2 long-term collaborators is the highest-leverage move; without
them, the schedule probably slips to Q4 2027 or later.

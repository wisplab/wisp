# Wisp Runtime — WASI Python Distribution

This subtree builds a **WASI-native Python distribution** that ships CPython
plus a curated set of data-science packages (numpy, pandas, sklearn, …) cross-
compiled to `wasm32-wasip1`. The artifact runs in any WASI runtime
(Wasmtime, Wasmer, WasmEdge, …) without requiring a JavaScript host.

This is the foundation layer of the Wisp project. The agent runtime, smart
router, and SDK all sit on top of this distribution.

See [`ROADMAP.md`](ROADMAP.md) for the full milestone plan (M0–M6, ~10–12 months).

## Subdirectories

```
runtime/
├── ROADMAP.md             — milestones, scope, schedule
├── README.md              — this file
└── cpython-wasi/          — M0: CPython 3.14 cross-compile to wasm32-wasip1
    ├── build.sh           — full build script
    ├── toolchain/         — wasi-sdk, wasmtime CLI (gitignored)
    ├── vendor/            — CPython source (gitignored)
    └── build/             — output binaries (gitignored)
```

## Quick start

```bash
cd cpython-wasi
./build.sh
# Output: build/wasi/python.wasm
toolchain/wasmtime/wasmtime --dir=. build/wasi/python.wasm -c 'print("hi")'
```

Build takes ~30–60 min on M-series hardware (downloads ~250 MB of toolchain +
source, compiles host CPython then cross-compiles to WASI).

## Why "WASI Python distribution" and not "use Pyodide"

See [`../bench/wasm-coldstart/NOTES.md`](../bench/wasm-coldstart/NOTES.md) for
the analysis. Short version: Pyodide is built for emscripten (JS-host
runtime), not WASI. To run Pyodide in Wasmtime we'd need to either bridge
emscripten ABI (3–6 weeks of fragile glue) or run inside a JS engine
(defeats the purpose). Building our own WASI-native distribution is the
cleaner long-term answer — and the foundation work is done once, used by
many.

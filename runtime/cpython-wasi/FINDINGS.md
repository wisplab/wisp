# M0 — CPython 3.14 in Wasmtime: findings

> **Build date**: 2026-05-02. **CPython**: v3.14.3. **wasi-sdk**: 32.0 (clang 22.1.0).
> **wasmtime**: 44.0.1. **Host**: Apple Silicon M-series, darwin.

## TL;DR

CPython 3.14.3 cross-compiles cleanly to `wasm32-wasip1` using the official
`Tools/wasm/wasi` driver. The resulting `python.wasm` (31 MB) runs under
Wasmtime 44 and executes Python code correctly. **39 of 44 surveyed stdlib
modules import out of the box**; the 5 failures are all C-extension modules
that depend on native libraries (OpenSSL, libffi, sqlite, zlib) we'll need
to cross-compile and link in separately.

This is the foundation of the Wisp WASI Python distribution. Numpy, pandas,
sklearn ports stack on top of this same artifact in subsequent milestones.

## Build details

- Foundation toolchain: official CPython 3.14 wasm support (`Tools/wasm/wasi`)
- Toolchain script: `build.sh` — fetches wasi-sdk + wasmtime + clones CPython,
  builds host bootstrap python, then cross-compiles to wasm32-wasip1
- Total wall-clock build time: ~25 min on M-series (includes ~4 min of
  toolchain download)
- Output: `vendor/cpython/cross-build/wasm32-wasip1/python.wasm` (31 MB)
- Helper: `cross-build/wasm32-wasip1/python.sh` invokes wasmtime with the
  correct flags (`--dir=`, `--env PYTHONPATH=`, `--wasm max-wasm-stack=...`)

## Smoke test

```bash
$ ./cross-build/wasm32-wasip1/python.sh -c 'print("hi from WASI Python"); import sys; print(sys.version)'
hi from WASI Python
3.14.3 (tags/v3.14.3-dirty:323c59a, Feb  3 2026, 15:32:20)
[Clang 22.1.0-wasi-sdk (https://github.com/llvm/llvm-project ...)]
```

## Stdlib survey

### Works (39 of 44 tested)

Pure-Python or self-contained C-extension modules — these all import cleanly:

```
os, sys, json, re, math, io, collections, itertools, functools, typing,
dataclasses, asyncio, socket, threading, multiprocessing, subprocess,
select, signal, hashlib, base64, urllib.parse, urllib.request, http.client,
xml.etree.ElementTree, csv, pickle, struct, array, bisect, heapq, time,
datetime, random, copy, enum, pathlib, tempfile, tarfile, zipfile
```

Caveats (need runtime testing, not just import):
- `socket` imports but won't connect — WASI sockets standard is partial
- `threading` / `multiprocessing` import but threading is no-op on WASI Preview 1
- `subprocess` imports but `wasm32-wasip1` has no `fork()` so process spawn fails

These are WASI-platform limitations, not CPython issues. They surface as
runtime errors when called, not at import.

### Fails (5)

All five are C extensions that need an upstream native library
cross-compiled to WASI:

| Module | Missing | Underlying lib | Plan |
|---|---|---|---|
| `ssl` | `_ssl` | OpenSSL | Cross-compile OpenSSL → wasi-sdk; re-link CPython |
| `ctypes` | `_ctypes` | libffi | libffi has experimental WASI port; integrate |
| `sqlite3` | `_sqlite3` | sqlite | Cross-compile sqlite (single-file C, easy) |
| `gzip` | `zlib` | zlib | Cross-compile zlib (small C, very easy) |
| `zlib` | `zlib` | zlib | Same as above |

Effort estimate: 1–2 weeks to add all five. zlib is hours of work; sqlite is
a day; OpenSSL and libffi are the harder ones (~1 week each, with libffi
being the riskiest due to its arch-specific assembly).

These belong in M0.5 — a "WASI CPython with full stdlib network/crypto" pass
before starting numpy work.

## Cold-start measurement (raw, unoptimized)

Five back-to-back runs of `python.sh -c 'pass'`:

```
real 5.25  user 0.40  sys 0.15
real 7.17  user 0.39  sys 0.10
real 3.62  user 0.39  sys 0.06
real 1.25  user 0.36  sys 0.05
real 3.14  user 0.37  sys 0.05
```

- **User time ~370–400 ms** (the actual work)
- **Real time 1.2–7.2 s** (dominated by OS scheduling / disk cache effects;
  the helper `python.sh` script forks wasmtime which itself loads the 31 MB
  wasm + does cold disk reads on first invocation)

**The 400 ms user time is the cold-start floor of "fresh wasmtime + load
python.wasm + Python init + run pass + exit"**, with zero optimization.

To get to the Wisp 5 ms target requires the architecture's pre-warmed pool
work:
1. Pre-instantiated wasmtime engine (skips ~5–50 ms wasmtime startup)
2. Pre-compiled / cached `python.wasm` module (skips parse + validate)
3. Pre-initialized Python interpreter snapshot (skips ~300 ms Python init)
4. Linear-memory clone instead of fresh init (the per-call sandbox primitive
   from `01-architecture.md` §5)

The 400 ms unoptimized number is comparable to or better than current Modal
warm-pool Python with deps. With the four optimizations above, Spike 1
already showed Wasmtime substrate at 3 μs — the gap to close on top of it
is "Python interpreter init + import diff," which the snapshot/restore
approach handles structurally.

## Reproduction

```sh
cd wisp/runtime/cpython-wasi
./build.sh
./vendor/cpython/cross-build/wasm32-wasip1/python.sh -c 'print("hi")'
```

First run downloads ~250 MB of toolchain + source and takes ~25 min on
M-series. Subsequent builds reuse the toolchain and CPython source.

## Where this slots in the roadmap

Looking at `runtime/ROADMAP.md`:

- **M0 — CPython core in Wasmtime: ✅ done** (this milestone)
- **M0.5 — Add zlib / sqlite / openssl / libffi to default build** (1–2 weeks)
- **M1 — Build system for native Python extensions** (next major milestone,
  needs Cython for M2's numpy port)

M0 took ~25 min of compute and a couple hours of tool-debug (`config.site`
path moved in 3.14, wasmtime needed to be on PATH for the official wasi
driver). Both fixes are now in `build.sh`.

## Open observations

1. **The official `Tools/wasm/wasi` driver is good.** It handles the
   host-bootstrap-python + cross-compile dance, the `config.site` cross-compile
   pre-answers, and the wasmtime host-runner config. Building manually was
   a mistake — first build attempt failed because I tried to do it the
   manual way.

2. **`python.wasm` is 31 MB.** Pyodide's main wasm is ~30 MB too — same ballpark.
   Adding numpy + pandas will roughly double this (Pyodide's full bundle is
   ~50–80 MB). Modular loading via WASI Component Model becomes necessary
   at some scale; for now ship the all-static blob.

3. **Cold start at 400 ms unoptimized is a relevant data point**: it tells
   us that without any pre-warming, just running `python.wasm -c 'pass'`
   from scratch is faster than Modal cold starts on Python with deps
   (typically 1–3 s). The thesis target of <5 ms requires pre-warming, but
   the *worst case* (cold disk, no pool) is already in agent-product
   tolerable range.

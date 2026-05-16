# scripts/numpy/ — building numpy 1.26 to wasm32-wasip1

The M1 pipeline for the big one. xxhash was the proof-of-concept (5 C
files, one TU); numpy is the headline: ~100 C/C++ files across
multiarray + umath + npymath, plus a code-generation step, plus
hand-written platform config.

## Status as of 2026-05-16 (Day 1)

| Stage | Script | State |
|---|---|---|
| Vendor numpy source | `01-prepare.sh` first run | ✅ `vendor/numpy-1.26.4/` |
| Template expansion (.c.src/.h.src/.inc.src) | `01-prepare.sh` | ✅ 22 plain templates done; 17 `*.dispatch.c.src` deferred |
| Code generators (API / umath) | `01-prepare.sh` | ✅ 6 files generated (`__multiarray_api.{c,h}`, `__ufunc_api.{c,h}`, `__umath_generated.c`, `_umath_doc_generated.h`) |
| `_numpyconfig.h` + `config.h` hand-write | `02-config.sh` | ✅ includes `HAVE_LDOUBLE_IEEE_DOUBLE_LE`, `NPY_RELAXED_STRIDES_DEBUG=0` |
| `npy_cpu.h` patched for `__wasi__` | `02-config.sh` | ✅ |
| Compile one .c (alloc.c) with WASI SDK | `03-compile-one.sh` | ✅ 43 KB wasm32 .o |
| Compile all multiarray + umath + common + npymath (.c + .cpp) | `04-compile-all.sh` | ✅ **100/100 .o, 4.3 MB total. `PyInit__multiarray_umath` exported.** |
| Archive .o's into `libnumpy.a` | `05-archive.sh` | ⬜ TODO |
| Static-link `libnumpy.a` into reactor.wasm | wisp_entry/build.sh add or Setup.local | ⬜ TODO |
| Copy pure-Python `numpy/` to PYTHONPATH | `06-stage-python.sh` | ⬜ TODO |
| `import numpy` works in sandbox | end-to-end | ⬜ TODO |
| SIMD dispatch path (the 17 `.dispatch.c.src`) | — | ⬜ deferred, scalar-only baseline first |
| `numpy.linalg` (BLAS dep) | — | ⬜ deferred |
| `numpy.fft`, `numpy.random` | — | ⬜ deferred |

What got us from "50 fails" to "100/100" in one session — three fixes:

1. **`HAVE_LDOUBLE_IEEE_DOUBLE_LE`** in `config.h` — wasm32 long double
   is identical to little-endian IEEE 754 double; saying so unblocked
   ~46 files (all `LDBL_EXP_MASK / IEEEl2bitsrep` errors).
2. **`NPY_RELAXED_STRIDES_DEBUG=0`** in `config.h` — used as a value
   (`if (… || NPY_RELAXED_STRIDES_DEBUG)`), not just an ifdef, so it
   has to be defined to a literal.
3. **Skip compiling `__multiarray_api.c` / `__ufunc_api.c` /
   `__umath_generated.c` as standalone TUs** — `multiarraymodule.c`
   and `umathmodule.c` `#include` them. They share the host's symbol
   scope. The `-I"$GEN"` already on the path makes the includes
   resolve.

Realistic timeline for `import numpy` end-to-end: 3–5 more evenings.
Hardest remaining steps are probably (a) linking the .o's into the
reactor without symbol collisions and (b) staging numpy's pure-Python
tree so the C extension can actually be imported as `numpy._core.…`.

## Pipeline stages

### Stage 1 — `01-prepare.sh`

Two things, both must happen before any .c can compile:

1. **Template expansion.** numpy's `.c.src` and `.h.src` files use
   `/**begin repeat … **/end repeat**/` blocks to generate per-type
   variants of the same code (one for int32, one for int64, …). We
   run `numpy/distutils/conv_template.py` over the 21 non-dispatch
   templates and write `.c` / `.h` siblings next to the `.src`.
   Skip the 17 `*.dispatch.c.src` files — those use a separate
   CPU-dispatch system (NPYV) that needs more setup, deferred.

2. **Codegen.** numpy ships Python scripts that emit:
   - `__multiarray_api.{c,h}` from `generate_numpy_api.py` (the
     PyArray_API exported-symbol table)
   - `__ufunc_api.{c,h}` from `generate_ufunc_api.py`
   - `__umath_generated.c` from `generate_umath.py` (6400+ lines
     wiring up every ufunc loop)
   - `_umath_doc_generated.h` from `generate_umath_doc.py`

   These land in `vendor/numpy-1.26.4/build-wasi/generated/`.

### Stage 2 — `02-config.sh`

Three hand-written / patched files compensate for the fact that
setup.py's auto-probe path can't execute cross-compile to wasm:

- **`_numpyconfig.h`** (public): wasm32 ABI sizes (long=4, intptr=4,
  off_t=8, long double=8, …) and feature flags (`NPY_NO_SIGNAL=1`,
  `NPY_NO_SMP=1`).
- **`config.h`** (internal): the `HAVE_FUNC` / `HAVE_HEADER` /
  `HAVE_INTRINSIC` set, hand-encoded for what wasi-sdk + WASI sysroot
  actually provide. wasi has C99 math, complex.h, sys/mman.h (via
  emulated mman), strtoll, etc. It does NOT have AVX intrinsics,
  execinfo.h, dlfcn.h, xlocale.h.
- **`npy_cpu.h`** patched: numpy already had `NPY_CPU_WASM` but
  gated it on `__EMSCRIPTEN__` only. Extended to also accept
  `__wasi__` (the WASI-SDK toolchain macro).
- **`npy_config.h`** appended: WASI-only block that hard-disables
  SIMD/CPU-dispatch attributes — `NPY_DISABLE_OPTIMIZATION=1` plus
  `#undef` of `NPY_HAVE_SSE2_INTRINSICS` etc.

### Stage 3 — `03-compile-one.sh` (sanity check)

Compiles `alloc.c` only. Validates the `-I` set, the WASI SDK clang
invocation, and that `Python.h` / `ndarraytypes.h` / `npy_config.h`
all parse. Once this works (it does, as of Day 1), the full per-file
loop in Stage 3b should mostly just work.

## Reproducing what's here

```sh
# 1. Vendor numpy 1.26.4 source (one-time; ~80 MB extracted)
cd runtime/cpython-wasi/vendor
curl -L -O https://github.com/numpy/numpy/releases/download/v1.26.4/numpy-1.26.4.tar.gz
tar xzf numpy-1.26.4.tar.gz

# 2. Run pipeline
bash scripts/numpy/01-prepare.sh   # template expand + codegen
bash scripts/numpy/02-config.sh    # write config headers + patch npy_cpu.h
bash scripts/numpy/03-compile-one.sh   # compile alloc.c, sanity check
```

## Why numpy 1.26.4 (last 1.x) and not 2.x

- 1.26.x still has `setup.py` + the `numpy.distutils.conv_template`
  CLI we use for template expansion. 2.x removed both — meson-only.
- meson cross-files for `wasm32-wasip1` exist but are not widely
  validated; debugging them is its own multi-day rabbit hole.
- 1.26 ABI is stable enough that real workloads compile against it.
- Pyodide itself shipped 1.26 for a long time before moving to 2.x.

We may bump to 2.x later once the 1.26 path is working and the
meson + WASI cross-file story is more battle-tested upstream.

## Known gotchas hit so far (Day 1)

1. **`numpy.distutils.conv_template` can't be imported from inside
   the numpy source tree** — `numpy/__init__.py` raises
   "you should not try to import numpy from its source directory".
   Workaround: run `conv_template.py` as a standalone script
   (`python3 numpy/distutils/conv_template.py file.c.src`); it's
   self-contained.
2. **`_numpyconfig.h` defining `NPY_SIZEOF_INTP` / `_UINTP` clashes
   with `npy_common.h`** — those are derived from
   `NPY_SIZEOF_PY_INTPTR_T` and must NOT be in `_numpyconfig.h`.
3. **`npy_cpu.h` `#error Unknown CPU`** — numpy supports wasm via
   `NPY_CPU_WASM` but the check is `defined(__EMSCRIPTEN__)`. Patched
   to also accept `__wasi__`.

## What's deferred (and why)

- **`*.dispatch.c.src` (17 files)**: needs numpy's NPYV dispatcher
  pass which generates multiple object files per source at different
  SIMD baseline targets. wasm has none of those baselines. Scalar-
  only baseline is sufficient for correctness; we'll add it as a
  Stage 1b.
- **`numpy.linalg`**: needs LAPACK; reference netlib LAPACK can
  compile to wasm but is ~50 more .c/.f files. Defer until ndarray
  basics work.
- **`numpy.fft._pocketfft_internal`**: small standalone C; should be
  easy after ndarray works.
- **`numpy.random._mt19937` / `_pcg64` / etc.**: separate C
  extensions, each ~5 files; tractable but not on the critical path
  for "import numpy" success.

## Next session checklist

1. Write `04-compile-all.sh` that loops over the multiarray + umath
   non-dispatch source list (multiarray_src + umath_src from
   `numpy/core/setup.py:840-993`). Skip files that aren't there
   (e.g. simd_qsort.cpp variants).
2. Cataloge per-file failures; expect ~5–10 to need individual
   patches (signature casts, missing intrinsic shims, etc.).
3. Once all .c compile, decide on link strategy: bundle into a
   single .a inside our CPython build tree, then add to Setup.local
   alongside `_multiarray_umath`.
4. Stage pure-Python `numpy/` into the PYTHONPATH mount.
5. Smoke test: `import numpy; print(numpy.array([1,2,3]).sum())`
   inside the sandbox via the wisp daemon.

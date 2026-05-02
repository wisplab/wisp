# scripts/ — M1 cross-build pipeline

Helpers for cross-compiling third-party Python packages with C extensions
into the Wisp WASI Python runtime.

## What's here

- `build-xxhash.sh` — proof of concept. Cross-compiles
  [python-xxhash 3.5.0](https://pypi.org/project/xxhash/) into a builtin
  module inside `python-reactor.wasm`. Run from anywhere; the script
  resolves its own paths.

## How it works (M1 v0)

A third-party C-extension package becomes a CPython builtin via four
steps. The xxhash script does all of them automatically.

### 1. Source staging

The package's `.c` and `.h` files get copied into
`runtime/cpython-wasi/vendor/cpython/Modules/<modname>/`. CPython's
own Makefile takes it from there; we don't shell out to wasi-sdk
directly. That way the module gets the same CFLAGS as built-in stdlib
modules — `Py_BUILD_CORE_BUILTIN`, internal include paths, the right
`-DXXX` flags. Matching those by hand is brittle and produces wasm
modules that load but trap at first call.

### 2. `Setup.local` entry

A line is appended to
`runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/Modules/Setup.local`:

```
_xxhash _xxhash/_xxhash.c -I$(srcdir)/Modules/_xxhash
```

CPython's `makesetup` script picks this up at configure time and:
- adds `_xxhash` to `MODBUILT_NAMES` in the generated Makefile
- emits a compile rule for `_xxhash.o` with `PY_BUILTIN_MODULE_CFLAGS`
- registers a `PyInit__xxhash` entry in `Modules/config.c` (the inittab)
- links the `.o` into `python.wasm` and `libpython3.14.a`

### 3. Pure-Python file copy

The package's `.py` files get copied into
`cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14/<package>/`.
That directory is on `PYTHONPATH` for our reactor.

### 4. Reactor relink

Once `python.wasm` is rebuilt, run `wisp_entry/build.sh` to relink
`python-reactor.wasm` with the new `libpython3.14.a`. The new module
is now reachable from `wisp_eval` through a normal `import xxhash`.

## WASM gotchas this script papers over

These are not unique to xxhash — any non-trivial C-extension cross-
compile will hit at least one of them.

| Problem | Fix in build-xxhash.sh | Why it happens |
|---|---|---|
| `wasm trap: indirect call type mismatch` calling `xxh64.update` etc. | Patch `_xxhash.c` so all `METH_NOARGS` handlers take `(self, PyObject *Py_UNUSED(ignored))` instead of `(self)`. | The xxhash binding casts one-arg functions to two-arg `PyCFunction`. Native ABIs ignore extra args; wasm enforces signature equality at indirect-call sites. |
| `wasm trap: indirect call type mismatch` deeper inside xxhash | `#define XXH_VECTOR 0` and `#define XXH_INLINE_ALL`, then `#include "xxhash.c"` from `_xxhash.c` (single TU). | xxhash's auto-detected SIMD dispatch table has function pointers that wasm's strict typecheck rejects. Inlining everything removes the indirect-call site. |
| `Modules/_xxhash/_xxhash.o: No such file or directory` from wasm-ld | Auto-create `cross-build/wasm32-wasip1/Modules/<modname>/`. | CPython's Makefile assumes the per-module build subdir already exists. |
| `Setup.local` lines silently dropped | Avoid `=` in the entry. Push `-DFOO=BAR` flags into source files, not the Setup.local line. | makesetup greedily treats any line containing `=` as a variable assignment and skips parsing it as a module entry. |
| `CPython mainline is `wasm32-wasip1`-only` | We stay on P1 (see `runtime/cpython-wasi/MIGRATION.md`). | No P2 driver upstream as of 2026-05-02. |

## Verify

```bash
bash wisp/scripts/build-xxhash.sh

# Then trigger CPython's relink:
cd wisp/runtime/cpython-wasi/vendor/cpython
PATH=".../wasmtime:$PATH" python3 Tools/wasm/wasi make-host

# And rebuild python-reactor.wasm:
wisp/runtime/cpython-wasi/wisp_entry/build.sh

# Standalone smoke test:
wisp/runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python.sh \
  -c "import xxhash; print(xxhash.xxh64(b'wisp').hexdigest())"
# → 490c61f4fa834ea2
```

## What's next

- Apply the same pattern to a non-trivial dataset library (a pure-Python
  one first to check the importer side end-to-end, then a small C
  binding like `lz4` or `cffi-less msgpack`).
- Generalize `build-xxhash.sh` into `build-package.sh <name>` that
  takes a package source dir and figures out the `Setup.local` entry
  from its `setup.py` `Extension(...)` definitions.
- Tackle numpy. Numpy uses meson + multiple C extensions; expect a
  much bigger script and probably its own MIGRATION-style rationale
  doc for the parts that need patching.

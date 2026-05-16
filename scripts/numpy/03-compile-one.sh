#!/usr/bin/env bash
# scripts/numpy/03-compile-one.sh
#
# Stage 3a: try to compile a single numpy .c with the WASI SDK clang,
# end-to-end. The goal here isn't to produce a useful artifact — it's
# a forcing function for finding every missing -I path, undefined
# symbol, and broken include before we automate the per-file compile
# loop (Stage 3b, deferred).
#
# Picks alloc.c because it's the smallest .c with the standard numpy
# include surface (Python.h, ndarraytypes.h, npy_config.h). If alloc.c
# compiles, the include / config story is sound.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
CPYTHON_DIR="$ROOT/vendor/cpython"

SRC="$NUMPY_DIR/numpy/core/src/multiarray/alloc.c"
OUT="$NUMPY_DIR/build-wasi/alloc.o"
mkdir -p "$(dirname "$OUT")"

CC="$WASI_SDK/bin/clang"
SYSROOT="$WASI_SDK/share/wasi-sysroot"
GEN="$NUMPY_DIR/build-wasi/generated"

# Flags inspired by what CPython itself uses for builtin modules
# (Py_BUILD_CORE_BUILTIN, internal include paths). Numpy-specific
# bits come from numpy/core/setup.py's add_extension args.
CFLAGS=(
  --target=wasm32-wasip1
  --sysroot="$SYSROOT"
  -O2
  -fPIC
  -DPy_BUILD_CORE_BUILTIN
  -DNPY_INTERNAL_BUILD=1
  -DHAVE_NPY_CONFIG_H=1
  -DNPY_NO_DEPRECATED_API=NPY_API_VERSION
  -DNPY_DISABLE_OPTIMIZATION=1
  # CPython internals
  -I"$CPYTHON_DIR/Include"
  -I"$CPYTHON_DIR/Include/internal"
  -I"$CPYTHON_DIR/cross-build/wasm32-wasip1"
  # numpy's own include trees
  -I"$NUMPY_DIR/numpy/core/include"
  -I"$NUMPY_DIR/numpy/core/src/multiarray"
  -I"$NUMPY_DIR/numpy/core/src/umath"
  -I"$NUMPY_DIR/numpy/core/src/common"
  -I"$NUMPY_DIR/numpy/core/src/npymath"
  -I"$NUMPY_DIR/numpy/core"
  # generated headers go in build-wasi/generated
  -I"$GEN"
)

echo "==> compiling alloc.c → alloc.o"
echo "    src: $SRC"
echo "    out: $OUT"
echo
set -x
"$CC" "${CFLAGS[@]}" -c "$SRC" -o "$OUT"
set +x
echo
echo "Done. Object file:"
ls -la "$OUT"
"$WASI_SDK/bin/llvm-nm" "$OUT" 2>&1 | head -10

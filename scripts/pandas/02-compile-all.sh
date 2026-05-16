#!/usr/bin/env bash
# scripts/pandas/02-compile-all.sh
#
# Stage 2: cross-compile every pandas .c/.cpp to wasm32-wasip1 .o.
# Mirrors the structure of scripts/numpy/04-compile-all.sh — same
# WASI SDK, same flag set, same per-file loop with pass/fail summary.
set -uo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
PANDAS_DIR="$ROOT/vendor/pandas-1.5.3"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
CPYTHON_DIR="$ROOT/vendor/cpython"

CC="$WASI_SDK/bin/clang"
CXX="$WASI_SDK/bin/clang++"
SYSROOT="$WASI_SDK/share/wasi-sysroot"
OBJS="$PANDAS_DIR/build-wasi/objs"
FAILURES="$PANDAS_DIR/build-wasi/failures.log"

mkdir -p "$OBJS"
: > "$FAILURES"

COMMON_FLAGS=(
  --target=wasm32-wasip1
  --sysroot="$SYSROOT"
  -O2
  -fPIC
  -DPy_BUILD_CORE_BUILTIN
  -DNPY_NO_DEPRECATED_API=0
  # CPython internals
  -I"$CPYTHON_DIR/Include"
  -I"$CPYTHON_DIR/Include/internal"
  -I"$CPYTHON_DIR/cross-build/wasm32-wasip1"
  # numpy include trees — pandas cimports `numpy as cnp` so it needs
  # numpy headers AND our hand-written _numpyconfig.h + config.h.
  -I"$NUMPY_DIR/numpy/core/include"
  -I"$NUMPY_DIR/numpy/core/src/multiarray"
  -I"$NUMPY_DIR/numpy/core/src/common"
  -I"$NUMPY_DIR/build-wasi/generated"
  # pandas's own headers
  -I"$PANDAS_DIR/pandas/_libs/src"
  -I"$PANDAS_DIR/pandas/_libs/src/headers"
  -I"$PANDAS_DIR/pandas/_libs/src/klib"
  -I"$PANDAS_DIR/pandas/_libs/src/parser"
  -I"$PANDAS_DIR/pandas/_libs/src/ujson/lib"
  -I"$PANDAS_DIR/pandas/_libs/src/ujson/python"
  -I"$PANDAS_DIR/pandas/_libs/tslibs/src/datetime"
)

PANDAS_SRC=(
  _libs/algos.c
  _libs/arrays.c
  _libs/groupby.c
  _libs/hashing.c
  _libs/hashtable.c
  _libs/index.c
  _libs/indexing.c
  _libs/internals.c
  _libs/interval.c
  _libs/join.c
  _libs/lib.c
  _libs/missing.c
  _libs/ops_dispatch.c
  _libs/ops.c
  _libs/parsers.c
  _libs/properties.c
  _libs/reduction.c
  _libs/reshape.c
  _libs/sparse.c
  _libs/testing.c
  _libs/tslib.c
  _libs/writers.c
  _libs/tslibs/base.c
  _libs/tslibs/ccalendar.c
  _libs/tslibs/conversion.c
  _libs/tslibs/dtypes.c
  _libs/tslibs/fields.c
  _libs/tslibs/nattype.c
  _libs/tslibs/np_datetime.c
  _libs/tslibs/offsets.c
  _libs/tslibs/parsing.c
  _libs/tslibs/period.c
  _libs/tslibs/strptime.c
  _libs/tslibs/timedeltas.c
  _libs/tslibs/timestamps.c
  _libs/tslibs/timezones.c
  _libs/tslibs/tzconversion.c
  _libs/tslibs/vectorized.c
  _libs/window/indexers.c
  io/sas/sas.c
)

PANDAS_SUPPORT_SRC=(
  _libs/src/parser/io.c
  _libs/src/parser/tokenizer.c
  _libs/src/ujson/lib/ultrajsondec.c
  _libs/src/ujson/lib/ultrajsonenc.c
  _libs/src/ujson/python/date_conversions.c
  _libs/src/ujson/python/JSONtoObj.c
  _libs/src/ujson/python/objToJSON.c
  _libs/src/ujson/python/ujson.c
  _libs/tslibs/src/datetime/np_datetime.c
  _libs/tslibs/src/datetime/np_datetime_strings.c
)

PANDAS_CPP=(
  _libs/window/aggregations.cpp
)

PASS=0
FAIL=0

compile_c() {
  local rel="$1"
  local src="$PANDAS_DIR/pandas/$rel"
  if [ ! -f "$src" ]; then
    echo "  ?? MISSING $rel" >> "$FAILURES"
    return
  fi
  local out="$OBJS/$(echo "$rel" | tr '/' '_' | sed 's/^\.*//').o"
  if "$CC" "${COMMON_FLAGS[@]}" -c "$src" -o "$out" 2>>"$FAILURES.tmp"; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "==== FAIL: $rel ====" >> "$FAILURES"
    cat "$FAILURES.tmp" >> "$FAILURES"
  fi
  : > "$FAILURES.tmp"
}

compile_cpp() {
  local rel="$1"
  local src="$PANDAS_DIR/pandas/$rel"
  if [ ! -f "$src" ]; then
    echo "  ?? MISSING $rel" >> "$FAILURES"
    return
  fi
  local out="$OBJS/$(echo "$rel" | tr '/' '_' | sed 's/^\.*//').o"
  if "$CXX" "${COMMON_FLAGS[@]}" -std=c++17 -c "$src" -o "$out" 2>>"$FAILURES.tmp"; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "==== FAIL: $rel ====" >> "$FAILURES"
    cat "$FAILURES.tmp" >> "$FAILURES"
  fi
  : > "$FAILURES.tmp"
}

echo "==> Compiling pandas cythonized .c (${#PANDAS_SRC[@]} files)"
for f in "${PANDAS_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling pandas support .c (${#PANDAS_SUPPORT_SRC[@]} files)"
for f in "${PANDAS_SUPPORT_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling pandas .cpp (${#PANDAS_CPP[@]} files)"
for f in "${PANDAS_CPP[@]}"; do compile_cpp "$f"; done

rm -f "$FAILURES.tmp"
TOTAL=$((PASS+FAIL))
echo
echo "==================================="
echo "  PASS: $PASS / $TOTAL"
echo "  FAIL: $FAIL"
echo "==================================="
if [ $FAIL -gt 0 ]; then
  echo "Failures cataloged in: $FAILURES"
  echo "First few:"
  grep "^==== FAIL:" "$FAILURES" | head -5
fi

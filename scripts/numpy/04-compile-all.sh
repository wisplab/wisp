#!/usr/bin/env bash
# scripts/numpy/04-compile-all.sh
#
# Stage 4: per-file compile loop. Take every .c / .cpp in numpy's
# multiarray + umath + common + npymath source lists (as enumerated in
# numpy/core/setup.py:840-993), compile each one with the WASI SDK,
# write a .o into build-wasi/objs/, and tabulate pass/fail at the end.
#
# This is the long-tail stage: each individual file might need its own
# patch (signature cast, missing intrinsic shim, etc). We expect some
# failures on first run; the goal is to catalog them in failures.log
# so they can be triaged one-by-one in the next session.
#
# Source lists are hard-coded rather than parsed from setup.py:
# setup.py has conditional includes (BLAS, has_svml, etc.) that we
# don't want; and a stable explicit list survives upstream churn
# better than a parser.
set -uo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
CPYTHON_DIR="$ROOT/vendor/cpython"

CC="$WASI_SDK/bin/clang"
CXX="$WASI_SDK/bin/clang++"
SYSROOT="$WASI_SDK/share/wasi-sysroot"
GEN="$NUMPY_DIR/build-wasi/generated"
OBJS="$NUMPY_DIR/build-wasi/objs"
FAILURES="$NUMPY_DIR/build-wasi/failures.log"

mkdir -p "$OBJS"
: > "$FAILURES"

# Shared flag set — same as 03-compile-one.sh.
COMMON_FLAGS=(
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
  # numpy include trees
  -I"$NUMPY_DIR/numpy/core/include"
  -I"$NUMPY_DIR/numpy/core/src/multiarray"
  -I"$NUMPY_DIR/numpy/core/src/umath"
  -I"$NUMPY_DIR/numpy/core/src/common"
  -I"$NUMPY_DIR/numpy/core/src/npymath"
  -I"$NUMPY_DIR/numpy/core"
  -I"$GEN"
)

# Source lists, derived from numpy/core/setup.py. Filenames given
# WITHOUT the .src suffix when they're templates — Stage 1 already
# expanded them.
COMMON_SRC=(
  src/common/array_assign.c
  src/common/mem_overlap.c
  src/common/npy_argparse.c
  src/common/npy_hashtable.c
  src/common/npy_longdouble.c
  src/common/ucsnarrow.c
  src/common/ufunc_override.c
  src/common/numpyos.c
  src/common/npy_cpu_features.c
)

MULTIARRAY_SRC=(
  src/multiarray/abstractdtypes.c
  src/multiarray/alloc.c
  src/multiarray/arrayobject.c
  src/multiarray/arraytypes.c           # .c.src expanded
  src/multiarray/array_coercion.c
  src/multiarray/array_method.c
  src/multiarray/array_assign_scalar.c
  src/multiarray/array_assign_array.c
  src/multiarray/arrayfunction_override.c
  src/multiarray/buffer.c
  src/multiarray/calculation.c
  src/multiarray/compiled_base.c
  src/multiarray/common.c
  src/multiarray/common_dtype.c
  src/multiarray/convert.c
  src/multiarray/convert_datatype.c
  src/multiarray/conversion_utils.c
  src/multiarray/ctors.c
  src/multiarray/datetime.c
  src/multiarray/datetime_strings.c
  src/multiarray/datetime_busday.c
  src/multiarray/datetime_busdaycal.c
  src/multiarray/descriptor.c
  src/multiarray/dlpack.c
  src/multiarray/dtypemeta.c
  src/multiarray/dragon4.c
  src/multiarray/dtype_transfer.c
  src/multiarray/dtype_traversal.c
  src/multiarray/einsum.c              # .c.src expanded
  src/multiarray/einsum_sumprod.c      # .c.src expanded
  src/multiarray/experimental_public_dtype_api.c
  src/multiarray/flagsobject.c
  src/multiarray/getset.c
  src/multiarray/hashdescr.c
  src/multiarray/item_selection.c
  src/multiarray/iterators.c
  src/multiarray/legacy_dtype_implementation.c
  src/multiarray/lowlevel_strided_loops.c  # .c.src expanded
  src/multiarray/mapping.c
  src/multiarray/methods.c
  src/multiarray/multiarraymodule.c
  src/multiarray/nditer_templ.c        # .c.src expanded
  src/multiarray/nditer_api.c
  src/multiarray/nditer_constr.c
  src/multiarray/nditer_pywrap.c
  src/multiarray/number.c
  src/multiarray/refcount.c
  src/multiarray/sequence.c
  src/multiarray/shape.c
  src/multiarray/scalarapi.c
  src/multiarray/scalartypes.c         # .c.src expanded
  src/multiarray/strfuncs.c
  src/multiarray/temp_elide.c
  src/multiarray/typeinfo.c
  src/multiarray/usertypes.c
  src/multiarray/vdot.c
  src/multiarray/textreading/conversions.c
  src/multiarray/textreading/field_types.c
  src/multiarray/textreading/growth.c
  src/multiarray/textreading/readtext.c
  src/multiarray/textreading/rows.c
  src/multiarray/textreading/stream_pyobject.c
  src/multiarray/textreading/str_to_int.c
  src/npymath/arm64_exports.c
)

# C++ files compiled with clang++
MULTIARRAY_CPP=(
  src/npysort/quicksort.cpp
  src/npysort/mergesort.cpp
  src/npysort/timsort.cpp
  src/npysort/heapsort.cpp
  src/npysort/radixsort.cpp
  src/npysort/selection.cpp
  src/npysort/binsearch.cpp
  src/multiarray/textreading/tokenize.cpp
)

UMATH_SRC=(
  src/umath/umathmodule.c
  src/umath/reduction.c
  src/umath/loops.c                    # .c.src expanded
  src/umath/matmul.c                   # .c.src expanded
  src/umath/dispatching.c
  src/umath/legacy_array_method.c
  src/umath/wrapping_array_method.c
  src/umath/ufunc_object.c
  src/umath/extobj.c
  src/umath/scalarmath.c               # .c.src expanded
  src/umath/ufunc_type_resolution.c
  src/umath/override.c
  src/umath/_scaled_float_dtype.c
)

UMATH_CPP=(
  src/umath/clip.cpp
  src/umath/string_ufuncs.cpp
)

# Dispatch files. In numpy's normal build these get compiled multiple
# times with different SIMD baselines (NPYV pass). For wasm there's no
# SIMD — we compile each as a single TU with NPY_DISABLE_OPTIMIZATION=1,
# which selects the scalar baseline via `#if NPY_SIMD` guards inside
# the source. _simd.dispatch.c is part of the _simd test module
# (separate extension), not _multiarray_umath, so skip it.
DISPATCH_SRC=(
  src/multiarray/argfunc.dispatch.c
  src/umath/loops_unary.dispatch.c
  src/umath/loops_unary_fp.dispatch.c
  src/umath/loops_unary_fp_le.dispatch.c
  src/umath/loops_unary_complex.dispatch.c
  src/umath/loops_arithm_fp.dispatch.c
  src/umath/loops_arithmetic.dispatch.c
  src/umath/loops_logical.dispatch.c
  src/umath/loops_minmax.dispatch.c
  src/umath/loops_trigonometric.dispatch.c
  src/umath/loops_umath_fp.dispatch.c
  src/umath/loops_exponent_log.dispatch.c
  src/umath/loops_hyperbolic.dispatch.c
  src/umath/loops_modulo.dispatch.c
  src/umath/loops_comparison.dispatch.c
  src/umath/loops_autovec.dispatch.c
)

NPYMATH_SRC=(
  src/npymath/npy_math.c
  src/npymath/ieee754.c                # .c.src expanded
  src/npymath/npy_math_complex.c       # .c.src expanded
)

NPYMATH_CPP=(
  src/npymath/halffloat.cpp
)

# Generated files are NOT compiled standalone — multiarraymodule.c and
# umathmodule.c #include them. They live in $GEN/ which is already on
# the -I path, so the host .c files find them.
#   multiarraymodule.c:89   #include "__ufunc_api.c"
#   multiarraymodule.c:4775 #include "__multiarray_api.c"
#   umathmodule.c:33        #include "__umath_generated.c"
GEN_SRC=()

# Counter + helper
PASS=0
FAIL=0

compile_c() {
  local rel="$1"
  local src="$NUMPY_DIR/numpy/core/$rel"
  if [ ! -f "$src" ]; then
    # Some files (textreading/, etc.) might not exist depending on numpy
    # version layout. Record as missing, not fail.
    echo "  ?? MISSING $rel" >> "$FAILURES"
    return
  fi
  # Output object name: dot-flatten directory separators
  local out="$OBJS/$(echo "$rel" | tr '/' '_').o"
  if "$CC" "${COMMON_FLAGS[@]}" -c "$src" -o "$out" 2>>"$FAILURES.tmp"; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "==== FAIL: $rel ====" >> "$FAILURES"
    cat "$FAILURES.tmp" >> "$FAILURES"
    : > "$FAILURES.tmp"
  fi
  : > "$FAILURES.tmp"
}

compile_cpp() {
  local rel="$1"
  local src="$NUMPY_DIR/numpy/core/$rel"
  if [ ! -f "$src" ]; then
    echo "  ?? MISSING $rel" >> "$FAILURES"
    return
  fi
  local out="$OBJS/$(echo "$rel" | tr '/' '_').o"
  if "$CXX" "${COMMON_FLAGS[@]}" -std=c++17 -c "$src" -o "$out" 2>>"$FAILURES.tmp"; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "==== FAIL: $rel ====" >> "$FAILURES"
    cat "$FAILURES.tmp" >> "$FAILURES"
  fi
  : > "$FAILURES.tmp"
}

compile_gen() {
  local src="$1"
  local out="$OBJS/$(basename "$src").o"
  # Generated files in numpy's normal build inherit the per-extension
  # defines that mark them as "internal to the module" — without these,
  # the API table arrays reference symbols that look opaque from outside.
  local extra=()
  case "$(basename "$src")" in
    __multiarray_api.c) extra=(-D_MULTIARRAYMODULE) ;;
    __ufunc_api.c|__umath_generated.c) extra=(-D_UMATHMODULE -D_MULTIARRAYMODULE) ;;
  esac
  if "$CC" "${COMMON_FLAGS[@]}" "${extra[@]}" -c "$src" -o "$out" 2>>"$FAILURES.tmp"; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    echo "==== FAIL: $(basename $src) ====" >> "$FAILURES"
    cat "$FAILURES.tmp" >> "$FAILURES"
  fi
  : > "$FAILURES.tmp"
}

echo "==> Compiling common (${#COMMON_SRC[@]} files)"
for f in "${COMMON_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling multiarray .c (${#MULTIARRAY_SRC[@]} files)"
for f in "${MULTIARRAY_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling multiarray .cpp (${#MULTIARRAY_CPP[@]} files)"
for f in "${MULTIARRAY_CPP[@]}"; do compile_cpp "$f"; done

echo "==> Compiling umath .c (${#UMATH_SRC[@]} files)"
for f in "${UMATH_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling umath .cpp (${#UMATH_CPP[@]} files)"
for f in "${UMATH_CPP[@]}"; do compile_cpp "$f"; done

echo "==> Compiling dispatch .c (${#DISPATCH_SRC[@]} files, scalar baseline)"
for f in "${DISPATCH_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling npymath .c (${#NPYMATH_SRC[@]} files)"
for f in "${NPYMATH_SRC[@]}"; do compile_c "$f"; done

echo "==> Compiling npymath .cpp (${#NPYMATH_CPP[@]} files)"
for f in "${NPYMATH_CPP[@]}"; do compile_cpp "$f"; done

if [ ${#GEN_SRC[@]} -gt 0 ]; then
  echo "==> Compiling generated (${#GEN_SRC[@]} files)"
  for f in "${GEN_SRC[@]}"; do compile_gen "$f"; done
fi

rm -f "$FAILURES.tmp"
TOTAL=$((PASS+FAIL))
echo
echo "==================================="
echo "  PASS: $PASS / $TOTAL"
echo "  FAIL: $FAIL"
echo "==================================="
echo
if [ $FAIL -gt 0 ]; then
  echo "Failures cataloged in:"
  echo "  $FAILURES"
  echo
  echo "First 5 failing files:"
  grep "^==== FAIL:" "$FAILURES" | head -5
fi

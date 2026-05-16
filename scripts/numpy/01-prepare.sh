#!/usr/bin/env bash
# scripts/numpy/01-prepare.sh
#
# Stage 1 of the numpy → WASI build pipeline: expand all .c.src/.h.src
# templates AND run numpy's code generators. Output: a numpy source tree
# where every file the compiler will see actually exists on disk.
#
# numpy is normally driven by setup.py / meson, both of which do these
# steps implicitly. We do them by hand because we don't run setup.py
# (it auto-runs CPU feature probes via execve, which doesn't work for
# cross-compile to wasm) and we don't run meson (1.26.x's meson path
# is half-broken and not the focus).
#
# What this DOES NOT do yet:
#   - Expand the 17 `*.dispatch.c.src` files (those drive numpy's SIMD
#     dispatch tables and need NPY_DISABLE_CPU_FEATURES + a custom
#     dispatcher pass; deferred to Stage 1b when we tackle SIMD).
#   - Compile anything (Stage 2).
#   - Link into reactor.wasm (Stage 3).
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi/vendor && pwd)"
NUMPY_DIR="$ROOT/numpy-1.26.4"
CODEGEN_DIR="$NUMPY_DIR/numpy/core/code_generators"
OUT_DIR="$NUMPY_DIR/build-wasi/generated"

if [ ! -d "$NUMPY_DIR" ]; then
  echo "FATAL: $NUMPY_DIR not found. Run download step first." >&2
  exit 1
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

echo "==> Expanding .c.src / .h.src templates (non-dispatch only)"
COUNT=0
FAILED=()
for f in $(find "$NUMPY_DIR/numpy/core/src" -name "*.c.src" ! -name "*.dispatch.c.src") \
         $(find "$NUMPY_DIR/numpy/core/src" -name "*.h.src") \
         $(find "$NUMPY_DIR/numpy/core/src" -name "*.inc.src") \
         $(find "$NUMPY_DIR/numpy/core/include" -name "*.h.src" 2>/dev/null); do
  if python3 "$NUMPY_DIR/numpy/distutils/conv_template.py" "$f" >/dev/null 2>&1; then
    COUNT=$((COUNT+1))
  else
    FAILED+=("$f")
  fi
done
echo "  expanded $COUNT files"
if [ ${#FAILED[@]} -gt 0 ]; then
  echo "  FAILED on ${#FAILED[@]} files:"
  printf "    %s\n" "${FAILED[@]}"
  exit 1
fi

echo "==> Running numpy's API code generators"
cd "$CODEGEN_DIR"

# __multiarray_api.h, __multiarray_api.c
python3 generate_numpy_api.py -o "$OUT_DIR" >/dev/null
echo "  -> __multiarray_api.{c,h}"

# __ufunc_api.h, __ufunc_api.c
python3 generate_ufunc_api.py -o "$OUT_DIR" >/dev/null
echo "  -> __ufunc_api.{c,h}"

# __umath_generated.c (the bulk of ufunc registrations)
python3 -c "
import sys
sys.path.insert(0, '.')
import generate_umath
out = generate_umath.make_code(generate_umath.defdict, generate_umath.__file__)
with open('$OUT_DIR/__umath_generated.c', 'w') as f: f.write(out)
print('  ->', '__umath_generated.c', len(out.splitlines()), 'lines')
"

# _umath_doc_generated.h
python3 -c "
import sys
sys.path.insert(0, '.')
import generate_umath_doc
generate_umath_doc.write_code('$OUT_DIR/_umath_doc_generated.h')
print('  -> _umath_doc_generated.h')
"

echo
echo "==> Stage 1 done. Generated headers + bulk code in:"
echo "    $OUT_DIR"
echo
echo "  Next: scripts/numpy/02-config.sh — write the hand-tuned"
echo "  _numpyconfig.h / npy_config.h for WASI (since the auto-probe"
echo "  path that setup.py uses doesn't work cross-compile to wasm)."

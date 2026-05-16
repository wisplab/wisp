#!/usr/bin/env bash
# scripts/pandas/01-cython.sh
#
# Stage 1: cython all pandas 1.5.3 .pyx → .c. Pure Cython run, no
# tempita templates this time (pandas 1.5 doesn't ship any .pyx.in).
#
# Pandas 1.5 declares Cython >=0.29.32,<3. We use 0.29.37.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
PANDAS_DIR="$ROOT/vendor/pandas-1.5.3"
CYTHON_VENV="${CYTHON_VENV:-/tmp/cython-venv}"

if [ ! -x "$CYTHON_VENV/bin/cython" ]; then
  echo "FATAL: Cython venv not found at $CYTHON_VENV" >&2
  exit 1
fi
CYTHON="$CYTHON_VENV/bin/cython"

# Pandas's setup.py does this via `maybe_cythonize`. We replicate the
# pieces it needs: language_level=3, includes for numpy/pandas-internal
# pxd, plus the cython directives pandas uses.
INCLUDE_DIRS=(
  -I "$PANDAS_DIR/pandas/_libs"
  -I "$PANDAS_DIR/pandas/_libs/tslibs"
)

# Cython runs against numpy's pxd headers — point at our staged numpy
# (with .pxd files preserved). pandas 1.5 cimports `numpy as cnp`.
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
INCLUDE_DIRS+=(-I "$NUMPY_DIR")
INCLUDE_DIRS+=(-I "$NUMPY_DIR/numpy/core/include")

echo "==> Patching khash.pxd to const-correct kh_get_str_starts_item"
# parsers.pyx declares `const char *word` and calls kh_get_str_starts_item
# with it. The .pxd declares the param as `char *` (non-const), so Cython
# tries to round-trip word through Python to widen — illegal under nogil.
# Make the .pxd const-correct.
KHASH_PXD="$PANDAS_DIR/pandas/_libs/khash.pxd"
if [ -f "$KHASH_PXD" ] && ! grep -q "WISP_CONST_KEY_PATCH" "$KHASH_PXD"; then
  $CYTHON_VENV/bin/python3 - "$KHASH_PXD" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
src = p.read_text()
src = src.replace(
    "kh_put_str_starts_item(kh_str_starts_t* table, char* key,",
    "kh_put_str_starts_item(const kh_str_starts_t* table, const char* key,  # WISP_CONST_KEY_PATCH",
)
src = src.replace(
    "kh_get_str_starts_item(kh_str_starts_t* table, char* key)",
    "kh_get_str_starts_item(const kh_str_starts_t* table, const char* key)",
)
p.write_text(src)
print(f"  patched {p.name}")
PY
fi

echo "==> Cythonizing pandas .pyx files (Cython $($CYTHON --version 2>&1 | awk '{print $3}'))"
COUNT=0
FAILED=()
for f in $(find "$PANDAS_DIR/pandas" -name "*.pyx"); do
  if "$CYTHON" -3 \
      --directive language_level=3,infer_types=True \
      "${INCLUDE_DIRS[@]}" "$f" 2>/tmp/pandas-cython-err.tmp; then
    COUNT=$((COUNT+1))
  else
    FAILED+=("$f")
    echo "  FAIL: $(basename $(dirname $f))/$(basename $f)" >&2
  fi
done
echo "  cythonized $COUNT files"
if [ ${#FAILED[@]} -gt 0 ]; then
  echo
  echo "FAILED on ${#FAILED[@]} files:"
  printf "  %s\n" "${FAILED[@]}"
  echo
  echo "Last error log:"
  cat /tmp/pandas-cython-err.tmp
  exit 1
fi
rm -f /tmp/pandas-cython-err.tmp
echo
echo "Stage 1 done. Generated .c files in pandas/_libs/."
ls "$PANDAS_DIR/pandas/_libs"/*.c 2>/dev/null | wc -l

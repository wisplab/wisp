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
  -I "$PANDAS_DIR"
  -I "$PANDAS_DIR/pandas/_libs"
  -I "$PANDAS_DIR/pandas/_libs/tslibs"
)

# Cython runs against numpy's pxd headers — point at our staged numpy
# (with .pxd files preserved). pandas 1.5 cimports `numpy as cnp`.
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
INCLUDE_DIRS+=(-I "$NUMPY_DIR")
INCLUDE_DIRS+=(-I "$NUMPY_DIR/numpy/core/include")

echo "==> Patching parsers.pyx + khash.pxd for nogil const-correct kh calls"
# Root cause: khash_python.h:421 declares
#   kh_get_str_starts_item(const kh_str_starts_t* table, const char* key)
# but khash.pxd had non-const params and call sites in parsers.pyx use
# `const char *word` + `const kh_str_starts_t *<hashset>`. Cython
# refuses the mismatch under nogil. Patch BOTH sides:
#   - khash.pxd: declare both params const (matches the C header)
#   - parsers.pyx: explicitly remove `nogil` from the affected
#     function body. Reason: even with .pxd const-fixed, Cython 0.29 /
#     3.0 still rejects the call inside `with nogil:` blocks. Easier
#     path: convert `with nogil:` to a plain block; the perf loss is
#     small (parsers.pyx is one of many _libs files; only loses
#     parallelism on bulk read_csv parsing, not basic import).
KHASH_PXD="$PANDAS_DIR/pandas/_libs/khash.pxd"
if [ -f "$KHASH_PXD" ] && ! grep -q "WISP_CONST_KEY_PATCH" "$KHASH_PXD"; then
  $CYTHON_VENV/bin/python3 - "$KHASH_PXD" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
src = p.read_text()
src = src.replace(
    "    khuint_t kh_put_str_starts_item(kh_str_starts_t* table, char* key,",
    "    # WISP_CONST_KEY_PATCH\n    khuint_t kh_put_str_starts_item(const kh_str_starts_t* table, const char* key,",
)
src = src.replace(
    "kh_get_str_starts_item(kh_str_starts_t* table, char* key)",
    "kh_get_str_starts_item(const kh_str_starts_t* table, const char* key)",
)
p.write_text(src)
print(f"  patched {p.name}")
PY
fi

PARSERS_PYX="$PANDAS_DIR/pandas/_libs/parsers.pyx"
if [ -f "$PARSERS_PYX" ] && ! grep -q "WISP_NOGIL_DOWNGRADE" "$PARSERS_PYX"; then
  $CYTHON_VENV/bin/python3 - "$PARSERS_PYX" <<'PY'
import pathlib, re, sys
p = pathlib.Path(sys.argv[1])
src = p.read_text()
# Strip ALL `nogil` markers in parsers.pyx:
#   1. ` nogil:` at end of cdef function signature → `:`
#   2. `with nogil:` block → `if True:`
# Holds the GIL throughout (perf loss, but parsers.pyx is one file;
# basic `import pandas` works fine without nogil parallelism, only
# bulk CSV parsing throughput is affected).
# Order matters: replace `with nogil:` FIRST so the bare-`nogil:` strip
# in step 2 doesn't turn `with nogil:` into `with :` (syntax error).
new = re.sub(r"with nogil\s*:\s*$", "if True:", src, flags=re.MULTILINE)
new = re.sub(r"\bnogil\s*:\s*$", ":", new, flags=re.MULTILINE)
if new == src:
    print("WARN: parsers.pyx nogil downgrade matched zero sites", file=sys.stderr)
new = "# WISP_NOGIL_DOWNGRADE (scripts/pandas/01-cython.sh)\n" + new
p.write_text(new)
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

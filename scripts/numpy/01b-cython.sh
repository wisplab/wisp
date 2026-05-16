#!/usr/bin/env bash
# scripts/numpy/01b-cython.sh
#
# Stage 1b: pre-Cython numpy.random.
#
# numpy/random ships its bit-generator + distribution bindings as Cython
# .pyx files. The build normally runs Cython at install time; we run it
# here once so Stage 4 sees vanilla .c files.
#
# numpy 1.26 requires Cython 0.29.x specifically — 3.x has breaking
# changes around pxd parsing that break this build.
#
# One file (_bounded_integers.pyx.in) is a tempita template that has to
# be expanded before Cython runs.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
RANDOM_DIR="$NUMPY_DIR/numpy/random"
CYTHON_VENV="${CYTHON_VENV:-/tmp/cython-venv}"

if [ ! -x "$CYTHON_VENV/bin/cython" ]; then
  echo "FATAL: Cython venv not found at $CYTHON_VENV" >&2
  echo "  Create with:" >&2
  echo "    python3 -m venv $CYTHON_VENV" >&2
  echo "    HTTP_PROXY=... $CYTHON_VENV/bin/pip install 'cython<3'" >&2
  exit 1
fi

CYTHON="$CYTHON_VENV/bin/cython"

echo "==> Expanding tempita templates (.pyx.in / .pxd.in)"
# Tempita is bundled with Cython 0.29 (Cython.Tempita). .pxd.in must
# be expanded BEFORE the cython step because .pyx files cimport from
# the .pxd at compile time.
$CYTHON_VENV/bin/python3 - <<PY
import pathlib
from Cython.Tempita import Template

for stem_ext in ["_bounded_integers.pxd", "_bounded_integers.pyx"]:
    src_path = pathlib.Path("$RANDOM_DIR/" + stem_ext + ".in")
    if not src_path.exists():
        continue
    src = src_path.read_text()
    out = Template(src).substitute({})
    pathlib.Path("$RANDOM_DIR/" + stem_ext).write_text(out)
    print(f"  -> {stem_ext}")
PY

echo "==> Patching mtrand.pyx for wasm32 ILP32 ABI"
# mtrand.pyx assumes LP64 (long == int64_t). On wasm32 long is 4 bytes,
# breaking the call to legacy_random_multinomial which takes int64_t*.
# Replace long with int64_t for the multinomial mnix path. Idempotent
# via a marker comment.
MTRAND="$RANDOM_DIR/mtrand.pyx"
if [ -f "$MTRAND" ] && ! grep -q "WISP_WASI_ILP32_MULTINOMIAL_PATCH" "$MTRAND"; then
  $CYTHON_VENV/bin/python3 - "$MTRAND" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
src = p.read_text()
# Three sites to patch. Two are the cdef declarations of legacy_random_multinomial
# and the local mnix; one is the runtime cast.
new = src
new = new.replace(
    "void legacy_random_multinomial(bitgen_t *bitgen_state, long n, long *mnix,",
    "# WISP_WASI_ILP32_MULTINOMIAL_PATCH\n    void legacy_random_multinomial(bitgen_t *bitgen_state, long n, int64_t *mnix,",
)
new = new.replace("cdef long *mnix",       "cdef int64_t *mnix")
new = new.replace("mnix = <long*>np.PyArray_DATA(mnarr)",
                  "mnix = <int64_t*>np.PyArray_DATA(mnarr)")
# Force the numpy array dtype to int64 so mnarr's storage matches.
new = new.replace(
    "multin = np.zeros(shape, dtype=int)",
    "multin = np.zeros(shape, dtype=np.int64)",
)
if new == src:
    print("  WARN: patch matched zero sites", file=sys.stderr); sys.exit(1)
p.write_text(new)
print("  patched mtrand.pyx for int64_t mnix")
PY
fi

# int64_t is in libc.stdint, cimport it at the top of mtrand.pyx if not already.
if ! grep -q "from libc.stdint cimport int64_t" "$MTRAND" 2>/dev/null; then
  $CYTHON_VENV/bin/python3 - "$MTRAND" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
text = p.read_text()
# Insert after first cimport block; safest is right after the existing
# `from libc.stdint cimport` line if present, otherwise after the first
# `cimport numpy as np`.
if "from libc.stdint cimport" in text:
    text = text.replace(
        "from libc.stdint cimport",
        "from libc.stdint cimport int64_t,",
        1,
    )
else:
    text = text.replace(
        "cimport numpy as np",
        "cimport numpy as np\nfrom libc.stdint cimport int64_t",
        1,
    )
p.write_text(text)
print("  added int64_t cimport to mtrand.pyx")
PY
fi

echo "==> Running Cython on numpy/random/*.pyx"
cd "$RANDOM_DIR"
PYX_FILES=(
  _bounded_integers.pyx
  _common.pyx
  _generator.pyx
  _mt19937.pyx
  _pcg64.pyx
  _philox.pyx
  _sfc64.pyx
  bit_generator.pyx
  mtrand.pyx
)
for f in "${PYX_FILES[@]}"; do
  if [ ! -f "$f" ]; then
    echo "  FATAL: $f not found" >&2
    exit 1
  fi
  # infer_types=True is necessary so Cython can deduce that variables
  # like `mask` and `last_rng` (declared `mask = last_rng = 0` in
  # _bounded_integers.pyx) are uint64_t (from _gen_mask's return type)
  # and thus legal to assign inside a `with nogil:` block. Without
  # it Cython picks Python object and the nogil-assign rule fires.
  "$CYTHON" -3 --directive infer_types=True "$f"
  echo "  -> ${f%.pyx}.c"
done

echo
echo "Stage 1b done. Generated .c files in $RANDOM_DIR/:"
ls "$RANDOM_DIR"/*.c | head

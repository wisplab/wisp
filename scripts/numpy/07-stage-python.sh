#!/usr/bin/env bash
# scripts/numpy/07-stage-python.sh
#
# Stage 7: copy numpy's pure-Python tree into the cross-build PYTHONPATH
# location so `import numpy` resolves once the C extension is linked in.
#
# Source: vendor/numpy-1.26.4/numpy/    (all the .py / .pyi files)
# Dest:   vendor/cpython/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14/numpy/
#
# That destination is already on PYTHONPATH for the reactor (see
# crates/wisp-runtime/src/main.rs PYTHONPATH_GUEST).
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
DEST="$ROOT/vendor/cpython/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14/numpy"

if [ ! -d "$NUMPY_DIR/numpy" ]; then
  echo "FATAL: $NUMPY_DIR/numpy not found" >&2
  exit 1
fi

echo "==> Copying $NUMPY_DIR/numpy → $DEST"
rm -rf "$DEST"
mkdir -p "$DEST"
# Copy the whole tree, then strip C source / build artifacts.
# We keep .py / .pyi / package data; drop .c / .h / .src / build dirs.
cp -R "$NUMPY_DIR/numpy/." "$DEST/"

# Strip what we don't need at import time (saves snapshot space).
echo "==> Stripping source and test trees"
find "$DEST" -type d -name '__pycache__' -exec rm -rf {} + 2>/dev/null || true
find "$DEST" -type d -name 'tests' -exec rm -rf {} + 2>/dev/null || true
find "$DEST" -name '*.c' -delete 2>/dev/null || true
find "$DEST" -name '*.cpp' -delete 2>/dev/null || true
find "$DEST" -name '*.h' -delete 2>/dev/null || true
find "$DEST" -name '*.src' -delete 2>/dev/null || true
find "$DEST" -name '*.dispatch.c.src' -delete 2>/dev/null || true
rm -rf "$DEST/_build_utils" "$DEST/typing" 2>/dev/null || true

# build-wasi dir we created is also under numpy/; nuke it.
rm -rf "$DEST/../build-wasi" 2>/dev/null || true

# Generate stub numpy/__config__.py — normally written by setup.py
# during install with the toolchain it was built with. numpy/__init__.py
# imports `show` from it and ImportErrors if not found. Minimal version
# that satisfies the contract.
# Stub numpy/core/_multiarray_tests — only used by _add_newdocs.py to
# attach docstrings via add_newdoc(). We don't ship that C extension
# (it's purely for the numpy test suite), so provide a no-op stub.
cat > "$DEST/core/_multiarray_tests.py" <<'EOF'
"""Stub for numpy 1.26's testing-only C extension.

Only used by numpy/core/_add_newdocs.py to attach docstrings.
add_newdoc looks up symbols here via __import__ + getattr; we provide
the few it references as no-op callables so the lookup succeeds.
"""
def format_float_OSprintf_g(*args, **kwargs):
    raise NotImplementedError("not built into wisp WASI runtime")
EOF

# numpy.linalg._umath_linalg is now a real C extension built against
# numpy's bundled f2c-translated reference BLAS+LAPACK (lapack_lite).
# Remove any leftover stub from a prior run.
rm -f "$DEST/linalg/_umath_linalg.py"

# numpy.fft._pocketfft_internal is now a real C extension — compiled in
# libnumpy.a and registered in wisp_entry.c inittab. No stub needed.
# Remove any leftover stub from a prior run so it doesn't shadow the
# real init.
rm -f "$DEST/fft/_pocketfft_internal.py"

# numpy.random submodules are now real Cythonized C extensions. Remove
# any leftover Python stubs from a prior run so they don't shadow the
# real inittab entries.
for mod in _common _bounded_integers bit_generator _mt19937 _pcg64 _philox _sfc64 mtrand _generator; do
  rm -f "$DEST/random/$mod.py"
done

cat > "$DEST/__config__.py" <<'EOF'
"""Stub written by wisp/scripts/numpy/07-stage-python.sh.

Normally numpy generates this at install time with the toolchain / BLAS /
LAPACK / etc. it was compiled against. We cross-compile to WASI from a
hand-written config, so there's no probe data to record. The only
contract numpy/__init__.py needs is `show()`.
"""
from __future__ import annotations

__all__ = ["show"]


def show() -> None:
    print(
        "numpy build info (wisp WASI build):\n"
        "  target: wasm32-wasip1\n"
        "  optimization: scalar baseline (NPY_DISABLE_OPTIMIZATION=1)\n"
        "  BLAS / LAPACK: none\n"
        "  source: numpy 1.26.4"
    )
EOF

echo "==> $DEST"
du -sh "$DEST"
ls "$DEST" | head -15
echo
echo "Stage 7 done. Now smoke-test:"
echo "  WISP_CAPABILITIES_JSON=... ./target/release/wisp-runtime &"
echo "  curl -s -X POST http://127.0.0.1:9000/v1/eval \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"code\":\"import numpy; print(numpy.array([1,2,3]).sum())\"}'"

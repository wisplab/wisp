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

# Stub numpy.linalg._umath_linalg — normally a separate C extension
# built against BLAS/LAPACK. We don't have a wasm BLAS yet (deferred);
# the stub lets `import numpy` succeed by satisfying the import-time
# lookup. Any actual linalg call (solve, inv, eig, …) raises a clear
# NotImplementedError instead of returning bogus data.
cat > "$DEST/linalg/_umath_linalg.py" <<'EOF'
"""Stub for numpy.linalg._umath_linalg.

The real module is a C extension that calls into BLAS/LAPACK. We have
no BLAS yet in the WASI runtime (deferred), so any actual call raises
NotImplementedError. `import numpy` succeeds because module lookup
finds this file; failure happens only on first numerical use.
"""
__all__ = []

class _Missing:
    def __init__(self, name): self._name = name
    def __call__(self, *a, **k):
        raise NotImplementedError(
            f"numpy.linalg.{self._name} requires BLAS/LAPACK, "
            "which is not built into the wisp WASI runtime yet.")

def __getattr__(name):
    return _Missing(name)
EOF

# Stub numpy.fft._pocketfft_internal — separate C extension for FFT.
cat > "$DEST/fft/_pocketfft_internal.py" <<'EOF'
"""Stub for numpy.fft._pocketfft_internal (pocketfft C extension)."""
__all__ = []

def __getattr__(name):
    def _missing(*a, **k):
        raise NotImplementedError(
            f"numpy.fft.{name} not built into wisp WASI runtime yet.")
    return _missing
EOF

# Stub numpy.random submodules — each PRNG (_mt19937, _pcg64, _philox,
# _sfc64, _common, bit_generator) is a separate C extension.
for mod in _common _bounded_integers bit_generator _mt19937 _pcg64 _philox _sfc64 mtrand _generator; do
  cat > "$DEST/random/$mod.py" <<EOF
"""Stub for numpy.random.$mod (C extension)."""
# Empty __all__ so \`from .$mod import *\` succeeds with no symbols
# rather than tripping over __getattr__ returning a function.
__all__ = []

def __getattr__(name):
    def _missing(*a, **k):
        raise NotImplementedError(
            f"numpy.random.$mod.{name} not built into wisp WASI runtime yet.")
    return _missing
EOF
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

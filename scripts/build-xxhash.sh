#!/usr/bin/env bash
# M1 v0: cross-compile a third-party CPython C extension (xxhash) to
# wasm32-wasip1 and link it into our reactor as a builtin module.
#
# This is the proof of "we can extend the runtime with arbitrary
# pip-installable packages, not just stdlib stuff." Once xxhash works,
# the same recipe extends to lz4, msgpack, and (with much more work) numpy.
#
# Steps performed:
#   1. Compile src/_xxhash.c + deps/xxhash/xxhash.c with wasi-sdk
#   2. Drop the .o files into CPython's Modules/_xxhash directory
#   3. Append a Setup.local entry so makesetup picks _xxhash up as a builtin
#   4. Caller is responsible for re-running make-host + wisp_entry/build.sh
#      and copying xxhash/*.py into the PYTHONPATH location
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd .. && pwd)/runtime/cpython-wasi"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
CPYTHON_DIR="$ROOT/vendor/cpython"
PKG_DIR="$ROOT/vendor/python-xxhash"

# Stage the C sources inside Modules/_xxhash/ so CPython's own Makefile
# picks them up with the right CFLAGS (Py_BUILD_CORE_BUILTIN, internal
# include paths, etc.). Manually invoking wasi-sdk clang with our own
# flags caused a "wasm trap: indirect call type mismatch" — the precise
# CFLAGS CPython uses for builtin modules are non-trivial, and matching
# them by hand is brittle.
SRC_DIR="$CPYTHON_DIR/Modules/_xxhash"
mkdir -p "$SRC_DIR"

echo "==> Staging _xxhash.c (with xxhash.c inlined + WASM signature patches)"
# Three things going on here, all driven by wasm32's strict indirect-call
# type check (which native x86_64/arm64 quietly tolerate):
#
# 1. Single-TU build: _xxhash.c includes xxhash.c with XXH_INLINE_ALL,
#    eliminating xxhash's internal function-pointer dispatch tables.
# 2. XXH_VECTOR=0 forces the scalar code path (no SIMD entry-point
#    dispatch).
# 3. The xxhash binding declares METH_NOARGS handlers as
#    `static PyObject *PYXXH<X>_<method>(PYXXH<X>Object *self)` but casts
#    them to PyCFunction (which is two-arg). On native ABIs the extra arg
#    is silently ignored; on wasm32 the indirect call traps. We sed-patch
#    those signatures to take an unused second arg.
#
# The XXH defines can't go through Setup.local: CPython's makesetup
# greedily treats any line containing `=` as a variable assignment,
# dropping the whole module entry.
cp "$PKG_DIR/deps/xxhash/xxhash.c" "$SRC_DIR/xxhash.c"
cp "$PKG_DIR/deps/xxhash/xxhash.h" "$SRC_DIR/xxhash.h"
cp "$PKG_DIR/deps/xxhash/xxh3.h"   "$SRC_DIR/xxh3.h"

# Stage _xxhash.c with all three patches.
cp "$PKG_DIR/src/_xxhash.c" "$SRC_DIR/_xxhash.c.in"
# Patch METH_NOARGS handlers to match PyCFunction's 2-arg signature.
# Each pattern matches `static PyObject *PYXXH..._<name>(PYXXH...Object *self)`
# and adds `, PyObject *Py_UNUSED(ignored)` before the closing paren.
sed -E 's/(static PyObject \*PYXXH[0-9A-Z_]+_(digest|hexdigest|intdigest|copy|reset)\(PYXXH[0-9A-Z_]+Object \*self)\)/\1, PyObject *Py_UNUSED(ignored))/' \
    "$SRC_DIR/_xxhash.c.in" > "$SRC_DIR/_xxhash.c.patched"
{
  echo '#define XXH_VECTOR 0'
  echo '#define XXH_INLINE_ALL'
  echo '#include "xxhash.c"'
  cat "$SRC_DIR/_xxhash.c.patched"
} > "$SRC_DIR/_xxhash.c"
rm -f "$SRC_DIR/_xxhash.c.in" "$SRC_DIR/_xxhash.c.patched"

echo "==> Wiring _xxhash into Setup.local (let make compile from source)"
SETUP_LOCAL="$CPYTHON_DIR/cross-build/wasm32-wasip1/Modules/Setup.local"
mkdir -p "$(dirname "$SETUP_LOCAL")"
# Replace any existing entry to keep this idempotent.
if grep -q "^_xxhash " "$SETUP_LOCAL" 2>/dev/null; then
  # remove the prior entry block (header + line)
  sed -i.bak '/^# Added by wisp\/build\/build-xxhash.sh/,/^_xxhash /d' "$SETUP_LOCAL"
  rm -f "$SETUP_LOCAL.bak"
fi
cat >> "$SETUP_LOCAL" <<EOF
# wisp/scripts/build-xxhash.sh
_xxhash _xxhash/_xxhash.c -I\$(srcdir)/Modules/_xxhash
EOF

echo "==> Copying pure-Python xxhash/ package"
PYLIB_DEST="$CPYTHON_DIR/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14"
mkdir -p "$PYLIB_DEST/xxhash"
cp -r "$PKG_DIR/xxhash"/*.py "$PKG_DIR/xxhash"/*.pyi "$PYLIB_DEST/xxhash/" 2>/dev/null || true
cp "$PKG_DIR/xxhash"/__init__.py "$PYLIB_DEST/xxhash/" 2>/dev/null || true

# CPython's makesetup registers our extension as top-level `_xxhash`, but
# the upstream package expects to import it as `xxhash._xxhash` (a submodule
# because setup.py uses ext_package='xxhash'). Patch the dotted relative
# import to a top-level absolute one.
sed -i.bak 's/from \._xxhash import/from _xxhash import/' "$PYLIB_DEST/xxhash/__init__.py"
rm -f "$PYLIB_DEST/xxhash/__init__.py.bak"
ls "$PYLIB_DEST/xxhash/" 2>&1 | head

echo
echo "Done. Now re-run:"
echo "  cd $ROOT/vendor/cpython"
echo "  PATH=\"$ROOT/toolchain/wasmtime:\$PATH\" python3 Tools/wasm/wasi make-host"
echo "  $ROOT/wisp_entry/build.sh"

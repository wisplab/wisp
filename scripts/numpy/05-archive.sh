#!/usr/bin/env bash
# scripts/numpy/05-archive.sh
#
# Stage 5: pack the 100 .o files from Stage 4 into a single static
# library libnumpy.a. wisp_entry/build.sh will -l it onto the link
# command for python-reactor.wasm in Stage 6.
#
# Why a .a not a .so: WASI Preview 1 has no dlopen. Anything Python
# is going to import as a C extension must be statically linked into
# the reactor wasm at build time. The .a is just a convenient bundle
# the wasm-ld linker can pull symbols out of.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
NUMPY_DIR="$ROOT/vendor/numpy-1.26.4"
OBJS="$NUMPY_DIR/build-wasi/objs"
LIB="$NUMPY_DIR/build-wasi/libnumpy.a"

if [ ! -d "$OBJS" ] || [ -z "$(ls "$OBJS"/*.o 2>/dev/null)" ]; then
  echo "FATAL: no .o files in $OBJS — run 04-compile-all.sh first" >&2
  exit 1
fi

AR="$WASI_SDK/bin/llvm-ar"
RANLIB="$WASI_SDK/bin/llvm-ranlib"

echo "==> Archiving $(ls "$OBJS"/*.o | wc -l | tr -d ' ') .o files into libnumpy.a"
rm -f "$LIB"
"$AR" rcs "$LIB" "$OBJS"/*.o
"$RANLIB" "$LIB"

echo "==> $LIB"
ls -la "$LIB"
echo
echo "==> verify libnumpy.a has PyInit__multiarray_umath"
"$WASI_SDK/bin/llvm-nm" "$LIB" 2>/dev/null | grep "PyInit__multiarray_umath" || {
  echo "FATAL: PyInit__multiarray_umath not found in archive" >&2
  exit 1
}
echo
echo "Done. Stage 6 next — wisp_entry/build.sh needs:"
echo "  - link libnumpy.a (path: $LIB)"
echo "  - link libnpymath.a (already in the archive as part of npymath/*.o)"
echo "  - wisp_entry.c needs to call PyImport_AppendInittab(\"_multiarray_umath\", PyInit__multiarray_umath)"

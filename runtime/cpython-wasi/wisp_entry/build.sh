#!/usr/bin/env bash
# Build python-reactor.wasm — CPython 3.14 in WASI Reactor mode.
#
# Reactor mode means the wasm exports _initialize (called once on instantiate)
# plus our custom wisp_init / wisp_eval / wisp_alloc / wisp_free. The host
# can then drive the embedded Python runtime directly, without going through
# _start, and snapshot/restore the linear memory between calls.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd .. && pwd)"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
CPYTHON_DIR="$ROOT/vendor/cpython"
WASI_BUILD_DIR="$CPYTHON_DIR/cross-build/wasm32-wasip1"
LIBPYTHON="$WASI_BUILD_DIR/libpython3.14.a"

OUT="$WASI_BUILD_DIR/python-reactor.wasm"

if [ ! -f "$LIBPYTHON" ]; then
  echo "FATAL: $LIBPYTHON not found. Run ../build.sh first to build CPython." >&2
  exit 1
fi

CC="$WASI_SDK/bin/clang"
SYSROOT="$WASI_SDK/share/wasi-sysroot"

echo "==> Compiling wisp_entry.c + libpython3.14.a → python-reactor.wasm"
"$CC" \
  --target=wasm32-wasip1 \
  --sysroot="$SYSROOT" \
  -mexec-model=reactor \
  -O2 \
  -fPIC \
  -DPy_BUILD_CORE \
  -I"$CPYTHON_DIR/Include" \
  -I"$CPYTHON_DIR/Include/internal" \
  -I"$WASI_BUILD_DIR" \
  -Wl,--export=wisp_init \
  -Wl,--export=wisp_eval \
  -Wl,--export=wisp_alloc \
  -Wl,--export=wisp_free \
  -Wl,-z,stack-size=4194304 \
  wisp_entry.c \
  "$LIBPYTHON" \
  "$WASI_BUILD_DIR/Modules/_decimal/libmpdec/libmpdec.a" \
  "$WASI_BUILD_DIR/Modules/expat/libexpat.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_Hash_BLAKE2.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_Hash_SHA3.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_Hash_MD5.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_Hash_SHA1.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_HMAC.a" \
  "$WASI_BUILD_DIR/Modules/_hacl/libHacl_Hash_SHA2.a" \
  -L"$ROOT/vendor/zlib-1.3.1" -lz \
  -L"$ROOT/vendor/sqlite-amalgamation-3530000" -lsqlite3 \
  -L"$ROOT/vendor/openssl-3.4.0/install/lib" -lcrypto \
  -ldl \
  -lwasi-emulated-getpid \
  -lwasi-emulated-signal \
  -lwasi-emulated-mman \
  -lwasi-emulated-process-clocks \
  -lm \
  -o "$OUT"

echo
echo "==> Built: $OUT ($(ls -la "$OUT" | awk '{print $5}') bytes)"
echo
echo "Exports:"
"$WASI_SDK/bin/llvm-nm" --extern-only --defined-only "$OUT" \
  | grep -E "wisp_|_initialize" || true

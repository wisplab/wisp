#!/usr/bin/env bash
# Build libsqlite3.a for wasm32-wasip1 using wasi-sdk.
#
# CPython's _sqlite3 extension requires sqlite3_set_authorizer to be present.
# An earlier build of this archive used -DSQLITE_OMIT_AUTHORIZATION which
# stripped that symbol → CPython configure marked _sqlite3 as missing.
#
# This script rebuilds without OMIT_AUTHORIZATION (and disables only the
# WASI-incompatible bits: threads, dlopen-style extensions).
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(pwd)"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
SQLITE_DIR="$ROOT/vendor/sqlite-amalgamation-3530000"

CC="$WASI_SDK/bin/clang"
AR="$WASI_SDK/bin/llvm-ar"
SYSROOT="$WASI_SDK/share/wasi-sysroot"

cd "$SQLITE_DIR"

echo "==> Cleaning old objects"
rm -f sqlite3.o libsqlite3.a

echo "==> Compiling sqlite3.c → sqlite3.o (wasm32-wasip1)"
"$CC" \
  --target=wasm32-wasip1 \
  --sysroot="$SYSROOT" \
  -c -O2 -fPIC \
  -DSQLITE_THREADSAFE=0 \
  -DSQLITE_OMIT_LOAD_EXTENSION=1 \
  -DSQLITE_OS_OTHER=0 \
  -DSQLITE_TEMP_STORE=2 \
  -DSQLITE_DISABLE_LFS=1 \
  -DSQLITE_DEFAULT_MEMSTATUS=0 \
  -DHAVE_USLEEP=1 \
  -o sqlite3.o sqlite3.c

echo "==> Archiving libsqlite3.a"
"$AR" rcs libsqlite3.a sqlite3.o

echo
echo "Built: $SQLITE_DIR/libsqlite3.a ($(ls -la libsqlite3.a | awk '{print $5}') bytes)"
echo
echo "Verifying sqlite3_set_authorizer is exported..."
"$WASI_SDK/bin/llvm-nm" --defined-only libsqlite3.a 2>/dev/null \
  | grep -E "sqlite3_(set_authorizer|prepare_v2|open|exec)$" | sort -u

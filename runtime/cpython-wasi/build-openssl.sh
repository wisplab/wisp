#!/usr/bin/env bash
# Build OpenSSL libcrypto.a + libssl.a for wasm32-wasip1.
#
# WASI Preview 1 has no sockets and no threads. We build a strictly local-
# crypto OpenSSL: no networking primitives, no thread support, no engines,
# no DSO. CPython's _hashlib uses libcrypto only; _ssl uses libssl but its
# socket-touching code paths just won't be reached in our use cases.
#
# This is the heavy M0.5 dependency: ~15 minutes of compile time.
set -euo pipefail
cd "$(dirname "$0")"

OPENSSL_VERSION="${OPENSSL_VERSION:-3.4.0}"
ROOT="$(pwd)"
WASI_SDK="$ROOT/toolchain/wasi-sdk-32.0-arm64-macos"
VENDOR_DIR="$ROOT/vendor"
OPENSSL_DIR="$VENDOR_DIR/openssl-$OPENSSL_VERSION"

export HTTP_PROXY="${HTTP_PROXY:-http://127.0.0.1:7890}"
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:7890}"

mkdir -p "$VENDOR_DIR"

# ---- step 1: source --------------------------------------------------------
if [ ! -d "$OPENSSL_DIR" ]; then
  echo "==> Downloading OpenSSL $OPENSSL_VERSION..."
  url="https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz"
  curl -L --fail -o "$VENDOR_DIR/openssl.tar.gz" "$url"
  tar -xzf "$VENDOR_DIR/openssl.tar.gz" -C "$VENDOR_DIR"
  rm "$VENDOR_DIR/openssl.tar.gz"
fi

cd "$OPENSSL_DIR"

CC="$WASI_SDK/bin/clang"
AR="$WASI_SDK/bin/llvm-ar"
RANLIB="$WASI_SDK/bin/llvm-ranlib"
SYSROOT="$WASI_SDK/share/wasi-sysroot"

# WASI doesn't ship netdb.h. OpenSSL's BIO/sock layer #includes it even when
# the runtime never uses sockets. Provide an empty shim header on the include
# path so compilation proceeds. The unresolved socket symbols are fine: they
# won't be called from CPython's ssl/hashlib paths we exercise.
SHIM_DIR="$ROOT/vendor/wasi-shim-include"
mkdir -p "$SHIM_DIR"
cat > "$SHIM_DIR/netdb.h" <<'EOF'
/* Empty stub so OpenSSL #include <netdb.h> compiles on wasm32-wasip1.
   WASI Preview 1 does not provide a network stack; symbols left unresolved. */
EOF
cat > "$SHIM_DIR/syslog.h" <<'EOF'
/* Empty stub so OpenSSL apps/ #include <syslog.h> compiles on wasm32-wasip1.
   WASI does not have syslog. We don't link the openssl CLI binary. */
EOF
NETDB_SHIM_FLAGS="-I$SHIM_DIR"

# ---- step 2: configure -----------------------------------------------------
# We hijack linux-generic32 — the most portable target — and override its
# tools with WASI SDK. The disable list is conservative for WASI Preview 1:
# no DSO/shared (no dlopen), no engine (uses DSO), no async (needs ucontext),
# no threads (no pthread), no sock (no networking), no UI (no termios),
# no tests (would need test runner).
if [ ! -f Makefile ]; then
  echo "==> Configuring OpenSSL for wasm32-wasip1..."
  CC="$CC" \
  AR="$AR" \
  RANLIB="$RANLIB" \
  CROSS_COMPILE="" \
  ./Configure linux-generic32 \
    --prefix="$OPENSSL_DIR/install" \
    --openssldir="/etc/ssl" \
    no-shared \
    no-dso \
    no-engine \
    no-async \
    no-threads \
    no-tests \
    no-sock \
    no-ui-console \
    no-stdio \
    no-secure-memory \
    no-aria \
    no-bf \
    no-camellia \
    no-cast \
    no-comp \
    no-dgram \
    no-dtls \
    no-egd \
    no-fips \
    no-filenames \
    no-idea \
    no-mdc2 \
    no-md4 \
    no-ocb \
    no-quic \
    no-rc2 \
    no-rc4 \
    no-rc5 \
    no-rfc3779 \
    no-rmd160 \
    no-seed \
    no-siphash \
    no-siv \
    no-sm2 \
    no-sm3 \
    no-sm4 \
    no-srp \
    no-srtp \
    no-ssl3 \
    no-ssl3-method \
    no-tls1 \
    no-tls1-method \
    no-tls1_1 \
    no-tls1_1-method \
    no-uplink \
    no-weak-ssl-ciphers \
    no-zlib \
    --target=wasm32-wasip1 \
    --sysroot="$SYSROOT" \
    $NETDB_SHIM_FLAGS \
    -D_WASI_EMULATED_SIGNAL \
    -D_WASI_EMULATED_GETPID \
    -D_WASI_EMULATED_PROCESS_CLOCKS \
    -D_WASI_EMULATED_MMAN \
    -DOPENSSL_NO_SECURE_MEMORY \
    -DNO_SYSLOG
fi

# ---- step 3: generated headers, then libcrypto.a only --------------------
# WASI Preview 1 lacks getsockname etc., so OpenSSL's libssl can't link.
# We build only libcrypto.a — gives CPython _hashlib (faster SHA/HMAC/PBKDF2);
# _ssl module is left missing because no real sockets are available anyway.
echo "==> Generating headers..."
make -j8 build_generated 2>&1 | tail -2
echo "==> Building libcrypto.a only..."
make -j8 libcrypto.a 2>&1 | tail -5

echo
echo "Built artifacts:"
ls -la "$OPENSSL_DIR"/libcrypto.a "$OPENSSL_DIR"/libssl.a 2>&1

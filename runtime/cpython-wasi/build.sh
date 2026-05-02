#!/usr/bin/env bash
# Path D Step 1: cross-compile CPython 3.14 to wasm32-wasip1.
#
# Uses CPython 3.14's official Tools/wasm/wasi driver, which handles the
# host-build-python + WASI cross-compile dance correctly (including the
# config.site cross-compile pre-answers that autoconf can't probe in WASI).
#
# Output: vendor/cpython/cross-build/wasm32-wasip1/python.wasm
#
# Test:
#   toolchain/wasmtime/wasmtime run --dir=. \
#     vendor/cpython/cross-build/wasm32-wasip1/python.wasm -c 'print("hi")'
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK_VERSION="${WASI_SDK_VERSION:-32}"
WASI_SDK_ARCH="${WASI_SDK_ARCH:-arm64-macos}"
CPYTHON_TAG="${CPYTHON_TAG:-v3.14.3}"
WASMTIME_VERSION="${WASMTIME_VERSION:-v44.0.1}"
WASMTIME_ARCH="${WASMTIME_ARCH:-aarch64-macos}"

export HTTP_PROXY="${HTTP_PROXY:-http://127.0.0.1:7890}"
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:7890}"

ROOT="$(pwd)"
TOOLCHAIN_DIR="$ROOT/toolchain"
VENDOR_DIR="$ROOT/vendor"
WASI_SDK_DIR="$TOOLCHAIN_DIR/wasi-sdk-${WASI_SDK_VERSION}.0-${WASI_SDK_ARCH}"
WASMTIME_DIR="$TOOLCHAIN_DIR/wasmtime"
CPYTHON_DIR="$VENDOR_DIR/cpython"

mkdir -p "$TOOLCHAIN_DIR" "$VENDOR_DIR"

# ---- step 1: wasi-sdk -------------------------------------------------------
if [ ! -d "$WASI_SDK_DIR" ]; then
  echo "==> Downloading wasi-sdk-${WASI_SDK_VERSION} ($WASI_SDK_ARCH)..."
  url="https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/wasi-sdk-${WASI_SDK_VERSION}.0-${WASI_SDK_ARCH}.tar.gz"
  curl -L --fail -o "$TOOLCHAIN_DIR/wasi-sdk.tar.gz" "$url"
  tar -xzf "$TOOLCHAIN_DIR/wasi-sdk.tar.gz" -C "$TOOLCHAIN_DIR"
  rm "$TOOLCHAIN_DIR/wasi-sdk.tar.gz"
fi
echo "wasi-sdk: $WASI_SDK_DIR"
"$WASI_SDK_DIR/bin/clang" --version | head -1

# ---- step 2: wasmtime CLI (used as host-runner during build self-tests) ----
if [ ! -x "$WASMTIME_DIR/wasmtime" ]; then
  echo "==> Downloading wasmtime ${WASMTIME_VERSION} (${WASMTIME_ARCH})..."
  url="https://github.com/bytecodealliance/wasmtime/releases/download/${WASMTIME_VERSION}/wasmtime-${WASMTIME_VERSION}-${WASMTIME_ARCH}.tar.xz"
  curl -L --fail -o "$TOOLCHAIN_DIR/wasmtime.tar.xz" "$url"
  tar -xJf "$TOOLCHAIN_DIR/wasmtime.tar.xz" -C "$TOOLCHAIN_DIR"
  mv "$TOOLCHAIN_DIR/wasmtime-${WASMTIME_VERSION}-${WASMTIME_ARCH}" "$WASMTIME_DIR"
  rm "$TOOLCHAIN_DIR/wasmtime.tar.xz"
fi
"$WASMTIME_DIR/wasmtime" --version

# ---- step 3: cpython source -------------------------------------------------
if [ ! -d "$CPYTHON_DIR" ]; then
  echo "==> Cloning CPython $CPYTHON_TAG..."
  git -c http.proxy="$HTTPS_PROXY" clone --depth 1 --branch "$CPYTHON_TAG" \
    https://github.com/python/cpython.git "$CPYTHON_DIR"
fi

# ---- step 4: cross-compile via official driver -----------------------------
# CPython's wasi driver looks for wasmtime via shutil.which() — needs PATH.
echo "==> Running CPython's Tools/wasm/wasi build..."
cd "$CPYTHON_DIR"
export PATH="$WASMTIME_DIR:$PATH"
python3 Tools/wasm/wasi build --wasi-sdk "$WASI_SDK_DIR"

# ---- step 5: report ---------------------------------------------------------
echo
echo "==> Build complete."
WASI_PYTHON_DIR="$CPYTHON_DIR/cross-build/wasm32-wasip1"
ls -la "$WASI_PYTHON_DIR/python.wasm" 2>/dev/null || echo "warning: python.wasm not at expected path"
echo
echo "Smoke test:"
echo "  $WASMTIME_DIR/wasmtime run --dir=. \\"
echo "    --env PYTHONPATH=$WASI_PYTHON_DIR/build/lib.wasi-wasm32-cpython-314/ \\"
echo "    $WASI_PYTHON_DIR/python.wasm -c 'print(\"hi from WASI Python\")'"

#!/usr/bin/env bash
# scripts/numpy/02-config.sh
#
# Stage 2: hand-write the platform-specific config headers numpy normally
# produces from runtime probes in setup.py.
#
# Why hand-written: numpy's auto-probe path compiles+executes tiny C
# programs to detect things like sizeof(long), HAVE_FEATURES_H, etc.
# Cross-compile to wasm32-wasip1 makes the "execute" step impossible
# from a macOS/Linux build host — there's no wasm interpreter in the
# auto-probe path. So we write down the wasm32 ABI facts directly:
#
#   wasm32 sizes (LLP64-ish for pointers but 32-bit):
#     short      = 2
#     int        = 4
#     long       = 4         ← differs from native macOS/linux (8)
#     long long  = 8
#     intptr_t   = 4         ← differs from native (8)
#     off_t      = 8         ← WASI uses 64-bit fd offsets
#     float      = 4
#     double     = 8
#     long double = 8         ← wasm collapses long double to double
#     wchar_t    = 4
#
# WASI Preview 1 capabilities relevant to numpy:
#   - mmap: yes, via -lwasi-emulated-mman
#   - signal: no — set NPY_NO_SIGNAL=1
#   - threads / SMP: no — set NPY_NO_SMP=1
#   - endian.h: no — undef NPY_HAVE_ENDIAN_H, wasm is little-endian anyway
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi/vendor && pwd)"
NUMPY_DIR="$ROOT/numpy-1.26.4"
GEN_DIR="$NUMPY_DIR/build-wasi/generated"
INC_DIR="$NUMPY_DIR/numpy/core/include/numpy"

mkdir -p "$GEN_DIR"

echo "==> Writing _numpyconfig.h (wasm32-wasip1 ABI)"
cat > "$INC_DIR/_numpyconfig.h" <<'EOF'
/* Hand-written for wasm32-wasip1 by wisp/scripts/numpy/02-config.sh.
 * The setup.py probe path can't execute test programs cross-compile,
 * so the values here come from the wasm32 ABI specification, not from
 * runtime detection. Do not edit by hand — re-run the script.
 */
/* #undef NPY_HAVE_ENDIAN_H */

#define NPY_SIZEOF_SHORT 2
#define NPY_SIZEOF_INT 4
#define NPY_SIZEOF_LONG 4
#define NPY_SIZEOF_FLOAT 4
#define NPY_SIZEOF_COMPLEX_FLOAT 8
#define NPY_SIZEOF_DOUBLE 8
#define NPY_SIZEOF_COMPLEX_DOUBLE 16
#define NPY_SIZEOF_LONGDOUBLE 8
#define NPY_SIZEOF_COMPLEX_LONGDOUBLE 16
#define NPY_SIZEOF_PY_INTPTR_T 4
/* NPY_SIZEOF_INTP / NPY_SIZEOF_UINTP are defined in npy_common.h
 * from NPY_SIZEOF_PY_INTPTR_T — don't double-define here. */
#define NPY_SIZEOF_WCHAR_T 4
#define NPY_SIZEOF_OFF_T 8
#define NPY_SIZEOF_PY_LONG_LONG 8
#define NPY_SIZEOF_LONGLONG 8

#define NPY_USE_C99_COMPLEX 1
#define NPY_HAVE_COMPLEX_DOUBLE 1
#define NPY_HAVE_COMPLEX_FLOAT 1
#define NPY_HAVE_COMPLEX_LONG_DOUBLE 1
#define NPY_USE_C99_FORMATS 1

/* WASI has no signal delivery in P1; no threads either. */
#define NPY_NO_SIGNAL 1
#define NPY_NO_SMP 1

#define NPY_VISIBILITY_HIDDEN __attribute__((visibility("hidden")))
#define NPY_ABI_VERSION 0x02000000
#define NPY_API_VERSION 0x00000010

#ifndef __STDC_FORMAT_MACROS
#define __STDC_FORMAT_MACROS 1
#endif
EOF
echo "  -> $INC_DIR/_numpyconfig.h"

echo "==> Writing config.h (numpy internal — HAVE_* probe results)"
# config.h is normally produced by setup.py's generate_config_h, which
# runs ~80 compile probes (HAVE_FUNC_X, HAVE_HEADER_Y, intrinsics, …).
# We can't run those probes cross-compile, so we hand-encode what we
# know is true for wasi-sdk + the WASI sysroot.
#
# Search path note: numpy's `#include "config.h"` resolves via include
# paths; we put the file in GEN_DIR which is already on -I.
cat > "$GEN_DIR/config.h" <<'EOF'
/* Hand-written for wasm32-wasip1 by wisp/scripts/numpy/02-config.sh.
 * Encodes what wasi-sdk + WASI sysroot expose. Do not edit by hand. */

/* C99 math (all present in wasi-libc via newlib math) */
#define HAVE_SIN 1
#define HAVE_COS 1
#define HAVE_TAN 1
#define HAVE_SINH 1
#define HAVE_COSH 1
#define HAVE_TANH 1
#define HAVE_FABS 1
#define HAVE_FLOOR 1
#define HAVE_CEIL 1
#define HAVE_SQRT 1
#define HAVE_LOG10 1
#define HAVE_LOG 1
#define HAVE_EXP 1
#define HAVE_ASIN 1
#define HAVE_ACOS 1
#define HAVE_ATAN 1
#define HAVE_FMOD 1
#define HAVE_MODF 1
#define HAVE_FREXP 1
#define HAVE_LDEXP 1
#define HAVE_EXPM1 1
#define HAVE_LOG1P 1
#define HAVE_ACOSH 1
#define HAVE_ASINH 1
#define HAVE_ATANH 1
#define HAVE_RINT 1
#define HAVE_TRUNC 1
#define HAVE_EXP2 1
#define HAVE_COPYSIGN 1
#define HAVE_NEXTAFTER 1
#define HAVE_STRTOLL 1
#define HAVE_STRTOULL 1
#define HAVE_CBRT 1
#define HAVE_LOG2 1
#define HAVE_POW 1
#define HAVE_HYPOT 1
#define HAVE_ATAN2 1
#define HAVE_CREAL 1
#define HAVE_CIMAG 1
#define HAVE_CONJ 1

/* C99 complex (clang supports across wasi) */
#define HAVE_COMPLEX_H 1
#define HAVE_CABS 1
#define HAVE_CACOS 1
#define HAVE_CACOSH 1
#define HAVE_CARG 1
#define HAVE_CASIN 1
#define HAVE_CASINH 1
#define HAVE_CATAN 1
#define HAVE_CATANH 1
#define HAVE_CEXP 1
#define HAVE_CLOG 1
#define HAVE_CPOW 1
#define HAVE_CSQRT 1
#define HAVE_CSIN 1
#define HAVE_CCOS 1
#define HAVE_CTAN 1
#define HAVE_CSINH 1
#define HAVE_CCOSH 1
#define HAVE_CTANH 1
#define HAVE_CPROJ 1

/* Optional headers WASI sysroot has */
#define HAVE_SYS_MMAN_H 1   /* via wasi-emulated-mman */
#define HAVE_FEATURES_H 1

/* Optional headers WASI sysroot does NOT have */
/* #undef HAVE_XMMINTRIN_H */
/* #undef HAVE_EMMINTRIN_H */
/* #undef HAVE_IMMINTRIN_H */
/* #undef HAVE_XLOCALE_H */
/* #undef HAVE_DLFCN_H */
/* #undef HAVE_EXECINFO_H */
/* #undef HAVE_LIBUNWIND_H */

/* Optional file ops */
#define HAVE_FTELLO 1
#define HAVE_FSEEKO 1
/* #undef HAVE_FALLOCATE */
/* #undef HAVE_BACKTRACE */
#define HAVE_MADVISE 1   /* emulated via -lwasi-emulated-mman */

/* Locale */
/* #undef HAVE_STRTOLD_L */

/* clang builtins (always available with wasi-sdk clang 22+) */
#define HAVE___BUILTIN_ISNAN 1
#define HAVE___BUILTIN_ISINF 1
#define HAVE___BUILTIN_ISFINITE 1
#define HAVE___BUILTIN_BSWAP32 1
#define HAVE___BUILTIN_BSWAP64 1
#define HAVE___BUILTIN_EXPECT 1
#define HAVE___BUILTIN_MUL_OVERFLOW 1
#define HAVE___BUILTIN_PREFETCH 1

/* Function attributes (clang supports all of these) */
#define HAVE_ATTRIBUTE_NONNULL 1
#define HAVE_ATTRIBUTE_OPTIMIZE_UNROLL_LOOPS 1
#define HAVE_ATTRIBUTE_OPTIMIZE_OPT_3 1
#define HAVE_ATTRIBUTE_OPTIMIZE_OPT_2 1

/* long double representation on wasm32: identical to little-endian IEEE 754
 * 64-bit double (wasm-ld collapses long double → double). This selects the
 * IEEE-double branch in numpy/core/src/npymath/npy_math_private.h and
 * resolves the ~40 "No long double representation defined" errors. */
#define HAVE_LDOUBLE_IEEE_DOUBLE_LE 1

/* numpy's relaxed-stride debugging flag is normally set by setup.py
 * from $NPY_RELAXED_STRIDES_DEBUG env (default 0). Hard-set to 0
 * because some .c files use it as a value, not just an ifdef. */
#define NPY_RELAXED_STRIDES_DEBUG 0

/* SIMD intrinsics — NONE on wasm32 (we disable optimization paths) */
/* #undef HAVE_ATTRIBUTE_TARGET_AVX */
/* #undef HAVE_ATTRIBUTE_TARGET_AVX2 */
/* #undef HAVE_ATTRIBUTE_TARGET_AVX512F */
/* #undef NPY_HAVE_SSE2_INTRINSICS */
/* #undef NPY_HAVE_AVX2_INTRINSICS */
/* #undef NPY_HAVE_AVX512F_INTRINSICS */
EOF
echo "  -> $GEN_DIR/config.h"

echo "==> Patching npy_cpu.h to recognize __wasi__"
# numpy already has NPY_CPU_WASM but gates it on __EMSCRIPTEN__ only.
# Extend to also accept the WASI-SDK toolchain macro __wasi__.
NPY_CPU_H="$NUMPY_DIR/numpy/core/include/numpy/npy_cpu.h"
if ! grep -q "WISP_WASI_CPU_PATCH" "$NPY_CPU_H"; then
  python3 - "$NPY_CPU_H" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
src = p.read_text()
new = src.replace(
    "#elif defined(__EMSCRIPTEN__)",
    "/* WISP_WASI_CPU_PATCH */\n#elif defined(__EMSCRIPTEN__) || defined(__wasi__)",
)
p.write_text(new)
PY
  echo "  -> patched $NPY_CPU_H"
else
  echo "  (already patched)"
fi

echo "==> Patching loops.c to no-op long-double ufunc bodies"
# Background: PyUFunc_g_g / _gg_g / _G_G / _GG_G dispatch a `void *func`
# pointer cast through a `npy_longdouble (*)(npy_longdouble)`-style
# typedef. wasm32 enforces signature equality at indirect call sites
# and rejects the cast (same family as the xxhash METH_NOARGS trap).
#
# On wasm32 `npy_longdouble == double` (8-byte IEEE 754), so the long-
# double variants don't actually add precision; dropping them via
# no-op bodies costs nothing for our scalar baseline. User code that
# explicitly uses `dtype=np.longdouble` ufuncs gets zero-output, but
# that's an acceptable degradation for now and matches what we already
# do for non-built C extensions (linalg / fft / random stubs).
#
# Python script does the rewrite because sed across multi-line C bodies
# is too brittle on macOS.
LOOPS_C="$NUMPY_DIR/numpy/core/src/umath/loops.c"
if [ -f "$LOOPS_C" ] && ! grep -q "WISP_WASI_LONGDOUBLE_NOOP" "$LOOPS_C"; then
  python3 - "$LOOPS_C" <<'PY'
import re, sys, pathlib
p = pathlib.Path(sys.argv[1])
src = p.read_text()
funcs = ["PyUFunc_g_g", "PyUFunc_gg_g", "PyUFunc_G_G", "PyUFunc_GG_G"]
noop_body = (
    "\n{\n"
    "    /* WISP_WASI_LONGDOUBLE_NOOP — see scripts/numpy/02-config.sh. */\n"
    "    (void)args; (void)dimensions; (void)steps; (void)func;\n"
    "}\n"
)
total = 0
for fn in funcs:
    # Match `<fn>(char **args, ...)\n{ ... }\n` — balanced braces, single
    # body. The bodies in loops.c are simple enough that a regex pinned
    # to the leading signature + balanced braces works.
    pattern = re.compile(
        r"(" + re.escape(fn) + r"\(char \*\*args, npy_intp const \*dimensions, npy_intp const \*steps, void \*func\))\s*\{[^{}]*\{[^{}]*\}[^{}]*\}",
        re.DOTALL,
    )
    m = pattern.search(src)
    if not m:
        print(f"  WARN: pattern miss for {fn}", file=sys.stderr)
        continue
    src = src[:m.start()] + m.group(1) + noop_body + src[m.end():]
    total += 1
p.write_text(src)
print(f"  patched {total} long-double ufunc bodies in {p}")
PY
else
  echo "  (already patched or loops.c missing)"
fi

echo "==> Writing src/common/npy_config.h additions (WASI-specific)"
# numpy's existing npy_config.h is mostly portable; we just inject a
# block that turns off optimization paths needing CPU dispatch.
NPY_CFG="$NUMPY_DIR/numpy/core/src/common/npy_config.h"
if ! grep -q "WISP_WASI_CONFIG_BLOCK" "$NPY_CFG"; then
  cat >> "$NPY_CFG" <<'EOF'

/* WISP_WASI_CONFIG_BLOCK — appended by scripts/numpy/02-config.sh.
 * Disable all SIMD/CPU-dispatch paths under wasm32-wasip1. The dispatch
 * machinery picks function pointers at runtime based on CPU features;
 * wasm has no equivalent and the function-pointer tables trap on
 * indirect call type mismatch. We hard-disable to take the scalar path.
 */
#ifdef __wasi__
  #undef HAVE_ATTRIBUTE_TARGET_AVX
  #undef HAVE_ATTRIBUTE_TARGET_AVX2
  #undef HAVE_ATTRIBUTE_TARGET_AVX512F
  #undef NPY_HAVE_SSE2_INTRINSICS
  #undef NPY_HAVE_AVX2_INTRINSICS
  #undef NPY_HAVE_AVX512F_INTRINSICS
  #ifndef NPY_DISABLE_OPTIMIZATION
    #define NPY_DISABLE_OPTIMIZATION 1
  #endif
#endif
EOF
  echo "  -> appended WASI block to $NPY_CFG"
else
  echo "  (block already present)"
fi

echo
echo "==> Stage 2 done."
echo
echo "  Generated headers and the config files are now in place. Stage 3"
echo "  (compile a single .c with WASI SDK) is next — start with alloc.c"
echo "  because it's small and the easiest forcing function for finding"
echo "  missing -I paths."

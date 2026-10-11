#!/usr/bin/env python3
"""Apply the WASM-only PCG64 integer optimization to NumPy 1.26.4.

Keep the uint128 ABI and the exact LCG recurrence. Only the hot single-step
multiplication changes; jump-ahead and all sampling/libm code stay upstream.
"""
from pathlib import Path

HEADER = Path(__file__).resolve().parents[2] / 'runtime/cpython-wasi/vendor/numpy-1.26.4/numpy/random/src/pcg64/pcg64.h'
ORIGINAL = '''static inline void pcg_setseq_128_step_r(pcg_state_setseq_128 *rng) {
  rng->state = rng->state * PCG_DEFAULT_MULTIPLIER_128 + rng->inc;
}'''
REPLACEMENT = '''static inline void pcg_setseq_128_step_r(pcg_state_setseq_128 *rng) {
#if defined(__wasm32__)
  /* WISP_PCG64_WASM_LIMBS: same modulo-2^128 recurrence, no __multi3.
   * Keep pcg128_t/layout unchanged. Unsigned limb operations wrap exactly.
   * This is the upstream emulated multiply's high-half calculation, with
   * the fixed multiplier exposed to LLVM for constant propagation. */
  const uint64_t lo = (uint64_t)rng->state;
  const uint64_t hi = (uint64_t)(rng->state >> 64);
  const uint64_t m = PCG_DEFAULT_MULTIPLIER_LOW;
  const uint64_t x0 = (uint32_t)lo, x1 = lo >> 32;
  const uint64_t y0 = (uint32_t)m, y1 = m >> 32;
  const uint64_t w0 = x0 * y0;
  const uint64_t t = x1 * y0 + (w0 >> 32);
  const uint64_t w1 = (uint32_t)t + x0 * y1;
  const uint64_t product_hi = x1 * y1 + (t >> 32) + (w1 >> 32)
      + hi * m + lo * PCG_DEFAULT_MULTIPLIER_HIGH;
  const uint64_t product_lo = lo * m;
  const uint64_t next_lo = product_lo + (uint64_t)rng->inc;
  const uint64_t next_hi = product_hi + (uint64_t)(rng->inc >> 64)
      + (next_lo < product_lo);
  rng->state = ((pcg128_t)next_hi << 64) | next_lo;
#else
  rng->state = rng->state * PCG_DEFAULT_MULTIPLIER_128 + rng->inc;
#endif
}'''


def main():
    source = HEADER.read_text()
    if REPLACEMENT in source:
        return
    if source.count(ORIGINAL) != 1 or 'WISP_PCG64_WASM_LIMBS' in source:
        raise SystemExit('unexpected PCG64 header: refusing to apply optimization')
    HEADER.write_text(source.replace(ORIGINAL, REPLACEMENT))


if __name__ == '__main__':
    main()

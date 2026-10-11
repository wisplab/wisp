#!/usr/bin/env python3
"""Check the WASM limb recurrence against native unsigned uint128 arithmetic."""
import pathlib
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[2]
header = root / 'runtime/cpython-wasi/vendor/numpy-1.26.4/numpy/random/src/pcg64'
if 'WISP_PCG64_WASM_LIMBS' not in (header / 'pcg64.h').read_text():
    raise SystemExit('apply patch-pcg64.py before testing the WASM recurrence')
code = r'''
#include "pcg64.h"
#include <assert.h>
#include <stdio.h>
static uint64_t sample(uint64_t *s) {
  *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; return *s;
}
int main(void) {
  uint64_t s = 7;
  const uint64_t edges[] = {0, 1, 0xffffffff, 0x100000000,
                            0x7fffffffffffffff, 0xffffffffffffffff};
  for (int a=0; a<6; a++) for (int b=0; b<6; b++)
  for (int c=0; c<6; c++) for (int d=0; d<6; d++) {
    pcg_state_setseq_128 r = {((pcg128_t)edges[a]<<64)|edges[b],
                             ((pcg128_t)edges[c]<<64)|edges[d]};
    pcg128_t expected = r.state * PCG_DEFAULT_MULTIPLIER_128 + r.inc;
    pcg_setseq_128_step_r(&r); assert(r.state == expected);
  }
  for (int i=0; i<1000000; i++) {
    uint64_t hi=sample(&s), lo=sample(&s), ih=sample(&s), il=sample(&s);
    pcg_state_setseq_128 r = {((pcg128_t)hi<<64)|lo, ((pcg128_t)ih<<64)|il};
    pcg128_t expected = r.state * PCG_DEFAULT_MULTIPLIER_128 + r.inc;
    pcg_setseq_128_step_r(&r); assert(r.state == expected);
  }
  puts("PASS: 1296 edge combinations and 1000000 integer states");
}
'''
with tempfile.TemporaryDirectory(prefix='wisp-pcg64-test-') as tmp:
    source = pathlib.Path(tmp) / 'check.c'
    source.write_text(code)
    binary = pathlib.Path(tmp) / 'check'
    subprocess.run(['clang', '-std=c99', '-O2', '-D__wasm32__', '-I', str(header), str(source), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)

# Sequence-preserving PCG64 optimization, 2026-10-11

This experiment changes only the WASM PCG64 single-step integer recurrence.
The Rust daemon, normal sampler, price expressions, branch implementation,
capabilities, and Native Seatbelt setup are unchanged from `27966be6`.

## Mechanism

NumPy 1.26.4's PCG64 stores its state and increment as unsigned 128-bit
integers. The WASI compiler emitted a call to compiler-rt `__multi3` on
every `next_uint64`, `next_uint32` cache miss, and `next_double`. The call
also used a temporary stack slot for the 128-bit result. This was observed
in the disassembly of `_random__pcg64.c.o`, not inferred from wall time.

The WASM-only patch exposes the fixed multiplier and calculates the high
half of the product using 32-bit limbs in unsigned 64-bit operations. It
performs the same modulo-2^128 multiply/add, explicitly retaining the carry
from the low-half addition. This follows the upstream emulated-multiply
formula, while retaining the original `pcg128_t` ABI and memory layout.
The output permutation, multiplier, increment, seeding, cached 32-bit
values and draw order retain their original semantics. Jump-ahead arithmetic
and other generators retain their upstream code.

The new PCG64 callbacks contain `i64.mul`/add/shift operations with no
`__multi3` call or temporary stack slot. No host import or shared RNG state
was added. Ziggurat tables, rejection and tail handling, `log1p`/`exp`,
floating-point compiler flags and NumPy's public API are unchanged.

`scripts/numpy/patch-pcg64.py` applies the tracked, idempotent patch and
rejects an unexpected header. Both full builds and `--rng-only` builds
apply it; the latter recompiles only `_pcg64.c` and `src/pcg64/pcg64.c`.
Archive and relink steps are documented in `scripts/numpy/README.md`.

## Exactness and isolation

`scripts/numpy/test-pcg64.py` compares the patched recurrence against native
unsigned 128-bit arithmetic for **1,296 boundary combinations and 1,000,000
deterministic arbitrary states/increments**, including carry and wraparound.

`verify-rng.py` starts the saved pre-change reactor and then the candidate
reactor sequentially, using the same release daemon. **85 byte-digest/state
records match exactly** in each of four candidate configurations:
Mach/file snapshots × acceleration enabled/disabled. Cases include four
seed forms, raw integers, float64/float32 normals, uniforms, bounded integers,
exponentials, gamma, scalar draws, zero-length draws, chunked draws, restoring
state, large advance and jumped generators, adversarial state/increment
boundaries, and a cached half-integer. A full 2M-normal seed-7 stream matches,
including the complete ending state. Other generators provide controls.

Each configuration also runs two batches of eight fresh branches inheriting
the same partly consumed generator. Every child produces the expected bytes
and final state despite independent further advancement; the parent keeps
its original state and subsequently produces the expected stream itself.
This tests isolation of mutable RNG state, not just numerical PnL agreement.
Normal tails and rejection paths are exercised by the large streams.

Raw verification: `results/rng-verification-20261011.json`.

The existing complete `verify.py` integration suite also passes in all four
Mach/file × accel on/off configurations after the rebuild: 40 math cases,
branch ordering/errors/recovery, bounded output, WASI open-handle rejection,
and concurrent parent/child writes and independent memory growth. Python
syntax checks, shell syntax checks and `git diff --check` pass.

## Measurements

The complete comparison uses `isolated.py` with the original seed-7 price
workload, 2M bars, 40 parameters, eight fresh concurrent children, one full
discarded warmup and nine measured shuffled rounds. Native still installs
Seatbelt per child; all capability-denial probes and result comparisons must
pass. Only one Wisp service is resident. Bootstrap is excluded on both sides.
Explicit `--wisp-reactor` selection and recorded SHA-256 hashes distinguish
the pre/post guest builds while keeping the Rust daemon fixed.

`profile-rng.py` is a separate diagnostic. Three randomized sequential
pre/post service blocks each contain one discarded warmup and nine measured
draws per method. Timing includes output allocation, but excludes hashing,
HTTP and bootstrap. It checks bytes and ending states in every sample. These
microbenchmarks cannot replace the complete isolated comparison.

### RNG-only results

All timings below are milliseconds for 2M draws, 27 samples per method/version.

| Method | Before median (IQR) | After median (IQR) | Median time reduction |
|---|---:|---:|---:|
| raw | 51.39 (48.20–65.49) | 40.80 (39.00–48.34) | 20.6% |
| normal64 | 70.79 (69.32–87.83) | 63.96 (61.88–76.21) | 9.6% |
| normal32 | 65.92 (60.73–75.86) | 58.75 (56.03–70.66) | 10.9% |
| uniform | 64.23 (58.21–76.57) | 51.94 (50.54–62.53) | 19.1% |

For float64 normals, the three block-median candidate/baseline ratios are
0.786, 0.910 and 0.888: each block improves, and the median block reduction
is **11.2%**. The pooled-sample median reduction is **9.6%**. Neither figure
is a fixed speed guarantee; block order and machine load still affect timing.

Raw data: `results/rng-profile-20261011.json`.

### Complete isolated benchmark

These full-sweep runs encountered large system-wide variation. Native
controls changed substantially too. Every run is retained; there is no
claim of a statistically established end-to-end speedup from these batches.

| Batch | Wisp parent median | Wisp branch median | Wisp total median (IQR) | Isolated Native total median (IQR) | Wisp/Native ratio of medians |
|---|---:|---:|---:|---:|---:|
| Before, initial | 149.6 | 570.1 | 784.9 (648.0–805.9) | 528.0 (460.5–685.1) | 1.487 |
| After | 188.5 | 1360.5 | 1549.8 (1149.5–1909.6) | 1349.0 (1190.9–1546.6) | 1.149 |
| Before, later control | 204.3 | 1116.7 | 1302.1 (1227.2–1514.0) | 1160.5 (1109.2–1217.0) | 1.122 |

The after batch is **1.55 s** versus **1.35 s** for isolated Native
(ratio 1.149); the median same-round ratio is also 1.149, with an IQR of
1.004–1.381. The later old-reactor control is 1.30 s versus 1.16 s
(ratio of medians 1.122); its median same-round ratio is 1.235. Thus the
absolute before/after numbers and the two ratio statistics do not support
a clean whole-task speedup claim. The initial old batch even contains
multi-second outliers, including a 14.36-s Wisp round. Global swap-out
counters also change; they cannot attribute the slowdown to one backend.

Do not compare the earlier quiet 407-ms result directly with these loaded
runs. The positive finding here is the repeated RNG-only improvement with
unchanged bytes/state. RNG remains slower than native; callbacks and the
sampler remain in WASM. No numerical approximation or host RNG was added.

All three full batches pass the Native and Wisp isolation probes, and all
40 PnLs match within 7.78e-15. Wisp PnL values are **exactly identical across
every round and both reactors**, beyond the cross-backend tolerance. The
daemon hashes are identical; only the reactor hashes differ.

Raw complete data:

- `results/rng-before-20261011.json`
- `results/rng-after-20261011.json`
- `results/rng-before-control-20261011.json`

## Reproduce

Before applying/rebuilding the patch, save the baseline reactor:

```sh
cp runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm /tmp/before-rng.wasm
bash scripts/numpy/04-compile-all.sh --rng-only
bash scripts/numpy/05-archive.sh
bash runtime/cpython-wasi/wisp_entry/build.sh
python3 scripts/numpy/test-pcg64.py
python3 bench/branch-sweep/verify-rng.py --reference-reactor /tmp/before-rng.wasm \
  --output bench/branch-sweep/results/rng-verification.json
python3 bench/branch-sweep/profile-rng.py --reference-reactor /tmp/before-rng.wasm \
  --output bench/branch-sweep/results/rng-profile.json
python3 bench/branch-sweep/isolated.py --repeats 9 --wisp-reactor /tmp/before-rng.wasm \
  --output bench/branch-sweep/results/rng-before.json
python3 bench/branch-sweep/isolated.py --repeats 9 \
  --output bench/branch-sweep/results/rng-after.json
```

The saved baseline in this experiment is `/tmp/wisp-rng-before-20261011/python-reactor.wasm`.
It predates the integer optimization and already includes the preceding COW
and prepared-import improvements. Results record the pre-change Git HEAD
plus actual daemon/reactor hashes because measurements precede the new commit.

Native uses NumPy 2.4.2 and Wisp uses 1.26.4. This experiment proves equality
to the preceding Wisp stream; it does not establish arbitrary cross-version
NumPy stream compatibility or equivalent security mechanisms. Neither
implementation adds new CPU/memory quotas in this change.

# Compute-penalty attribution (`k = wasm / native`)

The per-call latency numbers in [`../results.md`](../results.md) answer
"how fast is a cold sandbox?" This suite answers a different question
that decides whether the WASM path is viable for **compute-bound**
agent workloads (the quant-backtest fan-out use case): *once you're
inside the sandbox, how much slower is the actual math than native?*

`k` is the ratio `wasm_seconds / native_seconds`, timed **in-process**
with `perf_counter` inside each environment, so startup / HTTP / COW
overhead is excluded. `k = 1` means wasm matches native; `k = 10` means
ten times slower.

## Reproduce

```sh
cargo build --release
./target/release/wisp-runtime &          # daemon on :9000
LABEL=accel bench/compute-penalty/run.sh # native + in-guest, prints k
```

`kernels.py` runs identically host-side and in-guest. Results land in
`results/$LABEL-{native,wasm}.json`.

## Results (M4 Pro, arm64)

Native baseline: host CPython 3.11 + numpy 2.4 (Accelerate).
Guest: CPython 3.14 + numpy 1.26 on wasm32-wasip1.

| kernel | bottleneck | baseline `-O2` | `-O3 -msimd128` | + wisp-accel |
|---|---|---:|---:|---:|
| `membw_elementwise` | memory bandwidth | 0.6× | 0.8× | **0.7×** |
| `vec_math` | transcendentals | 1.0× | 1.1× | **1.0×** |
| `backtest_vectorized` | numpy vector ops | 0.7× | 0.8× | **0.7×** |
| `blas_matmul_400` | BLAS dgemm | 151.6× | 150.6× | **1.0×** |
| `blas_solve_300` | LAPACK LU (dgesv) | 10.8× | 6.3× | **6.2×** |
| `python_loop_100k` | interpreter | 7.8× | 8.0× | **7.9×** |

`k < 1` (wasm faster than native) on the memory-bound and
vectorized-numpy kernels is real, not noise: those are dominated by RAM
bandwidth (identical on both) plus per-call Python/numpy dispatch
overhead, and the host numpy 2.4 build carries slightly more of that
overhead than our stripped 1.26 guest. The point isn't that wasm is
faster — it's that **for the vectorized backtest kernels that make up
the fan-out workload, there is no compute penalty.**

## What each tier bought

### Local reproduction, 2026-10-11

The reactor was re-linked from the existing CPython/NumPy static
archives and the Rust release daemon rebuilt. Both `WISP_ACCEL=1`
and `WISP_ACCEL=0` passed 40 NumPy correctness cases in stateless,
session and branched execution (float32/64, complex64/128, C/Fortran
layouts, transposes, slices, negative strides, vector products).

Current host baseline is CPython **3.14.3**, NumPy **2.4.2**;
guest is CPython **3.14.3**, NumPy **1.26.4**. Stable-run results:

| kernel | native | accel enabled | accel disabled |
|---|---:|---:|---:|
| membw_elementwise | 5.13 ms | 8.04 ms | 8.00 ms |
| vec_math | 11.05 ms | 16.70 ms | 16.98 ms |
| backtest_vectorized | 9.99 ms | 11.36 ms | 11.69 ms |
| blas_matmul_400 | 1.13 ms | 0.45 ms | 48.94 ms |
| blas_solve_300 | 0.68 ms | 2.27 ms | 2.24 ms |
| python_loop_100k | 4.83 ms | 24.37 ms | 24.50 ms |

Native values above are from the accel-enabled stable run; the
disabled run has its own native reference in the raw JSON. These are
individual runs on a busy laptop, not statistical guarantees.
An earlier run was substantially slower on both host and guest;
its raw data is retained, but is not used for the table.

Raw results: `results/repro-20261011-accel-steady-{native,wasm}.json`
and `results/repro-20261011-portable-{native,wasm}.json`.
The host offload improvement reproduces, but the historical ratios
and the blanket claim of no vectorized compute penalty do not:
these current vector kernels range from about 1.1× to 1.6× native.
This is not a full clean rebuild of the CPython/NumPy distribution.
Post-fix runtime validation also passed, with fresh raw data in
`results/fixed-20261011-accel-{native,wasm}.json` (matmul 0.35 ms,
vectorized backtest 12.09 ms). Earlier numbers above are preserved
as a separate run rather than mixed into the same sample.

Reproduce (daemon must already be running):

```sh
LABEL=repro-new-accel bash bench/compute-penalty/run.sh
# Restart the daemon with WISP_ACCEL=0, then:
LABEL=repro-new-portable bash bench/compute-penalty/run.sh
```

- **SIMD (`-O3 -msimd128 -mrelaxed-simd`)** — halved the LU-solve
  penalty (10.8× → 6.3×). Little effect on the already-memory-bound
  kernels (they don't have spare compute to vectorize), and none on
  matmul (that gap is BLAS, not codegen).
- **wisp-accel (host BLAS offload)** — collapsed matmul 151× → 1.0×.
  numpy is built `-DHAVE_CBLAS`; its `cblas_dgemm` is provided in-module
  by `wisp_accel.c`, which forwards to the host's `wisp_dgemm` import.
  The host runs the multiply zero-copy on the instance's linear memory
  (macOS → Accelerate/AMX; elsewhere → the `matrixmultiply` crate).
  lapack_lite's internal `dgemm_` is `--wrap`'d onto the same path.

## Known residual penalties (honest accounting)

- **`blas_solve_300` stays ~6×.** `np.linalg.solve` is LU factorization
  (`dgesv`), whose cost is the scalar panel factorization, not the
  `dgemm` trailing-matrix update we offload. Fixing it means
  host-offloading `dgesv` itself — a larger surface, deferred. For the
  backtest workload solve is rare, so this is low priority.
- **`python_loop_100k` stays ~8× and is unfixable at the compile
  layer.** A pure-Python event loop pays CPython-in-wasm interpreter
  overhead. The answer is "vectorize it or route it to the native
  fork-pool," not a build flag. This is the signal the smart router
  needs: interpreter-bound + long ⇒ native path.

## Host-offload switch

Set `WISP_ACCEL=0` to disable host offload: the guest computes BLAS
calls in wasm instead. Native BLAS can differ in last-bit rounding
per platform. Disabling host offload alone does not establish
cross-host bit reproducibility: the default build also uses relaxed
SIMD, whose floating-point behavior can differ between hosts. A
deterministic build must address those compiler/runtime settings
as well and be checked on the target platforms.

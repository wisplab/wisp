# bench/compute-penalty/kernels.py
#
# Compute-penalty attribution suite: the SAME code runs (a) on host
# CPython + native numpy and (b) inside the wisp guest via /v1/eval.
# Kernels are timed in-process with time.perf_counter so startup /
# HTTP overhead is excluded — this measures pure compute, i.e. the
# per-core efficiency deficit "k" of the wasm build, by category:
#
#   membw_elementwise   memory-bandwidth-bound vector op   (expect k≈1)
#   vec_math            transcendental-heavy, vectorizable (SIMD-sensitive)
#   backtest_vectorized realistic MA-crossover backtest kernel on 1M bars
#   blas_matmul_400     BLAS dgemm path (lapack_lite / naive fallback)
#   blas_solve_300      BLAS dgesv path (lapack_lite)
#   python_loop_100k    pure-Python event loop (interpreter-bound)
#
# Output: one line `WISP_BENCH_RESULT {json}` — parsed by run.sh.
import json
import sys
import time

import numpy as np


def timeit(fn, reps=3):
    """Best-of-reps wall time in seconds."""
    best = float("inf")
    for _ in range(reps):
        t0 = time.perf_counter()
        fn()
        dt = time.perf_counter() - t0
        best = min(best, dt)
    return best


N = 1_000_000
rng = np.random.default_rng(42)
rets = rng.standard_normal(N) * 0.001
px = 100.0 * np.exp(np.cumsum(rets))
a = rng.standard_normal(N)
b = rng.standard_normal(N)

results = {}


def k_membw():
    for _ in range(10):
        c = a * 2.0 + b
    return c


results["membw_elementwise"] = timeit(k_membw)


def k_vecmath():
    for _ in range(3):
        c = np.sqrt(np.abs(a)) * np.exp(-np.abs(b))
    return c


results["vec_math"] = timeit(k_vecmath)


def k_backtest():
    w_fast, w_slow = 20, 100
    cs = np.cumsum(px)
    ma_fast = (cs[w_fast:] - cs[:-w_fast]) / w_fast
    ma_slow = (cs[w_slow:] - cs[:-w_slow]) / w_slow
    n = min(len(ma_fast), len(ma_slow))
    sig = (ma_fast[-n:] > ma_slow[-n:]).astype(np.float64)
    r = np.diff(px[-n - 1:]) / px[-n - 1:-1]
    pnl = sig[:-1] * r[1:]
    equity = np.cumprod(1.0 + pnl)
    return equity[-1]


results["backtest_vectorized"] = timeit(k_backtest)

M = rng.standard_normal((400, 400))


def k_matmul():
    return M @ M


results["blas_matmul_400"] = timeit(k_matmul)

A300 = rng.standard_normal((300, 300)) + np.eye(300) * 300.0
b300 = rng.standard_normal(300)


def k_solve():
    return np.linalg.solve(A300, b300)


results["blas_solve_300"] = timeit(k_solve)

px_list = px[:100_000].tolist()


def k_pyloop():
    pos = 0
    cash = 0.0
    prev = px_list[0]
    for p in px_list:
        if p > prev and pos == 0:
            pos = 1
            cash -= p
        elif p < prev and pos == 1:
            pos = 0
            cash += p
        prev = p
    return cash


results["python_loop_100k"] = timeit(k_pyloop)

meta = {
    "python": sys.version.split()[0],
    "numpy": np.__version__,
    "platform": sys.platform,
}
print("WISP_BENCH_RESULT " + json.dumps({"meta": meta, "seconds": results}))

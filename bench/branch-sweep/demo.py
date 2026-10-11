#!/usr/bin/env python3
"""Precompute-in-parent → fork-per-parameter backtest sweep.

The flagship demo for Wisp's session-branch primitive. It shows the
programming model that Modal / E2B can't express: load the dataset and
compute the shared features ONCE in a parent session, then fork one COW
child per parameter combination. Every child physically shares the
parent's ~pre-branch pages (numpy, the price series, the cumulative
sums); each child pays memory only for what it dirties.

Compared against the same sweep run with multiprocessing on host
CPython — same machine, same core count.

Usage:
  ./target/release/wisp-runtime &      # daemon on :9000
  python3 bench/branch-sweep/demo.py [--bars N] [--grid N]
"""
import argparse
import itertools
import json
import os
import time
import urllib.request

DAEMON = os.environ.get("WISP_DAEMON_URL", "http://127.0.0.1:9000").rstrip("/")


def checked_eval(path, code):
    result = post(path, {"code": code})
    if result["rc"] != 0:
        raise RuntimeError(f"eval failed: {result.get('stderr', '')}")
    return result


def post(path, body, timeout=600):
    req = urllib.request.Request(
        DAEMON + path,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req, timeout=timeout))


def make_grid(n):
    fasts = [5, 10, 15, 20, 25, 30, 40, 50]
    slows = [60, 80, 100, 120, 150, 200, 250, 300]
    combos = [(f, s) for f, s in itertools.product(fasts, slows) if f < s]
    return combos[:n]


def child_code(fast, slow):
    # References cs / px from the parent's pre-branch state.
    return f"""
ma_f = (cs[{fast}:] - cs[:-{fast}]) / {fast}
ma_s = (cs[{slow}:] - cs[:-{slow}]) / {slow}
n = min(len(ma_f), len(ma_s))
sig = (ma_f[-n:] > ma_s[-n:]).astype(float)
r_ = px[-n:][1:] / px[-n:][:-1] - 1.0
pnl = float((sig[:-1] * r_).sum())
print({fast}, {slow}, round(pnl, 6))
"""


def run_wisp(bars, grid):
    sid = post("/v1/session", {})["session_id"]
    try:
        t0 = time.time()
        checked_eval(f"/v1/session/{sid}/eval", f"""
import numpy as np
rng = np.random.default_rng(7)
px = 100.0 * np.exp(np.cumsum(rng.standard_normal({bars}) * 0.001))
cs = np.cumsum(px)
""")
        setup_ms = (time.time() - t0) * 1000
        codes = [child_code(f, s) for f, s in grid]
        t1 = time.time()
        resp = post(f"/v1/session/{sid}/branch", {"codes": codes})
        branch_ms = (time.time() - t1) * 1000
        if len(resp["results"]) != len(grid):
            raise RuntimeError("branch returned an incomplete result batch")
        results = {}
        for expected, it in zip(grid, resp["results"]):
            if it["rc"] != 0 or it.get("error"):
                raise RuntimeError(f"branch {expected} failed: {it}")
            f, s, pnl = it["stdout"].split()
            key = (int(f), int(s))
            if key != expected:
                raise RuntimeError(f"branch order mismatch: {key} != {expected}")
            results[key] = float(pnl)
        checked_eval(f"/v1/session/{sid}/eval", "assert 'ma_f' not in globals(); assert 'pnl' not in globals()")
        return results, setup_ms, branch_ms, resp["snapshot_us"] / 1000
    finally:
        urllib.request.urlopen(
            urllib.request.Request(DAEMON + "/v1/session/" + sid, method="DELETE")
        )


def _mp_worker(args):
    import numpy as np
    bars, fast, slow = args
    rng = np.random.default_rng(7)
    px = 100.0 * np.exp(np.cumsum(rng.standard_normal(bars) * 0.001))
    cs = np.cumsum(px)
    ma_f = (cs[fast:] - cs[:-fast]) / fast
    ma_s = (cs[slow:] - cs[:-slow]) / slow
    n = min(len(ma_f), len(ma_s))
    sig = (ma_f[-n:] > ma_s[-n:]).astype(float)
    r_ = px[-n:][1:] / px[-n:][:-1] - 1.0
    return (fast, slow), float((sig[:-1] * r_).sum())


def run_multiprocessing(bars, grid, workers=None):
    import multiprocessing as mp
    t0 = time.time()
    with mp.Pool(processes=workers) as pool:
        out = pool.map(_mp_worker, [(bars, f, s) for f, s in grid])
    return dict(out), (time.time() - t0) * 1000


def _shared_worker(params):
    fast, slow = params
    ma_f = (_shared_cs[fast:] - _shared_cs[:-fast]) / fast
    ma_s = (_shared_cs[slow:] - _shared_cs[:-slow]) / slow
    n = min(len(ma_f), len(ma_s))
    sig = (ma_f[-n:] > ma_s[-n:]).astype(float)
    r_ = _shared_px[-n:][1:] / _shared_px[-n:][:-1] - 1.0
    return params, float((sig[:-1] * r_).sum())


def run_native_shared(bars, grid, workers=None):
    """Native COW comparison: precompute once, inherit arrays through fork.

    An opt-in local benchmark; fork must be supported by the host.
    """
    import multiprocessing as mp
    import numpy as np
    global _shared_px, _shared_cs
    t0 = time.perf_counter()
    rng = np.random.default_rng(7)
    _shared_px = 100.0 * np.exp(np.cumsum(rng.standard_normal(bars) * 0.001))
    _shared_cs = np.cumsum(_shared_px)
    with mp.get_context("fork").Pool(processes=workers) as pool:
        out = pool.map(_shared_worker, grid)
    return dict(out), (time.perf_counter() - t0) * 1000


def compare_results(w_res, native):
    if set(w_res) != set(native):
        raise RuntimeError("wisp and native result keys differ")
    mism = [k for k in w_res if not abs(w_res[k] - native[k]) <= 1e-6]
    if mism:
        raise RuntimeError(f"result mismatch for {mism}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bars", type=int, default=2_000_000)
    ap.add_argument("--grid", type=int, default=40)
    ap.add_argument("--workers", type=int, default=None, help="native pool size; match WISP_BRANCH_PARALLEL on the daemon")
    ap.add_argument("--native-shared", action="store_true", help="also compare native fork with shared precomputed arrays")
    args = ap.parse_args()
    if args.bars <= 300 or not 1 <= args.grid <= 64:
        ap.error("--bars must exceed 300; --grid must be between 1 and 64")
    if args.workers is not None and args.workers < 1:
        ap.error("--workers must be positive")
    grid = make_grid(args.grid)

    print(f"backtest sweep: {len(grid)} param combos over {args.bars:,} bars\n")

    w_res, setup_ms, branch_ms, snap_ms = run_wisp(args.bars, grid)
    print("=== wisp: precompute-in-parent + fork-per-combo ===")
    print(f"  parent setup (load + cumsum) : {setup_ms:7.1f} ms")
    print(f"  branch snapshot (parent block): {snap_ms:7.1f} ms")
    print(f"  branch total ({len(grid)} children)   : {branch_ms:7.1f} ms")
    print(f"  wisp wall clock              : {setup_ms + branch_ms:7.1f} ms")

    m_res, m_ms = run_multiprocessing(args.bars, grid, args.workers)
    print("\n=== host CPython multiprocessing (reload per task) ===")
    print(f"  wall clock                   : {m_ms:7.1f} ms")

    compare_results(w_res, m_res)
    print(f"\n  results match within 1e-6: True  ({len(w_res)} combos)")
    if args.native_shared:
        shared_res, shared_ms = run_native_shared(args.bars, grid, args.workers)
        compare_results(w_res, shared_res)
        print("\n=== host CPython fork (shared precompute, includes pool startup) ===")
        print(f"  wall clock                   : {shared_ms:7.1f} ms")
        print(f"  results match within 1e-6: True  ({len(w_res)} combos)")
    best = max(w_res, key=w_res.get)
    print(f"  best combo: fast={best[0]} slow={best[1]} pnl={w_res[best]:.4f}")


if __name__ == "__main__":
    main()

# Branch-sweep: precompute-in-parent + fork-per-parameter

The demo for Wisp's session-**branch** primitive: fork the current
Python state into independent COW sandboxes and gather their results.

## The programming model

A parameter sweep (backtest grid, hyperparameter search, Monte-Carlo
fan-out) has a shape: a large **shared** part (dataset + derived
features) and a small **per-task** part (the parameters). Wisp lets you
compute the shared part once and fork the rest:

```
POST /v1/session                      → create parent
POST /v1/session/:id/eval  {code}     → load data, precompute features
POST /v1/session/:id/branch {codes[]} → fork one COW child per param combo,
                                         run in parallel, gather results[]
```

Each child's linear memory is `MAP_PRIVATE` over the parent's snapshot:
every pre-branch page (numpy, the price series, the cumulative sums) is
**physically shared** — a child pays RAM only for what it dirties.
Children never mutate the parent. Parent evals can continue after the
snapshot is captured and do not change existing children's state.

## Run

```sh
cargo build --release
./target/release/wisp-runtime &
python3 bench/branch-sweep/demo.py --bars 2000000 --grid 40
```

## Reproduction on 2026-10-11

The existing reactor rebuilt successfully and the release daemon
compiled. Both accel modes passed the integration checks.

```sh
WISP_BRANCH_PARALLEL=8 ./target/release/wisp-runtime
# In another terminal:
python3 bench/branch-sweep/verify.py
python3 bench/branch-sweep/demo.py --bars 2000000 --grid 40 --workers 8 --native-shared
```

`WISP_DAEMON_URL` selects the daemon for both scripts. Repeat the
verification with a daemon started using `WISP_ACCEL=0` to check the
guest fallback. The verification covers matrix layouts and dtypes,
branch ordering, parent/child isolation, memory growth, repeated
branching and recovery after a child Python exception.

After the runtime fixes, 40 combinations × 2M bars, with 8 workers
on both sides:

| Path | Wall time, including setup |
|---|---:|
| Wisp branch | 796.3 ms |
| Native multiprocessing, reload/recompute per task | 1041.3 ms |
| Native fork pool, shared parent precompute | 279.4 ms |

All 40 results matched within `1e-6`. This demo rounds guest PnL to
six decimal places; it does **not** establish bit-for-bit equality.
The shared native baseline is faster on this workload. It reuses
worker processes, whereas Wisp creates a fresh sandbox per branch;
these are different isolation contracts. The original reload-based
comparison alone cannot establish a general advantage over native
COW execution. These are individual laptop runs, not statistical
performance guarantees.

Fixes following this reproduction:

- The session thread captures the snapshot, then child execution runs
  on the request's blocking thread. Parent evals continue during the
  batch; each child still sees the state at snapshot time.
- Only linear memory is copied. Sessions with opened WASI file or
  directory handles return HTTP **409** before branching. Close handles
  first and reopen files inside each child. This includes raw
  `os.open` descriptors; modified standard/preopen descriptors are
  rejected too. Full host-resource cloning is not implemented.
- stdout/stderr capture is bounded to **1 MiB per stream per eval**
  and drained after each eval. Excess output is discarded with an
  explicit `[wisp: output truncated at 1048576 bytes]` marker; streams
  stay usable for subsequent evals. Stateless and branch evals use
  the same behavior.
- Empty batches and batches above 4096 codes return HTTP **400**.
- Branch snapshots use anonymous, owner-only temporary files, cleaned
  automatically on success or failure. Session code buffers are freed
  after execution rather than accumulating across calls.

## Historical result (M4 Pro, 40 combos × 2M bars)

```
=== wisp: precompute-in-parent + fork-per-combo ===
  parent setup (load + cumsum) :   150.0 ms
  branch snapshot (parent block):  104.4 ms
  branch total (40 children)   :   674.7 ms
  wisp wall clock              :   824.7 ms
=== host CPython multiprocessing (reload per worker) ===
  wall clock                   :   974.2 ms
  results match: True  (40 combos)
```

Wisp beat the reload-based baseline in this historical run (825 ms
vs 974 ms). The result comparison used rounded values and a tolerance;
it did not prove bit-for-bit identity or compare a shared-state native
fork baseline. See the newer reproduction above.

## Interpreting the comparison

COW avoids repeatedly loading shared data, but a native fork pool can
also share its parent arrays. Compare both baselines with matched
worker counts. Wisp's per-child sandbox isolation and NumPy host
offload are useful properties; this benchmark does not establish
superior compute throughput over a native shared-state pool.

## Current limitation

The branch snapshot is written to a temp file (≈100 ms for 30 MB here)
and the parent blocks for that write. For very large parent state or
very high branch frequency this dominates. A future version can map the
parent memory `MAP_PRIVATE` directly per child without the file
round-trip (the per-call `/v1/eval` path already does this against the
boot snapshot fd); the temp-file version was the correct first cut for
correctness.

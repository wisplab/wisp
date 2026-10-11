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

## Isolated Native comparison

See [ISOLATED.md](ISOLATED.md) for the newer comparison using a fresh Native
process plus macOS Seatbelt for every parameter, matched to Wisp's fresh
branch model. It includes verified filesystem/network/exec restrictions,
shuffled repeated runs, full-precision result checks, warm/cold measurement
boundaries, and isolation limitations. On the current M2, the original 2M
sweep's warm medians are 337 ms (isolated Native) and 538 ms (Wisp); the
10K sweep reverses the ordering at 104 ms and 63 ms. Laptop noise and
NumPy version differences limit attribution of a precise runtime penalty.

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

## Snapshot backends

On macOS, branches now capture immutable memory with `mach_vm_remap` and
`copy=TRUE`, then create a separate COW mapping for each child. Parent and
child writes remain private; the snapshot has no filesystem copy. Every
branch still gets a fresh Wasmtime Store, Instance, WASI resource table and
output buffers. Store-independent imports are resolved once per daemon.

Other platforms retain the anonymous, owner-only file snapshot. macOS also
falls back to that path if kernel remapping fails. Set
`WISP_BRANCH_SNAPSHOT=file` to force the portable path, and
`WISP_BRANCH_PRELINK=0` to disable prepared imports for diagnostic comparisons.
The parent is blocked only while capture runs. Open WASI handles must still
be closed before branching; host-resource cloning is not implemented.

Responses include `snapshot_bytes`, `snapshot_backend`, and per-child
`timings` (`setup_us`, `eval_us`, `cleanup_us`). Guest execution time includes
NumPy computation and first-touch/COW faults, not just interpreter overhead.
See [OPTIMIZATION.md](OPTIMIZATION.md) for the measured bottlenecks and
matched before/after benchmark.

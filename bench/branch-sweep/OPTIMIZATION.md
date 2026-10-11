# Large-sweep profiling and optimization, 2026-10-11

The warm original workload (seed-7 random prices, 2M bars, 40 parameters,
eight concurrent fresh children) improved from **535.8 ms to 407.3 ms** in
single-service measurements: approximately **24% less wall time**. The
isolated Native controls were 341.0 ms and 337.9 ms respectively, within 1%.
Wisp's ratio of total-time medians against isolated Native dropped from
**1.57 to 1.21**. These are laptop samples, not fixed performance guarantees.

## What was slow

An instrumented baseline batch found that child accumulated elapsed time
was about 90% guest execution, 4% initialization, and 6% cleanup. Execution
includes Python/NumPy, allocation, page faults and COW writes; this is not
an instruction-level CPU profile. Accumulated parallel child times also
cannot be added directly to wall time.

The original implementation copied the entire parent linear memory to an
anonymous file and remapped that file into each child. It copied **61,865,984
bytes** per sweep, including allocator free space, not only the useful arrays.
In the later single-service baseline, median capture took **63.864 ms**.
Each child also rebuilt its WASI/host linker and resolved identical imports.

This workload does subtract/divide/compare/cast/multiply/reduce operations;
the existing native GEMM acceleration does not accelerate those operations.

## Changes

1. **macOS memory COW snapshots.** `mach_vm_remap(copy=TRUE)` first captures
   a private snapshot of the quiescent parent, and then privately remaps that
   snapshot into each fresh child's committed memory. It avoids the parent
   heap-to-file copy, and retains independent parent/child/sibling writes.
   Only the committed region is replaced; Wasmtime's reservation and guards
   remain owned by its custom linear-memory implementation. Snapshot lifetime
   is managed by an owning region that deallocates the mapping on drop.
   ABI declarations and flags use the existing `mach2` dependency.
2. **Prepared branch imports.** Resolve store-independent WASI/host imports
   once with `InstancePre`. Every child still receives a fresh Store,
   Instance, memory, WASI resource table and bounded output capture. There is
   no guest-state reuse and no shared mutable WASI table.
3. **Profiling and reproducibility.** Responses add snapshot bytes/backend
   and per-child setup/eval/cleanup microseconds. The fairness harness can
   select a release binary, interleave an older binary, force the file backend,
   disable prepared imports, record global VM counters, and optionally time
   each NumPy stage. It records executable SHA-256 hashes with the results.

Other platforms retain the anonymous, owner-only file implementation. macOS
falls back to it with a warning if kernel capture fails. Open or modified
WASI descriptors still cause HTTP 409. No numerical expression or parameter
code was changed in the headline benchmark.

Apple API reference:
[mach_vm_remap](https://developer.apple.com/documentation/kernel/1402218-mach_vm_remap).

## Single-service comparison

One discarded warmup and nine measured shuffled rounds for each batch.
Both batches contain the same unsandboxed fresh-fork control, isolated
Native fresh-fork implementation, and one Wisp service. They ran sequentially
on the same M2 / 8-core / 8-GiB machine. Native NumPy is 2.4.2, guest NumPy
is 1.26.4; both CPython versions are 3.14.3. BLAS thread settings are 1.

| Path / batch | Parent setup median | Branch median | Total median | Total interquartile range |
|---|---:|---:|---:|---:|
| Old Wisp | 90.4 ms | 444.8 ms | 535.8 ms | 509.6–558.9 ms |
| Optimized Wisp | 80.7 ms | 319.3 ms | 407.3 ms | 398.6–426.2 ms |
| Isolated Native, old-Wisp batch | 35.4 ms | 307.5 ms | 341.0 ms | 330.3–352.3 ms |
| Isolated Native, optimized-Wisp batch | 32.2 ms | 303.9 ms | 337.9 ms | 333.3–339.9 ms |

Medians of components are independent and need not sum to the total median.
Native includes fresh fork, installation of Seatbelt on every child,
computation and collection; Wisp includes actual session/branch HTTP calls,
parent setup, snapshot, fresh instances, execution and collection. Service
bootstrap and parent disposal are excluded on both sides.

Snapshot capture fell from **63.864 ms to 0.256 ms** in these batches.
The approximately 64-ms reduction explains a substantial portion, but not
all, of the wall-time improvement. The remainder includes prepared imports,
different backing/page-fault behavior, and run-to-run system variation; it
has not been uniquely attributed to one mechanism. The new path avoids
copying unused heap pages into the file cache. Global swap-out counters rose
by 4,224 pages in the old batch and zero in the candidate batch, but these
are system-wide counters and cannot be assigned exclusively to Wisp.

Within each batch, the median same-round Wisp/Native ratio is **1.55 before**
and **1.24 after**; ratios of separate medians are 1.57 and 1.21. Neither
statistic establishes a constant runtime penalty.

The saved baseline binary is commit `52b4b3c` plus additive diagnostic
fields/timers, before either optimization. Measurements were made with an
uncommitted worktree: the result's `git_commit` is the pre-change HEAD, while
the executable SHA-256 identifies the actual tested binary. The candidate
source is included in the subsequent optimization commit.

Raw data:
- `results/large-before-single-20261011.json`
- `results/large-optimized-single-20261011.json`
- `results/large-before-20261011.json` (initial instrumented baseline)

## Why the co-resident A/B run is not the headline

An additional 11-round run kept both Wisp binaries resident and shuffled
all four backends within every round. It validated identical results and
security probes, and found a median paired candidate/baseline ratio of
0.896. However, **all** paths were much slower and noisier: isolated Native
was 1,706.5 ms, old Wisp 2,230.7 ms and candidate Wisp 1,983.0 ms, with large
outliers. The cause was not conclusively isolated; the machine was recently
under compilation load and has limited RAM. `pmset -g therm` did not provide
recorded thermal or CPU-power status. This run is retained rather than
silently discarded, but it cannot be substituted for a quiet single-service
latency measurement.

Raw data: `results/large-optimized-ab-20261011.json`.

## Remaining hotspot

A separate five-round diagnostic run times individual stages. It stages
and releases parent temporaries in expression order and adds timers; these
numbers are diagnostic, not the headline performance comparison. Child
numbers are per-operation medians across 200 children at concurrency eight.
They include allocation and first-touch/COW faults.

| Stage | Isolated Native | Optimized Wisp |
|---|---:|---:|
| Parent: 2M standard-normal draws | 10.77 ms | **51.79 ms** |
| Parent: price cumulative sum | 7.39 ms | 8.03 ms |
| Parent: exponential | 4.68 ms | 7.39 ms |
| Parent: shared cumulative sum | 7.28 ms | 7.56 ms |
| Child: first moving average | 7.45 ms | 13.66 ms |
| Child: second moving average | 6.33 ms | 7.55 ms |
| Child: comparison + float conversion | 7.36 ms | 10.03 ms |
| Child: returns | 8.29 ms | 10.48 ms |
| Child: weighted PnL + reduction | 8.85 ms | 7.59 ms |

The largest remaining parent-stage gap is random-number generation. Its
approximately 41-ms difference accounts for much of the observed ~49-ms
setup difference. This does not prove a universal 4.8x WASM RNG penalty:
NumPy version, build, indirect calls and RNG arithmetic differ between the
backends. A next optimization should inspect the PCG64/normal sampler and
its WASI compilation, preserving the random stream, generator state,
rejection behavior and parent/child independence. Faster approximate random
sampling would change the benchmark and is not an equivalent replacement.

Raw data: `results/large-array-profile-20261011.json`.

## Validation

- Seven Rust tests pass without warnings: capability matching, bounded
  output capture, file/Mach snapshot isolation and lifetime, rejection of
  invalid remap size/alignment.
- The full integration suite passes for **Mach/file snapshots × accel on/off**.
  It covers dtype/layout/vector/matrix cases, fresh child state, memory growth,
  ordering, exceptions and recovery, bounded stdout/stderr, open-descriptor
  rejection and recovery, and parent execution during an active child.
- The strengthened concurrent test overwrites all one-million parent array
  entries and allocates another four-million-element array while the child
  sleeps. The child verifies its old endpoints and **int64** checksum; the
  parent verifies its new values and allocation. The child also grows its own
  memory with a six-million-element float64 array and rechecks the original
  checksum after growth. WASM32's default integer
  width requires an explicit int64 sum in this test.
- Native file read/write, live network connection, exec and fork denial probes
  pass. Both Wisp versions' disabled file/network/shell bridge probes pass.
- All 40 headline results agree at full precision within 1e-9; observed
  maximum absolute PnL error is **7.78e-15**. `cargo fmt`, Python compilation,
  and whitespace checks pass.

These are the isolation guarantees already described in ISOLATED.md; they do
not equate Seatbelt and WASM threat models or add CPU/memory quotas.

## Reproduce

Keep a copy of the old release binary before rebuilding the candidate:

```sh
cp target/release/wisp-runtime /tmp/wisp-before-large
cargo test -p wisp-runtime
cargo build --release -p wisp-runtime
# Single-service samples, with identical Native controls:
python3 bench/branch-sweep/isolated.py --repeats 9 \
  --output bench/branch-sweep/results/candidate.json
python3 bench/branch-sweep/isolated.py --repeats 9 \
  --wisp-binary /tmp/wisp-before-large \
  --output bench/branch-sweep/results/baseline.json
# Diagnostic operation timers, separate from headline timing:
python3 bench/branch-sweep/isolated.py --profile-arrays --repeats 5 \
  --output bench/branch-sweep/results/array-profile.json
# Optional co-resident matched A/B, requiring additional RAM:
python3 bench/branch-sweep/isolated.py --repeats 11 \
  --baseline-binary /tmp/wisp-before-large \
  --output bench/branch-sweep/results/ab.json
```

To isolate mechanisms on the candidate binary, use `--snapshot-backend file`
and/or `--no-prelink`. Their environment equivalents are
`WISP_BRANCH_SNAPSHOT=file` and `WISP_BRANCH_PRELINK=0`. The daemon owns only
its own processes and temporary snapshots; the harness cleans them up.

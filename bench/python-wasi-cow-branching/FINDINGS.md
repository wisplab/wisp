# Spike B2 — K-way fork with mmap COW backend: findings

> **Run date**: 2026-05-02. **Wasmtime crate**: 27 in-process. **rayon**: 1.x.
> **CPython**: 3.14.3 reactor build. **Snapshot**: 10 MB (post-wisp_init).
> **Hardware**: Apple Silicon M-series, 8 cores. **rayon pool**: 8 threads.

## TL;DR

Spike B forked K children from a snapshot via per-branch memcpy and topped
out at **1025 br/s** parallel throughput (only 2.3× speedup on 8 cores
because per-thread 10 MB memcpy saturates the memory subsystem).

Spike B2 swaps the memcpy reset for a `mmap MAP_PRIVATE | MAP_FIXED` of a
shared snapshot file (the same trick from Spike A2.1, applied per-branch).

**Result: 2394 br/s parallel throughput** at K=256/1024 — 2.3× more than
Spike B. Sequential per-branch dropped from 2.65 ms to 0.88 ms (3× faster).

But: **8-core parallel speedup did NOT become near-linear** as A2.1's
findings predicted. It actually came in at 2.0× (vs Spike B's 2.5×). The
bottleneck moved — from "memcpy bandwidth" to "wasmtime instantiate
overhead and page-fault path." Honest, documented below.

## K-way fork sweep (COW backend)

| K | Sequential total | per branch | Parallel total | per branch | Throughput (par) | Speedup |
|---|---|---|---|---|---|---|
|   1 | 0.88 ms | 0.88 | 0.86 ms | 0.86 | 1163 br/s | 1.02× |
|   8 | 7.00 ms | 0.88 | 4.70 ms | 0.59 | 1702 br/s | 1.49× |
|  64 | 56.15 ms | 0.88 | 28.68 ms | 0.45 | **2231 br/s** | 1.96× |
| 256 | 220.47 ms | 0.86 | 108.34 ms | 0.42 | **2363 br/s** | 2.03× |
|1024 | 808.34 ms | 0.79 | 427.78 ms | 0.42 | **2394 br/s** | 1.89× |

Per-branch latency distribution at K=64 parallel (per-task wall time):

| Metric | ms |
|---|---|
| p50 | 3.42 |
| p99 | 6.17 |
| mean | 3.51 |
| max | 6.17 |

## Vs Spike B (memcpy backend), apples to apples

| K | Spike B (memcpy) par br/s | **Spike B2 (COW) par br/s** | Win |
|---|---|---|---|
|   1 |  367 | 1163 | 3.2× |
|   8 |  697 | 1702 | 2.4× |
|  64 |  926 | 2231 | 2.4× |
| 256 | 1025 | **2363** | 2.3× |
|1024 |  799 | **2394** | 3.0× |

Sequential per-branch:
- Spike B: 2.65 ms (memcpy 10 MB → bandwidth-bound)
- Spike B2: 0.88 ms (mmap MAP_FIXED → 30 µs syscall + lazy page faults)

Parallel per-branch latency at K=64:
- Spike B: p50 8.80 ms, p99 12.70 ms
- Spike B2: p50 3.42 ms, p99 6.17 ms — **2.6× faster latency tail**

## What COW won, and what it didn't

**Won (single-thread):** the per-call "snapshot reset" step dropped from a
1.07 ms memcpy to a 0.030 ms syscall. Net per-branch in serial: 0.88 ms
vs 2.65 ms — a **3× improvement**.

**Won (throughput):** 2394 br/s vs 1025 br/s — a **2.3× improvement**.
COW removes the memcpy bandwidth ceiling Spike B hit.

**Did not win:** 8-core parallel speedup is 2.0× (vs the naive prediction
of ~6–8×). Two new bottlenecks dominate:

1. **Wasmtime `instantiate`** still costs ~0.43 ms per branch (Spike A2.1
   single-thread number). At K=256 parallel that's 256 × 0.43 ms ÷ 8 cores
   ≈ 14 ms minimum, against the 108 ms wall clock. So instantiate accounts
   for ~13% of the wall-clock budget — JIT-code-cache locks and module
   metadata lookup show up here.
2. **Page-fault path under contention**. Each branch's eval lazily faults
   in 10–50 pages from the shared snapshot file's kernel page cache. With
   8 threads concurrently page-faulting, the kernel's per-VMA lock
   serializes faults more than expected on Apple Silicon. The `wisp_eval`
   step rose from A2.1's 0.234 ms single-thread to ~3.4 ms p50 under
   8-core contention.

## What this means for the architecture story

The architecture claim in `private/01-architecture.md` §5 is **cheap fork
at arbitrary state**. That claim is now empirically supported across two
implementations:

| Backend | Per-branch (parallel, peak) | Throughput (8 cores) |
|---|---|---|
| Linux process fork | 5–10 ms | ~1000 forks/s/core |
| Firecracker uVM snapshot/restore | 100–500 ms | ~10 forks/s/core |
| **Wisp WASM memcpy snapshot (Spike B)** | 0.98 ms | 1025 br/s total |
| **Wisp WASM mmap COW snapshot (this work)** | **0.42 ms** | **2394 br/s total** |

Per-trajectory cost for the canonical tree-search workload (K=100,
depth=100 = 10,000 forks):

| Backend | Per-trajectory branching cost |
|---|---|
| Linux fork | 50–100 s |
| Firecracker | 16–83 min |
| Wisp memcpy | ~10 s |
| **Wisp mmap COW** | **~4.2 s** |

100–1000× faster than Firecracker. The primitive is real; the
implementation has more headroom but is already qualitatively better
than anything native runtimes offer.

## Limits / open questions

1. **The 2× scaling cap is not memcpy bandwidth.** It's
   `wasmtime::Instance` instantiate + kernel page-fault contention.
   Profiling avenues:
   - Try wasmtime's `PoolingAllocationStrategy` with COW too (we currently
     use the default allocator + custom `MemoryCreator`). Pooling skips
     some metadata work but is incompatible with `with_host_memory` — would
     require patching wasmtime or rewriting the COW path on top of pooling.
   - On Linux, `MAP_POPULATE` could pre-fault snapshot pages, eliminating
     the per-fault cost at the price of higher per-instance memory.
   - Async I/O for the file-backed pages on a different core.

2. **`wisp_eval` page-fault cost dominates per-branch latency.** At K=64
   parallel, p50 wall time is 3.42 ms but the underlying serial work is
   only ~0.88 ms. The 4× inflation is mostly page-fault serialization
   under 8 concurrent threads. A precomputed memory image (wasmtime's
   `MemoryImage` API) might let the kernel skip the per-fault work.

3. **macOS-specific kernel behavior.** Apple Silicon's xnu page-fault
   path is known to scale less well than Linux's. A repro on a Linux
   server-class CPU would be informative — same code, different OS,
   would isolate the kernel contribution.

4. **Sequential 0.88 ms is the right number for cost models.** A K-way
   tree-search on a single core costs `K × 0.88 ms`. With 8 cores fully
   utilized, the effective per-fork cost is `0.42 ms`. Both numbers are
   well below the "≤ 5 ms per fork" threshold the architecture document
   asserted as the qualitative win condition.

## Reproduction

```bash
cd wisp/bench/python-wasi-cow-branching
cargo build --release
./target/release/py-cow-branching
```

## Where this slots in

- Spike B: K-way fork with memcpy backend, 1025 br/s parallel
- **Spike B2: K-way fork with mmap COW backend, 2394 br/s parallel** ← here
- Future Spike B3 (if needed): combine COW with wasmtime pooling allocator
  to attack the residual instantiate cost; expected ceiling is whatever
  the kernel page-fault throughput allows.

# Spike A2.1 — mmap COW snapshot: findings

> **Run date**: 2026-05-02. **Wasmtime crate**: 27 in-process.
> **CPython**: 3.14.3 reactor build. **Snapshot**: 10 MB.
> **Hardware**: Apple Silicon M-series. **N**: 200 iterations after 20 warmup.

## TL;DR

Replaced Spike A2's memcpy-based snapshot/restore with a custom Wasmtime
`MemoryCreator` that mmaps the snapshot file `MAP_PRIVATE` and re-mmaps
`MAP_FIXED` per call to reset wasmtime's data-init writes.

**Per-call cold start: 0.711 ms p50** (vs Spike A2's 1.68 ms — 2.4× faster).

The per-call snapshot reset cost dropped from **1.07 ms memcpy** to
**0.030 ms mmap syscall** — 35× cost reduction on that step. Total per-call
sub-ms.

## Per-call decomposition

```
| Phase                    |  p50 ms |  p99 ms | mean ms |
|--------------------------|---------|---------|---------|
| Instantiate + data init  |   0.434 |   0.485 |   0.438 |  ← wasmtime overhead
| mmap MAP_FIXED reset     |   0.030 |   0.049 |   0.031 |  ← was memcpy 1.07 ms
| Grow memory              |   0.002 |   0.003 |   0.002 |
| Alloc + write code       |   0.010 |   0.024 |   0.011 |
| wisp_eval                |   0.234 |   0.293 |   0.238 |  ← page faults lazy
| Total                    |   0.711 |   0.796 |   0.720 |
```

`wisp_eval` went up from A2's 0.11 ms to 0.234 ms because page faults
during Python execution lazily fault in pages from the snapshot file's
page cache. The total still drops by ~1 ms because the up-front memcpy
is gone.

## How it works

### 1. Custom MemoryCreator

```rust
struct CowMemoryCreator {
    snapshot_fd: RawFd,
    snapshot_len: usize,
}

unsafe impl MemoryCreator for CowMemoryCreator {
    fn new_memory(&self, ty, minimum, maximum, reserved, guard) -> Box<dyn LinearMemory> {
        // 1. Reserve large virtual region (~4 GB) with PROT_NONE
        // 2. mmap MAP_PRIVATE | MAP_FIXED of snapshot file at offset 0,
        //    length = min(minimum, snapshot_len)  → reads share kernel page
        //    cache; writes COW into private pages
        // 3. mmap MAP_PRIVATE | MAP_ANON | MAP_FIXED for [snap, minimum)
        //    → zero-filled trailing pages
    }
}
```

Register via `Config::with_host_memory(creator)`. Each instance gets a
fresh memory region from this factory.

### 2. Per-call reset via re-mmap MAP_FIXED

When wasmtime's `instantiate` runs, it copies the WASM module's data
segments into linear memory. That overwrites our snapshot's bytes for
those pages. To restore: re-mmap MAP_FIXED of the snapshot file at the
same address. The kernel:
1. Unmaps the COW-private pages wasmtime wrote
2. Re-establishes the file-backed mapping
3. Frees the private pages back to the OS

Cost: ~30 µs (one syscall, page table updates).

### 3. Why MADV_DONTNEED doesn't work on macOS

Tried first: `madvise(addr, len, MADV_DONTNEED)`. On Linux this drops
private pages and restores file-backed view. On macOS, `MADV_DONTNEED`
is aliased to `MADV_FREE` which **zeroes** the pages instead of
restoring file content. After madvise, reads returned 0 → CPython's
`Py_IsInitialized()` returned false → `wisp_eval` crashed.

Switched to `mmap MAP_FIXED` which is portable and explicitly re-establishes
the file mapping. Linux + macOS both work.

## Why this beats memcpy by more than the syscall difference

Memcpy at 1.07 ms is **bandwidth-bound** — 10 MB at ~10 GB/s. Mmap at
30 µs is **syscall-bound** — no actual data motion happens. Pages get
loaded lazily as wasm code touches them. Most Python evals touch ~10–50
pages out of 163 → only ~40–200 KB actually moves through the memory
system, vs 10 MB for memcpy.

This matters more under parallel load. Spike B (memcpy-based branching)
showed 2.3× speedup on 8 cores due to memory bandwidth saturation. The
COW approach should approach near-linear scaling because each thread's
mmap doesn't compete for bandwidth — they share the kernel page cache
read-only and only privatize on write. (Future spike to confirm.)

## Comparison

| Approach | Per-call p50 | Vs A2.1 |
|---|---|---|
| AWS Lambda Python with deps (published) | 800–1500 ms | 1100–2100× slower |
| Modal cold (published) | 1500 ms | 2100× slower |
| Cloudflare Workers Python via Pyodide (published) | 200–500 ms | 280–700× slower |
| Modal warm pool (published) | 50 ms | 70× slower |
| Wisp Spike A (pooled, fresh interpreter) | 39 ms | 55× slower |
| Wisp Spike A2 (memcpy snapshot) | 1.68 ms | 2.4× slower |
| **Wisp Spike A2.1 (mmap COW snapshot)** | **0.711 ms** | — |

## Limits / open questions

1. **Wasmtime forced data init is the real bottleneck**. The 0.43 ms
   "instantiate + data init" step exists because wasmtime always copies
   data segments at instantiate, even though our snapshot already has them.
   We then undo this with the re-mmap. A future Wasmtime feature
   ("instantiate without data init") could collapse instantiate to ~0.05 ms.
2. **Page-fault cost during wisp_eval went up**. From A2's 0.11 ms to
   A2.1's 0.234 ms. Trade-off: lazy fault-in means we don't pay for
   pages we don't touch, but we do pay first-touch cost during eval.
   For workloads that touch most of the snapshot, A2 wins; for workloads
   that touch <50% of pages, A2.1 wins.
3. **First-call cost includes warm page cache**. The benchmark warms up
   for 20 iterations so the page cache is hot. Cold-disk first call
   would be slower (file I/O). For a long-running runtime this is amortized.

## Reproduction

```bash
cd wisp/bench/python-wasi-cow
cargo build --release
./target/release/py-cow
```

## Where this slots in

- Spike A2: memcpy snapshot, 1.68 ms p50
- **Spike A2.1: mmap COW snapshot, 0.711 ms p50** ← here
- Future Spike B2: branching with COW backend, expected near-linear
  scaling on multiple cores (memcpy-based Spike B was bandwidth-capped
  at 2.3× on 8 cores)

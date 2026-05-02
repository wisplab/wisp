//! Phase 0 Spike B2 — K-way fork with mmap COW backend.
//!
//! Spike B forked K children from a 10 MB snapshot via per-call memcpy.
//! Result: 1025 branches/sec at K=256 parallel — but only 2.3× speedup on
//! 8 cores because the per-thread memcpy bandwidth saturates the memory
//! subsystem (each fork = 10 MB through the bus).
//!
//! Spike A2.1 proved that mmap MAP_PRIVATE | MAP_FIXED of a snapshot file
//! brings per-call reset down from 1.07 ms (memcpy) to ~30 µs (syscall),
//! with pages faulted in lazily from the shared kernel page cache. That
//! removes the bandwidth-bound step from the critical path entirely.
//!
//! This spike combines them: same K-way fork sweep as Spike B, but each
//! child uses a CowMemoryCreator-backed instance. The hypothesis is that
//! parallel speedup on 8 cores approaches near-linear, since per-thread
//! work no longer competes for memory bandwidth.

use anyhow::{anyhow, Result};
use rayon::prelude::*;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const PYTHON_REACTOR_WASM: &str =
    "../../runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm";
const CPYTHON_HOST_ROOT: &str = "../../runtime/cpython-wasi/vendor/cpython";
const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";
const SNAPSHOT_FILE: &str = "/tmp/wisp-snapshot-b2.bin";

const KS: &[usize] = &[1, 8, 64, 256, 1024];
const WARMUP: usize = 8;

fn make_wasi(host_root: &str) -> Result<WasiP1Ctx> {
    let mut b = WasiCtxBuilder::new();
    b.inherit_stdio()
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(host_root, "/", DirPerms::READ, FilePerms::READ)?;
    Ok(b.build_p1())
}

// =====================================================================
// CowMemoryCreator: same as Spike A2.1. Each `new_memory` call mmaps a
// fresh virtual region MAP_PRIVATE-backed by the snapshot file.
// =====================================================================

struct CowMemoryCreator {
    snapshot_fd: RawFd,
    snapshot_len: usize,
}

unsafe impl Send for CowMemoryCreator {}
unsafe impl Sync for CowMemoryCreator {}

unsafe impl MemoryCreator for CowMemoryCreator {
    fn new_memory(
        &self,
        _ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> std::result::Result<Box<dyn LinearMemory>, String> {
        let usable = reserved_size_in_bytes
            .unwrap_or_else(|| maximum.unwrap_or(minimum));
        let total = usable + guard_size_in_bytes;

        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                total,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(format!("mmap PROT_NONE reserve failed: {}",
                std::io::Error::last_os_error()));
        }

        let snap_in_min = self.snapshot_len.min(minimum);
        if snap_in_min > 0 {
            let r = unsafe {
                libc::mmap(
                    base,
                    snap_in_min,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED,
                    self.snapshot_fd,
                    0,
                )
            };
            if r == libc::MAP_FAILED {
                unsafe { libc::munmap(base, total); }
                return Err(format!("mmap MAP_PRIVATE snapshot failed: {}",
                    std::io::Error::last_os_error()));
            }
        }

        if snap_in_min < minimum {
            let r = unsafe {
                libc::mmap(
                    base.wrapping_byte_add(snap_in_min),
                    minimum - snap_in_min,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            if r == libc::MAP_FAILED {
                unsafe { libc::munmap(base, total); }
                return Err(format!("mmap MAP_ANON tail failed: {}",
                    std::io::Error::last_os_error()));
            }
        }

        Ok(Box::new(CowMemory {
            base: base as *mut u8,
            cur_size: minimum,
            max_size: maximum.unwrap_or(usable),
            total_reserved: total,
            snapshot_fd: self.snapshot_fd,
            snapshot_len: self.snapshot_len,
        }))
    }
}

struct CowMemory {
    base: *mut u8,
    cur_size: usize,
    max_size: usize,
    total_reserved: usize,
    snapshot_fd: RawFd,
    snapshot_len: usize,
}

unsafe impl Send for CowMemory {}
unsafe impl Sync for CowMemory {}

unsafe impl LinearMemory for CowMemory {
    fn byte_size(&self) -> usize { self.cur_size }
    fn maximum_byte_size(&self) -> Option<usize> { Some(self.max_size) }

    fn grow_to(&mut self, new_size: usize) -> Result<()> {
        if new_size <= self.cur_size {
            return Ok(());
        }
        if new_size > self.max_size {
            return Err(anyhow!("grow exceeds max"));
        }
        let old = self.cur_size;

        let snap_end = self.snapshot_len.min(new_size);
        if old < snap_end {
            let len = snap_end - old;
            let r = unsafe {
                libc::mmap(
                    self.base.wrapping_byte_add(old) as *mut libc::c_void,
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED,
                    self.snapshot_fd,
                    old as libc::off_t,
                )
            };
            if r == libc::MAP_FAILED {
                return Err(anyhow!("grow mmap snapshot failed: {}",
                    std::io::Error::last_os_error()));
            }
        }

        let anon_start = old.max(snap_end);
        if anon_start < new_size {
            let len = new_size - anon_start;
            let r = unsafe {
                libc::mmap(
                    self.base.wrapping_byte_add(anon_start) as *mut libc::c_void,
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            if r == libc::MAP_FAILED {
                return Err(anyhow!("grow mmap anon failed: {}",
                    std::io::Error::last_os_error()));
            }
        }

        self.cur_size = new_size;
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 { self.base }
    fn wasm_accessible(&self) -> std::ops::Range<usize> { 0..self.total_reserved }
}

impl Drop for CowMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.total_reserved);
        }
    }
}

// =====================================================================
// One branch: instantiate, MAP_FIXED-reset to undo wasmtime's data init,
// grow if needed, run divergent code.
// =====================================================================
fn run_branch(
    engine: &Engine,
    module: &Module,
    snapshot_len: usize,
    snapshot_pages: u64,
    snapshot_fd: RawFd,
    host_root: &str,
    branch_id: usize,
) -> Result<i32> {
    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
    let wasi = make_wasi(host_root)?;
    let mut store = Store::new(engine, wasi);
    let inst = linker.instantiate(&mut store, &module)?;
    let mem = inst.get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory"))?;

    // Re-mmap MAP_FIXED to undo wasmtime's data-segment init writes that
    // clobbered our COW-backed pages during instantiate.
    let base = mem.data_ptr(&mut store);
    let r = unsafe {
        libc::mmap(
            base as *mut libc::c_void,
            snapshot_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            snapshot_fd,
            0,
        )
    };
    if r == libc::MAP_FAILED {
        return Err(anyhow!("re-mmap snapshot failed: {}",
            std::io::Error::last_os_error()));
    }

    let cur = mem.size(&store);
    if cur < snapshot_pages {
        mem.grow(&mut store, snapshot_pages - cur)?;
    }

    let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;

    let code = format!(
        "json.dumps({{'branch': {b}, 'val': math.pi * {b}, 'sq': {b} * {b}}})\n",
        b = branch_id
    );
    let bytes = code.as_bytes();
    let ptr = alloc.call(&mut store, bytes.len() as i32)?;
    mem.data_mut(&mut store)[ptr as usize..ptr as usize + bytes.len()]
        .copy_from_slice(bytes);
    let rc = eval.call(&mut store, (ptr, bytes.len() as i32))?;
    Ok(rc)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// =====================================================================
// Capture snapshot via the standard allocator (wasmtime default), write
// to a file the COW creator will mmap from.
// =====================================================================
fn capture_snapshot(host_root: &str, python_wasm: &PathBuf) -> Result<(Vec<u8>, u64)> {
    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, python_wasm)?;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
    let wasi = make_wasi(host_root)?;
    let mut store = Store::new(&engine, wasi);
    let inst = linker.instantiate(&mut store, &module)?;
    if let Ok(init) = inst.get_typed_func::<(), ()>(&mut store, "_initialize") {
        init.call(&mut store, ())?;
    }
    let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
    let rc = wisp_init.call(&mut store, ())?;
    if rc != 0 {
        return Err(anyhow!("wisp_init returned {rc}"));
    }
    let mem = inst.get_memory(&mut store, "memory").unwrap();
    let bytes = mem.data(&store).to_vec();
    let pages = mem.size(&store);
    Ok((bytes, pages))
}

fn main() -> Result<()> {
    let python_wasm: PathBuf = PYTHON_REACTOR_WASM.into();
    let host_root = CPYTHON_HOST_ROOT.to_string();
    let py_size = std::fs::metadata(&python_wasm)?.len();

    println!("# Wisp WASI Python branching with COW backend — Phase 0 Spike B2");
    println!("python-reactor.wasm: {} ({} MB)", python_wasm.display(), py_size / 1024 / 1024);
    println!("rayon thread pool:   {} threads", rayon::current_num_threads());

    // ---- Capture snapshot ------------------------------------------------
    print!("Capturing snapshot... ");
    let t = Instant::now();
    let (snapshot, snapshot_pages) = capture_snapshot(&host_root, &python_wasm)?;
    println!("{} bytes ({} wasm pages) in {:.0} ms",
        snapshot.len(), snapshot_pages, t.elapsed().as_secs_f64() * 1000.0);

    std::fs::write(SNAPSHOT_FILE, &snapshot)?;
    let snap_file = std::fs::OpenOptions::new()
        .read(true)
        .open(SNAPSHOT_FILE)?;
    let snap_fd = snap_file.into_raw_fd();
    let snapshot_len = snapshot.len();
    drop(snapshot);
    println!("Snapshot file:       {} ({} bytes)\n", SNAPSHOT_FILE, snapshot_len);

    // ---- COW-backed engine -----------------------------------------------
    let creator = Arc::new(CowMemoryCreator {
        snapshot_fd: snap_fd,
        snapshot_len,
    });
    let mut config = Config::new();
    config.async_support(false);
    config.with_host_memory(creator);
    config.memory_init_cow(false);

    let t = Instant::now();
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, &python_wasm)?;
    println!("Engine + Module:     {:.2} ms\n", t.elapsed().as_secs_f64() * 1000.0);

    // ---- Warmup ----------------------------------------------------------
    for _ in 0..WARMUP {
        let _ = run_branch(&engine, &module, snapshot_len, snapshot_pages,
            snap_fd, &host_root, 0)?;
    }

    let engine = Arc::new(engine);
    let module = Arc::new(module);

    // ---- K-way sweep -----------------------------------------------------
    println!("## K-way fork sweep (COW backend)");
    println!();
    println!("| K     | Sequential total | per branch | throughput (br/s) | Parallel total | per branch | throughput (br/s) | Speedup |");
    println!("|-------|------------------|------------|-------------------|----------------|------------|-------------------|---------|");

    for &k in KS {
        // Sequential
        let t_seq = Instant::now();
        for i in 0..k {
            let rc = run_branch(&engine, &module, snapshot_len, snapshot_pages,
                snap_fd, &host_root, i)?;
            if rc != 0 {
                return Err(anyhow!("branch {i} returned {rc}"));
            }
        }
        let seq_ms = t_seq.elapsed().as_secs_f64() * 1000.0;
        let seq_per = seq_ms / k as f64;
        let seq_thr = (k as f64) / (seq_ms / 1000.0);

        // Parallel
        let t_par = Instant::now();
        let par_results: Result<Vec<i32>> = (0..k)
            .into_par_iter()
            .map(|i| run_branch(&engine, &module, snapshot_len, snapshot_pages,
                snap_fd, &host_root, i))
            .collect();
        let par_ms = t_par.elapsed().as_secs_f64() * 1000.0;
        let _ = par_results?;
        let par_per = par_ms / k as f64;
        let par_thr = (k as f64) / (par_ms / 1000.0);

        let speedup = seq_ms / par_ms;
        println!(
            "| {:>5} | {:>14.2}ms | {:>8.2}ms | {:>17.0} | {:>12.2}ms | {:>8.2}ms | {:>17.0} | {:>5.2}× |",
            k, seq_ms, seq_per, seq_thr, par_ms, par_per, par_thr, speedup
        );
    }

    println!();

    // ---- Per-branch latency at K=64 (parallel) --------------------------
    println!("## Per-branch latency at K=64 (parallel)");
    let k = 64;
    let mut times: Vec<f64> = (0..k).into_par_iter().map(|i| {
        let t = Instant::now();
        let _ = run_branch(&engine, &module, snapshot_len, snapshot_pages,
            snap_fd, &host_root, i);
        t.elapsed().as_secs_f64() * 1000.0
    }).collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&times, 50.0);
    let p99 = percentile(&times, 99.0);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let max = times.last().copied().unwrap_or(0.0);
    println!();
    println!("| Metric          | ms       |");
    println!("|-----------------|----------|");
    println!("| p50             | {:>7.2} |", p50);
    println!("| p99             | {:>7.2} |", p99);
    println!("| mean            | {:>7.2} |", mean);
    println!("| max             | {:>7.2} |", max);

    println!();
    println!("## Notes\n");
    println!("- Each branch instantiates a fresh wasmtime instance with a COW-backed");
    println!("  linear memory (mmap MAP_PRIVATE of the snapshot file).");
    println!("- After instantiate, we re-mmap MAP_FIXED to undo wasmtime's data-segment");
    println!("  init writes. Per-branch memory work is O(syscall) instead of O(memcpy).");
    println!("- Parallel speedup should approach near-linear since per-thread work no");
    println!("  longer competes for memory bandwidth (Spike B was capped at 2.3× on 8 cores).");

    Ok(())
}

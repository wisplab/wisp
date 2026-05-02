//! Phase 0 Spike A2.1 — mmap COW snapshot/restore.
//!
//! Spike A2 used `Memory::data_mut().copy_from_slice(&snapshot)` per call:
//! a 10 MB memcpy at ~1 ms each. That's the floor for a memcpy-based
//! implementation — bandwidth-bound, doesn't scale across cores.
//!
//! This spike replaces the memcpy with **mmap MAP_PRIVATE of a snapshot
//! file**. The kernel page cache is shared across instances; reads page-
//! fault into the same physical pages; writes COW into private pages.
//! Per-call cost should drop from "memcpy 10 MB" to "mmap syscall + page
//! faults on the few pages Python actually touches" — ~100 KB instead of
//! 10 MB worth of work, lazily.
//!
//! Expected headline: per-call cost <100 µs.

use anyhow::{anyhow, Result};
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
const SNAPSHOT_FILE: &str = "/tmp/wisp-snapshot.bin";

const ITERATIONS: usize = 200;
const WARMUP: usize = 20;

const PAGE_SIZE: usize = 65536; // wasm page = 64 KiB
const HOST_PAGE: usize = 4096;  // typical OS page

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
// CowMemoryCreator: hand wasmtime a custom LinearMemory that mmaps the
// snapshot file MAP_PRIVATE. Pages read from kernel page cache (shared);
// writes COW to private pages.
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
        // Total virtual reservation. If wasmtime asked for a specific size,
        // honor it; otherwise reserve enough for max (or just minimum).
        let usable = reserved_size_in_bytes
            .unwrap_or_else(|| maximum.unwrap_or(minimum));
        let total = usable + guard_size_in_bytes;

        // 1. Reserve total virtual region with PROT_NONE.
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

        // 2. For the prefix that overlaps the snapshot: mmap MAP_PRIVATE
        //    of the snapshot file at offset 0. Reads share page cache; writes COW.
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

        // 3. For [snap_in_min, minimum): zero-filled anon pages.
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
        let added = new_size - old;

        // For [old, min(new_size, snapshot_len)): map snapshot file
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

        // For [max(old, snap_end), new_size): zero anon pages
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
        let _ = added;
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
// percentile helpers
// =====================================================================
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}
fn report(name: &str, raw: &[f64]) {
    let mut s = raw.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&s, 50.0);
    let p99 = percentile(&s, 99.0);
    let mean = raw.iter().sum::<f64>() / raw.len() as f64;
    println!("| {:<26} | {:>9.3} | {:>9.3} | {:>10.3} |", name, p50, p99, mean);
}

// =====================================================================
// main: capture snapshot once via the standard allocator, write to file,
// then switch to CowMemoryCreator for the per-call benchmark.
// =====================================================================

fn capture_snapshot(host_root: &str, python_wasm: &PathBuf) -> Result<(Vec<u8>, u64)> {
    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, python_wasm)?;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    // Stub the wisp_entry host bridge import. This bench never calls
    // _wisp.call_host from Python, so a no-op that returns -1 (host error)
    // is enough to satisfy the import resolver.
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

    println!("# Wisp WASI Python — mmap COW snapshot — Spike A2.1");
    println!("python-reactor.wasm: {}", python_wasm.display());
    println!("snapshot file:       {}", SNAPSHOT_FILE);
    println!();

    // ---- 1) capture snapshot via standard allocator -----------------------
    print!("Capturing snapshot... ");
    let t = Instant::now();
    let (snapshot, pages) = capture_snapshot(&host_root, &python_wasm)?;
    println!("{} bytes ({} wasm pages) in {:.0} ms",
        snapshot.len(), pages, t.elapsed().as_secs_f64() * 1000.0);

    // ---- 2) write snapshot to a file the COW mmap will reference ----------
    std::fs::write(SNAPSHOT_FILE, &snapshot)?;
    let snap_file = std::fs::OpenOptions::new()
        .read(true)
        .open(SNAPSHOT_FILE)?;
    let snap_fd = snap_file.into_raw_fd();
    println!("Snapshot file: {} bytes\n", snapshot.len());

    let creator = Arc::new(CowMemoryCreator {
        snapshot_fd: snap_fd,
        snapshot_len: snapshot.len(),
    });

    // ---- 3) build COW-backed engine + module ------------------------------
    let mut config = Config::new();
    config.async_support(false);
    config.with_host_memory(creator);
    config.memory_init_cow(false);  // we handle COW ourselves
    let engine = Engine::new(&config)?;
    let t = Instant::now();
    let module = Module::from_file(&engine, &python_wasm)?;
    println!("Module compile (COW engine): {:.0} ms\n",
        t.elapsed().as_secs_f64() * 1000.0);

    let snapshot_pages = pages;

    // ---- 4) warmup --------------------------------------------------------
    for _ in 0..WARMUP {
        run_one(&engine, &module, snapshot_pages, &host_root, 0)?;
    }

    // ---- 5) benchmark -----------------------------------------------------
    println!("## COW snapshot — per-call benchmark ({} iters)\n", ITERATIONS);

    let mut t_inst:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_grow:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_alloc: Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_eval:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_total: Vec<f64> = Vec::with_capacity(ITERATIONS);

    let mut t_madv: Vec<f64> = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS {
        let total_t = Instant::now();
        let inst_t = Instant::now();
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    // Stub the wisp_entry host bridge import. This bench never calls
    // _wisp.call_host from Python, so a no-op that returns -1 (host error)
    // is enough to satisfy the import resolver.
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
        let wasi = make_wasi(&host_root)?;
        let mut store = Store::new(&engine, wasi);
        let inst = linker.instantiate(&mut store, &module)?;
        // Wasmtime's instantiate ran data-segment init and clobbered our
        // snapshot's .data pages. madvise MADV_DONTNEED drops those private
        // (COW) pages, restoring the file-backed view → next read re-faults
        // against the snapshot's post-wisp_init bytes.
        let mem = inst.get_memory(&mut store, "memory").unwrap();
        t_inst.push(inst_t.elapsed().as_secs_f64() * 1000.0);

        let madv_t = Instant::now();
        let base = mem.data_ptr(&mut store);
        // macOS MADV_DONTNEED zeroes pages instead of restoring file view.
        // Re-mmap MAP_PRIVATE | MAP_FIXED of the snapshot to undo wasmtime's
        // data-init writes and restore the COW-from-file mapping.
        let r = unsafe {
            libc::mmap(
                base as *mut libc::c_void,
                snapshot.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                snap_fd,
                0,
            )
        };
        if r == libc::MAP_FAILED {
            return Err(anyhow!("re-mmap snapshot failed: {}",
                std::io::Error::last_os_error()));
        }
        t_madv.push(madv_t.elapsed().as_secs_f64() * 1000.0);

        let grow_t = Instant::now();
        let cur = mem.size(&store);
        if cur < snapshot_pages {
            mem.grow(&mut store, snapshot_pages - cur)?;
        }
        t_grow.push(grow_t.elapsed().as_secs_f64() * 1000.0);

        let alloc_t = Instant::now();
        let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
        let code = format!(
            "json.dumps({{'i': {i}, 'pi': math.pi}})\n",
        );
        let bytes = code.as_bytes();
        let ptr = alloc.call(&mut store, bytes.len() as i32)?;
        mem.data_mut(&mut store)[ptr as usize..ptr as usize + bytes.len()]
            .copy_from_slice(bytes);
        t_alloc.push(alloc_t.elapsed().as_secs_f64() * 1000.0);

        let eval_t = Instant::now();
        let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
        let rc = eval.call(&mut store, (ptr, bytes.len() as i32))?;
        if rc != 0 {
            return Err(anyhow!("wisp_eval rc={rc}"));
        }
        t_eval.push(eval_t.elapsed().as_secs_f64() * 1000.0);

        t_total.push(total_t.elapsed().as_secs_f64() * 1000.0);
    }

    println!("| Phase                      |   p50 ms  |   p99 ms  |   mean ms  |");
    println!("|----------------------------|-----------|-----------|------------|");
    report("Instantiate + data init",   &t_inst);
    report("madvise(DONTNEED) reset",   &t_madv);
    report("Grow memory (mmap)",        &t_grow);
    report("Alloc + write code",        &t_alloc);
    report("wisp_eval",                 &t_eval);
    report("Total",                     &t_total);

    println!();
    println!("## Comparison");
    println!();
    println!("| Approach                                       | Total cold start p50 |");
    println!("|------------------------------------------------|----------------------|");
    println!("| Subprocess wasmtime CLI (M0 baseline)          |    ~400 ms           |");
    println!("| In-process pooled, fresh interpreter (Spike A) |     39.39 ms         |");
    println!("| memcpy snapshot/restore (Spike A2)             |      1.68 ms         |");
    {
        let mut s = t_total.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = percentile(&s, 50.0);
        println!("| mmap COW snapshot (this spike)                 |      {p:>5.3} ms        |");
    }

    let _ = HOST_PAGE; let _ = PAGE_SIZE;  // silence unused
    Ok(())
}

fn run_one(
    engine: &Engine,
    module: &Module,
    snapshot_pages: u64,
    host_root: &str,
    seed: usize,
) -> Result<()> {
    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    // Stub the wisp_entry host bridge import. This bench never calls
    // _wisp.call_host from Python, so a no-op that returns -1 (host error)
    // is enough to satisfy the import resolver.
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
    let wasi = make_wasi(host_root)?;
    let mut store = Store::new(engine, wasi);
    let inst = linker.instantiate(&mut store, module)?;
    let mem = inst.get_memory(&mut store, "memory").unwrap();
    // Drop wasmtime's data-init writes; restore file-backed COW view
    let snap_len = (snapshot_pages as usize) * 65536;
    unsafe {
        libc::madvise(mem.data_ptr(&mut store) as *mut libc::c_void,
                      snap_len, libc::MADV_DONTNEED);
    }
    let cur = mem.size(&store);
    if cur < snapshot_pages {
        mem.grow(&mut store, snapshot_pages - cur)?;
    }
    let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
    let code = format!("json.dumps({{'s': {seed}}})\n");
    let bytes = code.as_bytes();
    let ptr = alloc.call(&mut store, bytes.len() as i32)?;
    mem.data_mut(&mut store)[ptr as usize..ptr as usize + bytes.len()]
        .copy_from_slice(bytes);
    let _ = eval.call(&mut store, (ptr, bytes.len() as i32))?;
    Ok(())
}

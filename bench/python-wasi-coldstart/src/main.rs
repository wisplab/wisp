//! Phase 0 Spike A — WASI Python cold-start with module pooling.
//!
//! Validates the per-call sandbox primitive on a real Python.wasm:
//! Engine + Module compiled once and cached; per-call we create a fresh
//! Store + Instance with copy-on-write memory init, then run `_start`.
//!
//! Compares two configs:
//!   on-demand allocator   — baseline; Wasmtime's default per-instance setup
//!   pooling allocator     — pre-allocated instance pool + memory_init_cow
//!
//! Per-call decomposition: store create / linker setup / instantiate / run.

use anyhow::Result;
use std::path::PathBuf;
use std::time::Instant;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const ITERATIONS: usize = 200;
const WARMUP: usize = 20;

const PYTHON_WASM_ENV: &str = "PYTHON_WASM";
const DEFAULT_PYTHON_WASM: &str =
    "../../runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python.wasm";
const CPYTHON_HOST_ROOT_ENV: &str = "CPYTHON_HOST_ROOT";
const DEFAULT_CPYTHON_HOST_ROOT: &str = "../../runtime/cpython-wasi/vendor/cpython";
const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(name: &str, sorted: &[f64], raw: &[f64]) {
    let p50 = percentile(sorted, 50.0);
    let p99 = percentile(sorted, 99.0);
    let mean = raw.iter().sum::<f64>() / raw.len() as f64;
    println!("| {:<22} | {:>9.2} | {:>9.2} | {:>10.2} |", name, p50, p99, mean);
}

#[derive(Default)]
struct Phase {
    raw: Vec<f64>,
}
impl Phase {
    fn record(&mut self, t: Instant) {
        self.raw.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    fn sorted(&self) -> Vec<f64> {
        let mut s = self.raw.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s
    }
}

struct PerCall {
    store: Phase,
    linker: Phase,
    instantiate: Phase,
    run: Phase,
    total: Phase,
}
impl PerCall {
    fn new(cap: usize) -> Self {
        Self {
            store: Phase { raw: Vec::with_capacity(cap) },
            linker: Phase { raw: Vec::with_capacity(cap) },
            instantiate: Phase { raw: Vec::with_capacity(cap) },
            run: Phase { raw: Vec::with_capacity(cap) },
            total: Phase { raw: Vec::with_capacity(cap) },
        }
    }
    fn dump(&self, label: &str) {
        println!("\n## {label}\n");
        println!("| Phase                  |   p50 ms  |   p99 ms  |   mean ms  |");
        println!("|------------------------|-----------|-----------|------------|");
        report("Store create",     &self.store.sorted(),       &self.store.raw);
        report("Linker setup",     &self.linker.sorted(),      &self.linker.raw);
        report("Instantiate",      &self.instantiate.sorted(), &self.instantiate.raw);
        report("Run _start",       &self.run.sorted(),         &self.run.raw);
        report("Total",            &self.total.sorted(),       &self.total.raw);
    }
}

fn make_wasi(host_root: &str, code: &str) -> Result<WasiP1Ctx> {
    let mut b = WasiCtxBuilder::new();
    b.inherit_stdio()
        .args(&["python", "-c", code])
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .preopened_dir(host_root, "/", DirPerms::READ, FilePerms::READ)?;
    Ok(b.build_p1())
}

fn run_loop(
    label: &str,
    config: Config,
    python_wasm_path: &PathBuf,
    host_root: &str,
    code: &str,
) -> Result<()> {
    let t = Instant::now();
    let engine = Engine::new(&config)?;
    let engine_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let module = Module::from_file(&engine, python_wasm_path)?;
    let module_ms = t.elapsed().as_secs_f64() * 1000.0;

    println!("\n=== {label} ===");
    println!("Engine creation:    {:.2} ms", engine_ms);
    println!("Module compile:     {:.2} ms", module_ms);
    println!("Iterations:         {ITERATIONS} (after {WARMUP} warmup)");
    println!("Python script:      {code:?}");

    // Warmup
    for _ in 0..WARMUP {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
        linker.func_wrap("env", "host_call",
            |_c: wasmtime::Caller<'_, WasiP1Ctx>,
             _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
        let wasi = make_wasi(host_root, code)?;
        let mut store = Store::new(&engine, wasi);
        let inst = linker.instantiate(&mut store, &module)?;
        let start = inst.get_typed_func::<(), ()>(&mut store, "_start")?;
        let _ = start.call(&mut store, ());
    }

    let mut p = PerCall::new(ITERATIONS);
    for _ in 0..ITERATIONS {
        let total_t = Instant::now();

        let s_t = Instant::now();
        let wasi = make_wasi(host_root, code)?;
        let mut store = Store::new(&engine, wasi);
        p.store.record(s_t);

        let l_t = Instant::now();
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
        linker.func_wrap("env", "host_call",
            |_c: wasmtime::Caller<'_, WasiP1Ctx>,
             _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
        p.linker.record(l_t);

        let i_t = Instant::now();
        let inst = linker.instantiate(&mut store, &module)?;
        p.instantiate.record(i_t);

        let r_t = Instant::now();
        let start = inst.get_typed_func::<(), ()>(&mut store, "_start")?;
        let _ = start.call(&mut store, ());
        p.run.record(r_t);

        p.total.record(total_t);
    }

    p.dump(label);
    Ok(())
}

fn main() -> Result<()> {
    let python_wasm: PathBuf = std::env::var(PYTHON_WASM_ENV)
        .unwrap_or_else(|_| DEFAULT_PYTHON_WASM.to_string())
        .into();
    let host_root = std::env::var(CPYTHON_HOST_ROOT_ENV)
        .unwrap_or_else(|_| DEFAULT_CPYTHON_HOST_ROOT.to_string());

    let py_size = std::fs::metadata(&python_wasm)?.len();
    println!("# Wisp WASI Python cold-start — Phase 0 Spike A");
    println!("python.wasm:        {} ({} MB)", python_wasm.display(), py_size / 1024 / 1024);
    println!("host root:          {host_root}");

    // Test scripts: trivial → noticeable Python work
    let trivial = "pass";
    let with_imports = "import json, re, math; json.dumps({'pi': math.pi})";

    // 1. On-demand allocator (Wasmtime default)
    {
        let mut config = Config::new();
        config.async_support(false);
        run_loop("on-demand · pass", config, &python_wasm, &host_root, trivial)?;
    }

    // 2. On-demand + the import workload
    {
        let mut config = Config::new();
        config.async_support(false);
        run_loop("on-demand · imports", config, &python_wasm, &host_root, with_imports)?;
    }

    // 3. Pooling allocator + memory_init_cow
    {
        let mut config = Config::new();
        config.async_support(false);
        let mut pool = PoolingAllocationConfig::default();
        pool.total_memories(64)
            .max_memory_size(512 * 1024 * 1024)
            .total_tables(64)
            .total_core_instances(64)
            .total_stacks(64);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        config.memory_init_cow(true);
        run_loop("pooling+CoW · pass", config, &python_wasm, &host_root, trivial)?;
    }

    // 4. Pooling allocator + memory_init_cow + imports
    {
        let mut config = Config::new();
        config.async_support(false);
        let mut pool = PoolingAllocationConfig::default();
        pool.total_memories(64)
            .max_memory_size(512 * 1024 * 1024)
            .total_tables(64)
            .total_core_instances(64)
            .total_stacks(64);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        config.memory_init_cow(true);
        run_loop("pooling+CoW · imports", config, &python_wasm, &host_root, with_imports)?;
    }

    println!("\n## Notes\n");
    println!("- 'pass' isolates Python interpreter init time (no user code work).");
    println!("- 'imports' adds json+re+math import + json.dumps round trip.");
    println!("- Pooling+CoW shares the .data section across instances via copy-on-write.");
    println!("- Run `_start` time dominates because Python interpreter init runs each call.");
    println!("- A future spike can pre-init Python and snapshot linear memory to skip that.");

    Ok(())
}

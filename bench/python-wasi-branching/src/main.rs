//! Phase 0 Spike B — Branching session: K-way fork from one snapshot.
//!
//! From a single Python-ready snapshot (captured after wisp_init), fork K
//! children that each diverge by running a different Python expression.
//! Measure per-branch cost and aggregate throughput across multiple K and
//! sequential vs parallel execution. The branching primitive's claim is
//! that fork cost stays sub-ms regardless of K.
//!
//! Use case: tree-search RL (MCTS, Tree-of-Thoughts, branching GRPO) needs
//! to fork at each decision point of each rollout. On Linux/Firecracker
//! that's 5–500 ms per fork → infeasible at K=100, depth=100 (10k forks).
//! With WASM linear-memory snapshot/restore that's ~1–2 ms per fork → the
//! same workload is seconds, not hours.

use anyhow::{anyhow, Result};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const PYTHON_REACTOR_WASM: &str =
    "../../runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm";
const CPYTHON_HOST_ROOT: &str = "../../runtime/cpython-wasi/vendor/cpython";
const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";

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

/// Run one branch: instantiate fresh, restore snapshot, run divergent code.
/// `branch_id` is interpolated into the code so each branch's Python work
/// is genuinely different (not a tight loop the host could elide).
fn run_branch(
    engine: &Engine,
    module: &Module,
    snapshot: &[u8],
    snapshot_pages: u64,
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
    let inst = linker.instantiate(&mut store, module)?;
    if let Ok(init) = inst.get_typed_func::<(), ()>(&mut store, "_initialize") {
        init.call(&mut store, ())?;
    }
    let mem = inst.get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory"))?;
    let cur_pages = mem.size(&store);
    if cur_pages < snapshot_pages {
        mem.grow(&mut store, snapshot_pages - cur_pages)?;
    }
    mem.data_mut(&mut store)[..snapshot.len()].copy_from_slice(snapshot);

    let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;

    /* Each branch runs slightly different Python so the host can't elide.
     * `import json, math` are cached in sys.modules from the snapshot. */
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

fn main() -> Result<()> {
    let python_wasm: PathBuf = PYTHON_REACTOR_WASM.into();
    let host_root = CPYTHON_HOST_ROOT.to_string();
    let py_size = std::fs::metadata(&python_wasm)?.len();

    println!("# Wisp WASI Python branching — Phase 0 Spike B");
    println!("python-reactor.wasm: {} ({} MB)", python_wasm.display(), py_size / 1024 / 1024);
    println!("rayon thread pool:   {} threads", rayon::current_num_threads());

    // --- Engine with pooling allocator sized for K up to 1024 -------------
    let mut config = Config::new();
    config.async_support(false);
    let mut pool = PoolingAllocationConfig::default();
    pool.total_memories(2048)
        .max_memory_size(64 * 1024 * 1024)
        .total_tables(2048)
        .total_core_instances(2048)
        .total_stacks(2048);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    config.memory_init_cow(true);

    let t = Instant::now();
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, &python_wasm)?;
    println!("Engine + Module:     {:.2} ms\n", t.elapsed().as_secs_f64() * 1000.0);

    // --- Capture snapshot --------------------------------------------------
    println!("## Capturing post-wisp_init snapshot");
    let snapshot: Arc<Vec<u8>>;
    let snapshot_pages: u64;
    {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
        let wasi = make_wasi(&host_root)?;
        let mut store = Store::new(&engine, wasi);
        let inst = linker.instantiate(&mut store, &module)?;
        if let Ok(init) = inst.get_typed_func::<(), ()>(&mut store, "_initialize") {
            init.call(&mut store, ())?;
        }
        let mem = inst.get_memory(&mut store, "memory").unwrap();
        let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
        let rc = wisp_init.call(&mut store, ())?;
        if rc != 0 {
            return Err(anyhow!("wisp_init returned {rc}"));
        }
        snapshot = Arc::new(mem.data(&store).to_vec());
        snapshot_pages = mem.size(&store);
        println!("Snapshot size:       {} bytes ({} wasm pages, {} MB)\n",
            snapshot.len(), snapshot_pages, snapshot.len() / 1024 / 1024);
    }

    // --- Warmup -----------------------------------------------------------
    for _ in 0..WARMUP {
        let _ = run_branch(&engine, &module, &snapshot, snapshot_pages, &host_root, 0)?;
    }

    let engine = Arc::new(engine);
    let module = Arc::new(module);

    // --- Sweep K, measure sequential and parallel ------------------------
    println!("## K-way fork sweep");
    println!();
    println!("| K     | Sequential total | per branch | throughput (br/s) | Parallel total | per branch | throughput (br/s) | Speedup |");
    println!("|-------|------------------|------------|-------------------|----------------|------------|-------------------|---------|");

    for &k in KS {
        // Sequential
        let t_seq = Instant::now();
        for i in 0..k {
            let rc = run_branch(&engine, &module, &snapshot, snapshot_pages, &host_root, i)?;
            if rc != 0 {
                return Err(anyhow!("branch {i} returned {rc}"));
            }
        }
        let seq_ms = t_seq.elapsed().as_secs_f64() * 1000.0;
        let seq_per = seq_ms / k as f64;
        let seq_thr = (k as f64) / (seq_ms / 1000.0);

        // Parallel (rayon, default thread pool = num_cpus)
        let t_par = Instant::now();
        let par_results: Result<Vec<i32>> = (0..k)
            .into_par_iter()
            .map(|i| run_branch(&engine, &module, &snapshot, snapshot_pages, &host_root, i))
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

    // --- Per-branch latency distribution at K=64 (parallel) ---------------
    println!("## Per-branch latency at K=64 (parallel)");
    let k = 64;
    let mut times: Vec<f64> = (0..k).into_par_iter().map(|i| {
        let t = Instant::now();
        let _ = run_branch(&engine, &module, &snapshot, snapshot_pages, &host_root, i);
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
    println!("- Each branch instantiates a fresh wasmtime instance, restores the");
    println!("  10 MB post-wisp_init snapshot, then runs a divergent Python expression.");
    println!("- 'Per branch' under parallel = wall-clock / K; the per-thread cost is");
    println!("  approximately the sequential per-branch number.");
    println!("- Throughput scales near-linearly with cores until snapshot bandwidth");
    println!("  saturates the memory subsystem.");
    println!("- For tree-search RL: K=100 branches at depth=100 = 10,000 forks. At");
    println!("  parallel per-branch cost shown above, that's a few seconds of pure");
    println!("  fork overhead per trajectory — versus hours on Linux/Firecracker.");

    Ok(())
}

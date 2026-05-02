//! Phase 0 Spike A2 — WASI Python per-call sandbox via linear-memory snapshot/restore.
//!
//! Boots python-reactor.wasm once, calls wisp_init() to initialize the
//! Python interpreter, captures the linear memory bytes as a snapshot.
//! Then for N iterations: instantiate fresh, memcpy the snapshot back,
//! call wisp_eval(code) directly. Measures the per-call cost of running
//! a fresh-state Python — without paying interpreter init each time.
//!
//! Headline question: does this beat Spike A's 39 ms p50 by enough to be
//! worth the architectural complexity?

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Instant;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const ITERATIONS: usize = 200;
const WARMUP: usize = 10;

const PYTHON_REACTOR_WASM: &str =
    "../../runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm";
const CPYTHON_HOST_ROOT: &str = "../../runtime/cpython-wasi/vendor/cpython";
const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";

fn make_wasi(host_root: &str) -> Result<WasiP1Ctx> {
    let mut b = WasiCtxBuilder::new();
    b.inherit_stdio()
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(host_root, "/", DirPerms::READ, FilePerms::READ)?;
    Ok(b.build_p1())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(name: &str, raw: &[f64]) {
    let mut sorted = raw.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&sorted, 50.0);
    let p99 = percentile(&sorted, 99.0);
    let mean = raw.iter().sum::<f64>() / raw.len() as f64;
    println!("| {:<26} | {:>9.2} | {:>9.2} | {:>10.2} |", name, p50, p99, mean);
}

fn fresh_instance(
    engine: &Engine,
    module: &Module,
    host_root: &str,
) -> Result<(Store<WasiP1Ctx>, Instance, Memory)> {
    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap("env", "host_call",
        |_c: wasmtime::Caller<'_, WasiP1Ctx>,
         _np: i32, _nl: i32, _pp: i32, _pl: i32, _rp: i32, _rm: i32| -> i32 { -1 })?;
    let wasi = make_wasi(host_root)?;
    let mut store = Store::new(engine, wasi);

    // Reactor instantiate runs `_initialize` automatically as part of
    // wasmtime's WASI command/reactor handling — but we have to invoke
    // it ourselves since we're using Linker::instantiate (not Wasi cmd).
    let inst = linker.instantiate(&mut store, module)?;
    if let Ok(init) = inst.get_typed_func::<(), ()>(&mut store, "_initialize") {
        init.call(&mut store, ())?;
    }
    let mem = inst.get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("module has no exported `memory`"))?;
    Ok((store, inst, mem))
}

fn main() -> Result<()> {
    let python_wasm: PathBuf = PYTHON_REACTOR_WASM.into();
    let host_root = CPYTHON_HOST_ROOT.to_string();
    let py_size = std::fs::metadata(&python_wasm)?.len();

    println!("# Wisp WASI Python snapshot/restore — Phase 0 Spike A2");
    println!("python-reactor.wasm: {} ({} MB)", python_wasm.display(), py_size / 1024 / 1024);
    println!("host root:           {host_root}");

    // ---------- one-time setup ----------------------------------------------
    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let t = Instant::now();
    let module = Module::from_file(&engine, &python_wasm)?;
    println!("Module compile:      {:.2} ms\n", t.elapsed().as_secs_f64() * 1000.0);

    // ---------- Phase 1: smoke test -----------------------------------------
    println!("## Smoke test");
    {
        let (mut store, inst, _mem) = fresh_instance(&engine, &module, &host_root)?;
        let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
        let t = Instant::now();
        let rc = wisp_init.call(&mut store, ())?;
        println!("wisp_init() returned {rc} in {:.2} ms (cold Python init)",
            t.elapsed().as_secs_f64() * 1000.0);

        let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
        let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;

        let code = b"print('hi from snapshotted Python')";
        let mem  = inst.get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("no memory"))?;
        let ptr = alloc.call(&mut store, code.len() as i32)?;
        mem.data_mut(&mut store)[ptr as usize..ptr as usize + code.len()]
            .copy_from_slice(code);
        let t = Instant::now();
        let rc = eval.call(&mut store, (ptr, code.len() as i32))?;
        println!("wisp_eval('print(...)') returned {rc} in {:.2} ms",
            t.elapsed().as_secs_f64() * 1000.0);
    }

    // ---------- Phase 2: capture snapshot of post-wisp_init memory ----------
    println!("\n## Capturing snapshot of post-wisp_init linear memory");
    let snapshot: Vec<u8>;
    let snapshot_pages: u64;
    {
        let (mut store, inst, mem) = fresh_instance(&engine, &module, &host_root)?;
        let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
        wisp_init.call(&mut store, ())?;
        snapshot = mem.data(&store).to_vec();
        snapshot_pages = mem.size(&store);
        println!("Snapshot size:       {} bytes ({} wasm pages, {} MB)",
            snapshot.len(), snapshot_pages, snapshot.len() / 1024 / 1024);
    }

    // ---------- Phase 3: snapshot/restore benchmark -------------------------
    println!("\n## Snapshot/restore per-call benchmark");
    println!("Iterations:          {ITERATIONS} (after {WARMUP} warmup)");

    let code_str = b"json.dumps({'pi': math.pi})";
    let preamble = b"import json, math\n";

    let mut t_inst:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_grow:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_copy:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_alloc: Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_eval:  Vec<f64> = Vec::with_capacity(ITERATIONS);
    let mut t_total: Vec<f64> = Vec::with_capacity(ITERATIONS);

    let bench_one = |store_out: &mut Option<Store<WasiP1Ctx>>,
                     buckets: Option<(&mut Vec<f64>, &mut Vec<f64>, &mut Vec<f64>,
                                       &mut Vec<f64>, &mut Vec<f64>, &mut Vec<f64>)>|
                     -> Result<i32> {
        let total_t = Instant::now();

        // 1. instantiate (incl. _initialize) — fresh Wasmtime instance
        let inst_t = Instant::now();
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
        let inst_ms = inst_t.elapsed().as_secs_f64() * 1000.0;

        // 2. grow memory to match snapshot size
        let grow_t = Instant::now();
        let cur_pages = mem.size(&store);
        if cur_pages < snapshot_pages {
            mem.grow(&mut store, snapshot_pages - cur_pages)?;
        }
        let grow_ms = grow_t.elapsed().as_secs_f64() * 1000.0;

        // 3. memcpy snapshot into linear memory  (the per-call sandbox primitive)
        let copy_t = Instant::now();
        mem.data_mut(&mut store)[..snapshot.len()].copy_from_slice(&snapshot);
        let copy_ms = copy_t.elapsed().as_secs_f64() * 1000.0;

        // 4. allocate buffer + write code
        let alloc_t = Instant::now();
        let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
        let total_len = (preamble.len() + code_str.len()) as i32;
        let ptr = alloc.call(&mut store, total_len)?;
        let mut buf = Vec::with_capacity(total_len as usize);
        buf.extend_from_slice(preamble);
        buf.extend_from_slice(code_str);
        mem.data_mut(&mut store)[ptr as usize..ptr as usize + buf.len()]
            .copy_from_slice(&buf);
        let alloc_ms = alloc_t.elapsed().as_secs_f64() * 1000.0;

        // 5. call wisp_eval
        let eval_t = Instant::now();
        let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
        let rc = eval.call(&mut store, (ptr, total_len))?;
        let eval_ms = eval_t.elapsed().as_secs_f64() * 1000.0;

        let total_ms = total_t.elapsed().as_secs_f64() * 1000.0;

        if let Some((a,b,c,d,e,f)) = buckets {
            a.push(inst_ms); b.push(grow_ms); c.push(copy_ms);
            d.push(alloc_ms); e.push(eval_ms); f.push(total_ms);
        }
        *store_out = Some(store);
        Ok(rc)
    };

    // warmup
    for _ in 0..WARMUP {
        let mut sink = None;
        let _ = bench_one(&mut sink, None)?;
    }

    // measured
    for _ in 0..ITERATIONS {
        let mut sink = None;
        let rc = bench_one(&mut sink,
            Some((&mut t_inst, &mut t_grow, &mut t_copy,
                  &mut t_alloc, &mut t_eval, &mut t_total)))?;
        if rc != 0 {
            return Err(anyhow!("wisp_eval returned non-zero rc={rc}"));
        }
    }

    println!();
    println!("| Phase                      |   p50 ms  |   p99 ms  |   mean ms  |");
    println!("|----------------------------|-----------|-----------|------------|");
    report("Instantiate + _initialize", &t_inst);
    report("Grow memory to snapshot",   &t_grow);
    report("memcpy snapshot",           &t_copy);
    report("Alloc + write code",        &t_alloc);
    report("wisp_eval",                 &t_eval);
    report("Total",                     &t_total);

    println!();
    println!("## Comparison");
    println!();
    println!("| Approach                                       | Total cold start p50 |");
    println!("|------------------------------------------------|----------------------|");
    println!("| Subprocess wasmtime CLI (M0 baseline)          |    ~400 ms          |");
    println!("| In-process pooled, fresh interpreter (Spike A) |     39.39 ms        |");
    {
        let mut s = t_total.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = percentile(&s, 50.0);
        println!("| Snapshot/restore per call (this spike)         |     {p:>5.2} ms        |");
    }

    Ok(())
}

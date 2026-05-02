//! Phase 0 Spike 1 — Wasmtime cold-start floor measurement.
//!
//! Establishes the *baseline* per-instance startup cost in Wasmtime, with the
//! WASM module pre-compiled and the engine pre-warmed. This is the lower
//! bound any Python-on-Wasmtime approach must add to.
//!
//! It does NOT yet load Pyodide. Pyodide's WASM is emscripten-targeted (expects
//! a JS host) and cannot drop directly into Wasmtime; see NOTES.md for the
//! research gap. This binary measures the empty-instance floor.

use anyhow::Result;
use std::time::Instant;
use wasmtime::{Config, Engine, Linker, Module, Store};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::WasiCtxBuilder;

const ITERATIONS: usize = 1000;

/// Minimal WAT module that exports a `noop` function.
/// We compile this once and measure instance creation + a single call.
const NOOP_WAT: &str = r#"
(module
  (func (export "noop") (result i32) i32.const 42)
)
"#;

fn percentile(sorted_us: &[f64], p: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() as f64) * p / 100.0) as usize;
    sorted_us[idx.min(sorted_us.len() - 1)]
}

fn main() -> Result<()> {
    println!("# Wisp WASM cold-start floor — Phase 0 Spike 1");
    println!();

    // ---- engine + module pre-warm (one-time) -------------------------------
    let t0 = Instant::now();
    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let engine_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t0 = Instant::now();
    let module = Module::new(&engine, NOOP_WAT)?;
    let module_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!("Pre-warm:");
    println!("  Engine creation:    {:.3} ms", engine_ms);
    println!("  Module compilation: {:.3} ms (noop WAT)", module_ms);
    println!();

    // ---- benchmark loop ----------------------------------------------------
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;

    // warm-up
    for _ in 0..50 {
        let wasi = WasiCtxBuilder::new().build_p1();
        let mut store = Store::new(&engine, wasi);
        let inst = linker.instantiate(&mut store, &module)?;
        let f = inst.get_typed_func::<(), i32>(&mut store, "noop")?;
        let _ = f.call(&mut store, ())?;
    }

    let mut samples_us = Vec::with_capacity(ITERATIONS);
    let mut store_us = Vec::with_capacity(ITERATIONS);
    let mut inst_us = Vec::with_capacity(ITERATIONS);
    let mut call_us = Vec::with_capacity(ITERATIONS);

    for _ in 0..ITERATIONS {
        let total_t0 = Instant::now();

        // Phase 1: build Store (per-tenant context, holds WASI state)
        let s0 = Instant::now();
        let wasi = WasiCtxBuilder::new().build_p1();
        let mut store = Store::new(&engine, wasi);
        store_us.push(s0.elapsed().as_secs_f64() * 1e6);

        // Phase 2: instantiate module
        let i0 = Instant::now();
        let inst = linker.instantiate(&mut store, &module)?;
        inst_us.push(i0.elapsed().as_secs_f64() * 1e6);

        // Phase 3: call exported function
        let c0 = Instant::now();
        let f = inst.get_typed_func::<(), i32>(&mut store, "noop")?;
        let _ = f.call(&mut store, ())?;
        call_us.push(c0.elapsed().as_secs_f64() * 1e6);

        samples_us.push(total_t0.elapsed().as_secs_f64() * 1e6);
    }

    // ---- report ------------------------------------------------------------
    let mut sorted = samples_us.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut sorted_store = store_us.clone();
    sorted_store.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut sorted_inst = inst_us.clone();
    sorted_inst.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut sorted_call = call_us.clone();
    sorted_call.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("Per-call decomposition (N={} iterations, post-warmup):", ITERATIONS);
    println!();
    println!("| Phase           | p50 (μs) | p99 (μs) | mean (μs) |");
    println!("|---|---|---|---|");
    let report = |name: &str, sorted: &[f64], raw: &[f64]| {
        let p50 = percentile(sorted, 50.0);
        let p99 = percentile(sorted, 99.0);
        let mean = raw.iter().sum::<f64>() / raw.len() as f64;
        println!("| {:<15} | {:>8.2} | {:>8.2} | {:>9.2} |", name, p50, p99, mean);
    };
    report("Store create", &sorted_store, &store_us);
    report("Instantiate", &sorted_inst, &inst_us);
    report("Func call", &sorted_call, &call_us);
    report("Total", &sorted, &samples_us);
    println!();

    let total_p50 = percentile(&sorted, 50.0);
    println!("## Verdict");
    println!();
    println!("WASM empty-instance cold-start floor: **{:.2} μs p50**", total_p50);
    println!();
    println!("This is the irreducible Wasmtime per-call overhead. Anything we");
    println!("layer on top (Pyodide, custom Python-WASI build, numpy import)");
    println!("adds to this. The thesis target of <5 ms (5000 μs) p50 has");
    println!("{:.0}× headroom over this floor.", 5000.0 / total_p50);

    Ok(())
}

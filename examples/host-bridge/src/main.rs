//! Host bridge demo: Python sandbox calls into a host-provided capability
//! via `_wisp.call_host(name, payload)`.
//!
//! Why this pattern: WASI Preview 1 has no sockets / subprocess / dlopen.
//! Sandboxed Python that needs an outside resource (HTTP fetch, KV lookup,
//! secret retrieval, billing usage write, etc.) cannot do it directly --
//! and shouldn't, because the sandbox is supposed to be capability-bound.
//! Instead the host implements the WASM import `env::host_call` and
//! decides which `name`s are exposed. Sandbox cannot enumerate, cannot
//! bypass.
//!
//! This example exposes two capabilities:
//!   - "echo"   : returns the payload as-is (debugging / latency probe)
//!   - "kv_get" : returns a value for a hardcoded set of keys
//!
//! In a real deployment, "fetch" would call out via reqwest, "kv_get"
//! would hit redis, "secret_get" would hit a KMS, etc. The bridge shape
//! stays the same.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Instant;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

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

/// host_call return codes used by this demo. Negative = error.
const RC_UNKNOWN_CAPABILITY: i32 = -2;
const RC_RESPONSE_TOO_LARGE: i32 = -3;
const RC_MEMORY_READ: i32 = -101;
const RC_MEMORY_WRITE: i32 = -103;

/// Implement the host capabilities. Returns the response bytes for a known
/// name+payload, or an error code (negative) if the capability isn't
/// exposed.
fn dispatch(name: &str, payload: &[u8]) -> std::result::Result<Vec<u8>, i32> {
    match name {
        "echo" => Ok(payload.to_vec()),
        "kv_get" => {
            let key = std::str::from_utf8(payload).unwrap_or("");
            let value: &[u8] = match key {
                "user.name"  => b"huanghx",
                "env.region" => b"cn-hangzhou",
                "build.git"  => b"d54dd03+ca04898",
                _            => b"",
            };
            Ok(value.to_vec())
        }
        _ => Err(RC_UNKNOWN_CAPABILITY),
    }
}

fn main() -> Result<()> {
    let python_wasm: PathBuf = PYTHON_REACTOR_WASM.into();
    let host_root = CPYTHON_HOST_ROOT.to_string();

    println!("# Wisp host-bridge demo");
    println!("python-reactor.wasm: {}", python_wasm.display());
    println!();

    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, &python_wasm)?;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;

    // Provide the env::host_call import. Python reaches it via
    // `_wisp.call_host(name: bytes, payload: bytes) -> bytes`.
    linker.func_wrap("env", "host_call",
        |mut caller: Caller<'_, WasiP1Ctx>,
         name_ptr: i32, name_len: i32,
         payload_ptr: i32, payload_len: i32,
         result_ptr: i32, result_max: i32| -> i32 {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -100,
            };
            let mut name_buf = vec![0u8; name_len as usize];
            if mem.read(&caller, name_ptr as usize, &mut name_buf).is_err() {
                return RC_MEMORY_READ;
            }
            let mut payload_buf = vec![0u8; payload_len as usize];
            if mem.read(&caller, payload_ptr as usize, &mut payload_buf).is_err() {
                return RC_MEMORY_READ;
            }
            let name = std::str::from_utf8(&name_buf).unwrap_or("");
            match dispatch(name, &payload_buf) {
                Ok(response) => {
                    if response.len() > result_max as usize {
                        return RC_RESPONSE_TOO_LARGE;
                    }
                    if mem.write(&mut caller, result_ptr as usize, &response).is_err() {
                        return RC_MEMORY_WRITE;
                    }
                    response.len() as i32
                }
                Err(rc) => rc,
            }
        })?;

    let wasi = make_wasi(&host_root)?;
    let mut store = Store::new(&engine, wasi);
    let inst = linker.instantiate(&mut store, &module)?;

    // wisp_init brings up the Python interpreter + pre-imports _wisp.
    let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
    let t = Instant::now();
    let rc = wisp_init.call(&mut store, ())?;
    println!("wisp_init: rc={rc} ({:.0} ms)", t.elapsed().as_secs_f64() * 1000.0);
    if rc != 0 {
        return Err(anyhow!("wisp_init failed"));
    }

    let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval  = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
    let mem   = inst.get_memory(&mut store, "memory").unwrap();

    // Drive a Python script that exercises the bridge.
    let script = br#"
import _wisp, sys

# 1) Echo round-trip
echoed = _wisp.call_host(b"echo", b"hello from sandbox")
assert echoed == b"hello from sandbox", echoed
sys.stdout.write(f"echo OK: {echoed!r}\n")

# 2) Three KV lookups, including one missing key
for key in (b"user.name", b"env.region", b"build.git", b"missing.key"):
    val = _wisp.call_host(b"kv_get", key)
    sys.stdout.write(f"kv_get({key!r}) -> {val!r}\n")

# 3) Capability boundary: an unknown capability raises
try:
    _wisp.call_host(b"shell_exec", b"rm -rf /")
except RuntimeError as e:
    sys.stdout.write(f"unknown capability blocked: {e}\n")

sys.stdout.flush()
"#;

    let ptr = alloc.call(&mut store, script.len() as i32)?;
    mem.data_mut(&mut store)[ptr as usize..ptr as usize + script.len()]
        .copy_from_slice(script);
    let t = Instant::now();
    let r = eval.call(&mut store, (ptr, script.len() as i32))?;
    println!("\nwisp_eval: rc={r} ({:.2} ms)", t.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

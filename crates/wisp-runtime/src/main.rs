//! Wisp host runtime daemon.
//!
//! What this is, today (V1):
//!
//!   - Loads `python-reactor.wasm` once on startup
//!   - Captures a snapshot of post-`wisp_init` linear memory
//!   - Mounts the snapshot as a COW MemoryCreator (per Spike A2.1)
//!   - Exposes HTTP `POST /v1/eval` that, per request:
//!       1. Instantiates a fresh wasm Instance against the COW snapshot
//!       2. Re-mmaps MAP_FIXED to undo wasmtime's data-segment init writes
//!       3. Runs `wisp_eval(code)` against captured stdout/stderr
//!       4. Returns { rc, stdout, stderr, elapsed_us }
//!
//! What it ISN'T, yet:
//!   - No multi-tenancy / auth
//!   - No instance pool reuse — each request creates+destroys an Instance
//!   - No host capability bridge yet (env::host_call stubbed to -1)
//!   - No streaming responses
//!   - No CPU/memory limits per request
//!   - No metrics endpoint
//!
//! These are deliberate omissions for the platform-skeleton commit. The
//! Day-1 goal is "anyone can `cargo run` and curl localhost:9000 to run
//! Python in a fresh WASM sandbox sub-millisecond."

use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};
use wasmtime::*;
use wasmtime_wasi::pipe::MemoryOutputPipe;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";

// =====================================================================
// CowMemoryCreator — same as bench/python-wasi-cow. Mmaps the snapshot
// file MAP_PRIVATE; per-call MAP_FIXED reset undoes wasmtime's data-init
// writes. See bench/python-wasi-cow/FINDINGS.md for the architecture.
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
        let usable = reserved_size_in_bytes.unwrap_or_else(|| maximum.unwrap_or(minimum));
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
            return Err(format!(
                "mmap PROT_NONE reserve failed: {}",
                std::io::Error::last_os_error()
            ));
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
                unsafe {
                    libc::munmap(base, total);
                }
                return Err(format!(
                    "mmap MAP_PRIVATE snapshot failed: {}",
                    std::io::Error::last_os_error()
                ));
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
                unsafe {
                    libc::munmap(base, total);
                }
                return Err(format!(
                    "mmap MAP_ANON tail failed: {}",
                    std::io::Error::last_os_error()
                ));
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
    fn byte_size(&self) -> usize {
        self.cur_size
    }
    fn maximum_byte_size(&self) -> Option<usize> {
        Some(self.max_size)
    }

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
                return Err(anyhow!(
                    "grow mmap snapshot failed: {}",
                    std::io::Error::last_os_error()
                ));
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
                return Err(anyhow!(
                    "grow mmap anon failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        self.cur_size = new_size;
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 {
        self.base
    }
    fn wasm_accessible(&self) -> std::ops::Range<usize> {
        0..self.total_reserved
    }
}

impl Drop for CowMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.total_reserved);
        }
    }
}

// =====================================================================
// Engine setup: load wasm, capture snapshot, mount COW creator
// =====================================================================

struct Runtime {
    engine: Engine,
    module: Module,
    snapshot_len: usize,
    snapshot_pages: u64,
    snapshot_fd: RawFd,
    host_root: String,
}

fn capture_snapshot(host_root: &str, python_wasm: &PathBuf) -> Result<(Vec<u8>, u64)> {
    let mut config = Config::new();
    config.async_support(false);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, python_wasm)?;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap(
        "env",
        "host_call",
        |_c: Caller<'_, WasiP1Ctx>,
         _np: i32,
         _nl: i32,
         _pp: i32,
         _pl: i32,
         _rp: i32,
         _rm: i32|
         -> i32 { -1 },
    )?;

    let mut b = WasiCtxBuilder::new();
    b.inherit_stdio()
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(host_root, "/", DirPerms::READ, FilePerms::READ)?;
    let wasi = b.build_p1();
    let mut store = Store::new(&engine, wasi);
    let inst = linker.instantiate(&mut store, &module)?;
    if let Ok(init) = inst.get_typed_func::<(), ()>(&mut store, "_initialize") {
        init.call(&mut store, ())?;
    }
    let wisp_init = inst.get_typed_func::<(), i32>(&mut store, "wisp_init")?;
    let rc = wisp_init.call(&mut store, ())?;
    if rc != 0 {
        return Err(anyhow!("wisp_init returned {rc} during snapshot capture"));
    }
    let mem = inst
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export"))?;
    let bytes = mem.data(&store).to_vec();
    let pages = mem.size(&store);
    Ok((bytes, pages))
}

fn build_runtime(python_wasm: PathBuf, host_root: String, snapshot_path: PathBuf) -> Result<Runtime> {
    info!("capturing initial snapshot...");
    let t = Instant::now();
    let (snapshot, snapshot_pages) = capture_snapshot(&host_root, &python_wasm)?;
    info!(
        "snapshot captured: {} bytes ({} pages) in {:.0} ms",
        snapshot.len(),
        snapshot_pages,
        t.elapsed().as_secs_f64() * 1000.0
    );

    let snapshot_len = snapshot.len();
    std::fs::write(&snapshot_path, &snapshot)?;
    drop(snapshot);
    let snap_file = std::fs::OpenOptions::new().read(true).open(&snapshot_path)?;
    let snapshot_fd = snap_file.into_raw_fd();

    let creator = Arc::new(CowMemoryCreator {
        snapshot_fd,
        snapshot_len,
    });
    let mut config = Config::new();
    config.async_support(false);
    config.with_host_memory(creator);
    config.memory_init_cow(false);

    let engine = Engine::new(&config)?;
    let t = Instant::now();
    let module = Module::from_file(&engine, &python_wasm)?;
    info!(
        "module compiled (COW engine): {:.0} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    Ok(Runtime {
        engine,
        module,
        snapshot_len,
        snapshot_pages,
        snapshot_fd,
        host_root,
    })
}

// =====================================================================
// Per-request: instantiate, MAP_FIXED reset, run wisp_eval, return
// =====================================================================

#[derive(Debug, Deserialize)]
struct EvalRequest {
    code: String,
    #[serde(default = "default_timeout_ms")]
    #[allow(dead_code)] // timeout enforcement is a TODO; field accepted for forward-compat
    timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Serialize)]
struct EvalResponse {
    rc: i32,
    stdout: String,
    stderr: String,
    elapsed_us: u64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

fn run_one(rt: &Runtime, code: &str) -> Result<EvalResponse> {
    let t_total = Instant::now();

    let stdout = MemoryOutputPipe::new(1 << 20);
    let stderr = MemoryOutputPipe::new(1 << 20);

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&rt.engine);
    wasmtime_wasi::preview1::add_to_linker_sync(&mut linker, |s| s)?;
    linker.func_wrap(
        "env",
        "host_call",
        |_c: Caller<'_, WasiP1Ctx>,
         _np: i32,
         _nl: i32,
         _pp: i32,
         _pl: i32,
         _rp: i32,
         _rm: i32|
         -> i32 { -1 }, // capability bridge stubbed for V1
    )?;

    let mut b = WasiCtxBuilder::new();
    b.stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(&rt.host_root, "/", DirPerms::READ, FilePerms::READ)?;
    let wasi = b.build_p1();
    let mut store = Store::new(&rt.engine, wasi);
    let inst = linker.instantiate(&mut store, &rt.module)?;
    let mem = inst
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export on instance"))?;

    // MAP_FIXED reset to undo wasmtime's data-segment init writes
    let base = mem.data_ptr(&mut store);
    let r = unsafe {
        libc::mmap(
            base as *mut libc::c_void,
            rt.snapshot_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            rt.snapshot_fd,
            0,
        )
    };
    if r == libc::MAP_FAILED {
        return Err(anyhow!(
            "re-mmap snapshot failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let cur = mem.size(&store);
    if cur < rt.snapshot_pages {
        mem.grow(&mut store, rt.snapshot_pages - cur)?;
    }

    let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;

    // Wrap user code so stdout/stderr get flushed before wisp_eval returns.
    // Without this, the in-memory buffer may stay empty even on rc=0.
    let wrapped = format!(
        "import sys\ntry:\n{}\nfinally:\n    sys.stdout.flush(); sys.stderr.flush()\n",
        indent_lines(code, "    ")
    );

    let bytes = wrapped.as_bytes();
    let ptr = alloc.call(&mut store, bytes.len() as i32)?;
    mem.data_mut(&mut store)[ptr as usize..ptr as usize + bytes.len()].copy_from_slice(bytes);
    let rc = eval.call(&mut store, (ptr, bytes.len() as i32))?;

    drop(store); // releases pipes' senders; downstream can drain

    let stdout_bytes = stdout.contents().to_vec();
    let stderr_bytes = stderr.contents().to_vec();
    Ok(EvalResponse {
        rc,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        elapsed_us: t_total.elapsed().as_micros() as u64,
    })
}

fn indent_lines(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// =====================================================================
// HTTP handlers
// =====================================================================

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    runtime: Arc<Runtime>,
    jobs: std::sync::mpsc::Sender<Job>,
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn eval_handler(
    State(state): State<AppState>,
    Json(req): Json<EvalRequest>,
) -> impl IntoResponse {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Result<EvalResponse>>();
    if let Err(e) = state.jobs.send(Job::Eval {
        code: req.code,
        reply: reply_tx,
    }) {
        warn!("send to worker failed: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("{e}") }).unwrap()),
        );
    }
    // Receive on a blocking thread (workers might take time; never block tokio)
    let res = tokio::task::spawn_blocking(move || reply_rx.recv()).await;
    match res {
        Ok(Ok(Ok(resp))) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())),
        Ok(Ok(Err(e))) => {
            warn!("eval error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(ErrorResponse { error: format!("{e:#}") }).unwrap()),
            )
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("worker disconnected: {e}") }).unwrap()),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("join error: {e}") }).unwrap()),
        ),
    }
}

// =====================================================================
// main
//
// Wasmtime-wasi 27's sync API panics if it detects an ambient tokio
// runtime ("Cannot start a runtime from within a runtime"). So we do the
// snapshot capture and per-request wasm execution OUTSIDE any tokio
// context — capture is done before the runtime starts, and per-request
// work runs on a dedicated std::thread worker pool that the HTTP handler
// dispatches to via a channel.
// =====================================================================

enum Job {
    Eval {
        code: String,
        reply: std::sync::mpsc::Sender<Result<EvalResponse>>,
    },
}

fn worker_loop(rt: Arc<Runtime>, rx: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<Job>>>) {
    loop {
        let job = {
            let lock = rx.lock().unwrap();
            lock.recv()
        };
        match job {
            Ok(Job::Eval { code, reply }) => {
                let res = run_one(&rt, &code);
                let _ = reply.send(res);
            }
            Err(_) => break, // channel closed → shut down worker
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let python_wasm: PathBuf = std::env::var("WISP_PYTHON_WASM")
        .unwrap_or_else(|_| {
            "runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm"
                .to_string()
        })
        .into();
    let host_root = std::env::var("WISP_HOST_ROOT")
        .unwrap_or_else(|_| "runtime/cpython-wasi/vendor/cpython".to_string());
    let snapshot_path: PathBuf = std::env::var("WISP_SNAPSHOT_PATH")
        .unwrap_or_else(|_| "/tmp/wisp-runtime-snapshot.bin".to_string())
        .into();
    let bind: String = std::env::var("WISP_BIND").unwrap_or_else(|_| "127.0.0.1:9000".to_string());
    let workers: usize = std::env::var("WISP_WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));

    info!(?python_wasm, ?host_root, ?snapshot_path, ?bind, workers, "starting wisp-runtime");

    // Sync phase — capture snapshot, build the COW-mounted engine. No
    // tokio runtime is active yet, so wasmtime-wasi's sync API is happy.
    let runtime = Arc::new(build_runtime(python_wasm, host_root, snapshot_path)?);

    // Worker pool — dedicated OS threads, no tokio context.
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for i in 0..workers {
        let rt_clone = runtime.clone();
        let rx_clone = rx.clone();
        std::thread::Builder::new()
            .name(format!("wisp-worker-{i}"))
            .spawn(move || worker_loop(rt_clone, rx_clone))?;
    }

    // Async phase — tokio runtime drives axum. HTTP handlers send jobs
    // to the worker pool over the std::mpsc channel.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tokio_rt.block_on(async move {
        let state = AppState { runtime, jobs: tx };
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/v1/eval", post(eval_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        info!("listening on http://{bind}");
        axum::serve(listener, app).await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

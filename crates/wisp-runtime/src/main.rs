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

mod capabilities;
mod session;
use capabilities::Capabilities;
use session::{CreateSessionResponse, SessionEvalRequest, SessionInfo, Sessions};

use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
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

pub(crate) const PYTHONPATH_GUEST: &str = "/cross-build/wasm32-wasip1/build/lib.wasi-wasm32-3.14";

/// Guest path where the host's `wisp_python_lib` directory is mounted —
/// gives sandbox Python `import wisp`. CPython on WASI's startup parser
/// only honors ONE PYTHONPATH entry (does not split on `:`), so we add
/// the wisp-lib path to `sys.path` via a bootstrap eval after wisp_init.
pub(crate) const WISP_LIB_GUEST: &str = "/wisp_lib";

/// Run after `wisp_init` and BEFORE we capture the snapshot, so every
/// per-call instance starts with sys.modules already populated with
/// the modules we know will dominate per-call latency.
///
/// The `import numpy` here adds ~30 MB to the snapshot but drops a
/// numpy-using call from ~520 ms to ~2 ms (per-call freshness still
/// holds — each instance gets a fresh copy of the post-init state,
/// just doesn't re-run the importer). Wrapped in try/except so a
/// missing numpy (e.g. someone running an older reactor.wasm built
/// before the M1 numpy patch) doesn't break the snapshot.
const SNAPSHOT_BOOTSTRAP_PY: &str = "\
import sys
sys.path.insert(0, '/wisp_lib')
import wisp
try:
    import numpy
except ImportError:
    pass
";

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

pub(crate) struct Runtime {
    pub engine: Engine,
    pub module: Module,
    pub snapshot_len: usize,
    pub snapshot_pages: u64,
    pub snapshot_fd: RawFd,
    pub host_root: String,
    pub wisp_lib_root: Option<String>,
    pub capabilities: Arc<Capabilities>,
}

fn capture_snapshot(
    host_root: &str,
    wisp_lib_root: Option<&str>,
    python_wasm: &PathBuf,
) -> Result<(Vec<u8>, u64)> {
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
    if let Some(lib) = wisp_lib_root {
        b.preopened_dir(lib, WISP_LIB_GUEST, DirPerms::READ, FilePerms::READ)?;
    }
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

    // Bootstrap: pre-import the wisp helper module if its mount is set
    // up, so every per-call instance starts with sys.modules["wisp"]
    // already populated. Without this, the first user `import wisp` in
    // each call pays the .py parse + bytecode compile cost (~30 ms).
    if wisp_lib_root.is_some() {
        let alloc = inst.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
        let eval = inst.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
        let mem = inst
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("no memory export during bootstrap"))?;
        let src = SNAPSHOT_BOOTSTRAP_PY.as_bytes();
        let ptr = alloc.call(&mut store, src.len() as i32)?;
        mem.data_mut(&mut store)[ptr as usize..ptr as usize + src.len()]
            .copy_from_slice(src);
        let brc = eval.call(&mut store, (ptr, src.len() as i32))?;
        if brc != 0 {
            return Err(anyhow!(
                "snapshot bootstrap (`import wisp`) returned rc={brc}"
            ));
        }
    }

    let mem = inst
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export"))?;
    let bytes = mem.data(&store).to_vec();
    let pages = mem.size(&store);
    Ok((bytes, pages))
}

fn build_runtime(
    python_wasm: PathBuf,
    host_root: String,
    wisp_lib_root: Option<String>,
    snapshot_path: PathBuf,
    capabilities: Arc<Capabilities>,
) -> Result<Runtime> {
    info!("capturing initial snapshot...");
    let t = Instant::now();
    let (snapshot, snapshot_pages) =
        capture_snapshot(&host_root, wisp_lib_root.as_deref(), &python_wasm)?;
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
        wisp_lib_root,
        capabilities,
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
pub(crate) struct EvalResponse {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_us: u64,
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
    let caps = rt.capabilities.clone();
    linker.func_wrap(
        "env",
        "host_call",
        move |mut caller: Caller<'_, WasiP1Ctx>,
              name_ptr: i32,
              name_len: i32,
              payload_ptr: i32,
              payload_len: i32,
              result_ptr: i32,
              result_max: i32|
              -> i32 {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -100,
            };
            let mut name_buf = vec![0u8; name_len as usize];
            if mem.read(&caller, name_ptr as usize, &mut name_buf).is_err() {
                return -101;
            }
            let mut payload_buf = vec![0u8; payload_len as usize];
            if mem
                .read(&caller, payload_ptr as usize, &mut payload_buf)
                .is_err()
            {
                return -101;
            }
            let name = std::str::from_utf8(&name_buf).unwrap_or("");
            match caps.dispatch(name, &payload_buf) {
                Ok(response) => {
                    if response.len() > result_max as usize {
                        return capabilities::RC_RESPONSE_TOO_LARGE;
                    }
                    if mem
                        .write(&mut caller, result_ptr as usize, &response)
                        .is_err()
                    {
                        return -103;
                    }
                    response.len() as i32
                }
                Err(rc) => rc,
            }
        },
    )?;

    let mut b = WasiCtxBuilder::new();
    b.stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(&rt.host_root, "/", DirPerms::READ, FilePerms::READ)?;
    if let Some(lib) = &rt.wisp_lib_root {
        b.preopened_dir(lib, WISP_LIB_GUEST, DirPerms::READ, FilePerms::READ)?;
    }
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

pub(crate) fn indent_lines(s: &str, prefix: &str) -> String {
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
    runtime: Arc<Runtime>,
    jobs: std::sync::mpsc::Sender<Job>,
    sessions: Arc<Sessions>,
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
// Session HTTP handlers
// =====================================================================

async fn create_session_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Spawning the session thread does sync wasmtime work — keep tokio
    // out of it via spawn_blocking.
    let rt = state.runtime.clone();
    let sessions = state.sessions.clone();
    let result = tokio::task::spawn_blocking(move || sessions.create(rt)).await;
    match result {
        Ok(Ok(handle)) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(CreateSessionResponse {
                session_id: handle.id.clone(),
            }).unwrap()),
        ),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("{e:#}") }).unwrap()),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("join error: {e}") }).unwrap()),
        ),
    }
}

async fn session_eval_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<SessionEvalRequest>,
) -> impl IntoResponse {
    let Some(handle) = state.sessions.get(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::to_value(ErrorResponse {
                error: format!("session {session_id} not found"),
            }).unwrap()),
        );
    };
    let result = tokio::task::spawn_blocking(move || handle.eval(req.code)).await;
    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())),
        Ok(Err(e)) => {
            warn!("session {session_id}: eval error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(ErrorResponse { error: format!("{e:#}") }).unwrap()),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::to_value(ErrorResponse { error: format!("join error: {e}") }).unwrap()),
        ),
    }
}

async fn delete_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    if state.sessions.delete(&session_id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn list_sessions_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let map = state.sessions.map.lock().unwrap();
    let infos: Vec<SessionInfo> = map.values().map(|h| SessionInfo::from(h.as_ref())).collect();
    Json(infos)
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

fn load_capabilities() -> Result<Capabilities> {
    if let Ok(path) = std::env::var("WISP_CAPABILITIES_JSON") {
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow!("failed to read {path}: {e}"))?;
        Ok(serde_json::from_slice(&bytes)
            .map_err(|e| anyhow!("failed to parse {path}: {e}"))?)
    } else {
        Ok(Capabilities::default())
    }
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
    // Optional: directory containing `wisp.py`. If present (default), it
    // gets mounted at `/wisp_lib` and added to the sandbox's PYTHONPATH
    // so `import wisp` works. Set to empty string to disable.
    let wisp_lib_root: Option<String> = match std::env::var("WISP_PYTHON_LIB") {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        Err(_) => Some("runtime/cpython-wasi/wisp_python_lib".to_string()),
    };
    let snapshot_path: PathBuf = std::env::var("WISP_SNAPSHOT_PATH")
        .unwrap_or_else(|_| "/tmp/wisp-runtime-snapshot.bin".to_string())
        .into();
    let bind: String = std::env::var("WISP_BIND").unwrap_or_else(|_| "127.0.0.1:9000".to_string());
    let workers: usize = std::env::var("WISP_WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));

    info!(?python_wasm, ?host_root, ?wisp_lib_root, ?snapshot_path, ?bind, workers, "starting wisp-runtime");

    // Capability config: load from $WISP_CAPABILITIES_JSON file if set,
    // otherwise default to no capabilities (sandbox can run pure Python
    // but cannot reach out — same default as no host bridge at all).
    let capabilities = Arc::new(load_capabilities()?);
    info!(?capabilities, "loaded capability config");

    // Sync phase — capture snapshot, build the COW-mounted engine. No
    // tokio runtime is active yet, so wasmtime-wasi's sync API is happy.
    let runtime = Arc::new(build_runtime(
        python_wasm,
        host_root,
        wisp_lib_root,
        snapshot_path,
        capabilities,
    )?);

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

    // Sessions registry + idle-GC thread. Default 5 min idle timeout;
    // override via WISP_SESSION_IDLE_SECONDS.
    let sessions = Sessions::new();
    let idle_secs: u64 = std::env::var("WISP_SESSION_IDLE_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    session::spawn_gc(sessions.clone(), std::time::Duration::from_secs(idle_secs));
    info!(idle_timeout_secs = idle_secs, "session GC thread started");

    // Async phase — tokio runtime drives axum. HTTP handlers send jobs
    // to the worker pool over the std::mpsc channel.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tokio_rt.block_on(async move {
        let state = AppState {
            runtime,
            jobs: tx,
            sessions,
        };
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/v1/eval", post(eval_handler))
            // Session API: stateful, per-call Python interpreter state
            // survives across evals within a session.
            .route("/v1/session", post(create_session_handler))
            .route("/v1/sessions", get(list_sessions_handler))
            .route("/v1/session/:id/eval", post(session_eval_handler))
            .route("/v1/session/:id", delete(delete_session_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        info!("listening on http://{bind}");
        axum::serve(listener, app).await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

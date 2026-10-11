//! Session API — E2B-style persistent semantics.
//!
//! A "session" is a wasmtime Instance that survives across multiple
//! `eval` calls. State (sys.modules, user-defined names, mid-imports)
//! carries over. Antithetical to the per-call freshness that's the
//! daemon's default — sessions are an opt-in for workloads where the
//! state-carry is the point (notebook-style exploration, multi-step
//! agent flows that want intermediate variables to persist).
//!
//! Architecture:
//!
//!   * One dedicated std::thread per session.
//!   * Session thread holds Store<WasiP1Ctx> + Instance for the life
//!     of the session — built once at session creation, never reset.
//!   * Eval requests reach the session via an mpsc channel; replies
//!     come back via a per-call oneshot.
//!   * stdout/stderr use bounded, drainable buffers reset after each eval.
//!   * Sessions are tracked in a daemon-global Sessions map keyed by
//!     UUID. A background GC thread reaps sessions idle longer than
//!     `idle_timeout` (default 5 min).
//!
//! What's deliberately *not* in v1:
//!   * Per-session capability override (uses daemon-wide caps).
//!   * Per-session resource limits (sandbox can still grow memory).
//!   * Session persistence across daemon restarts.
//!   * Streaming output (read final buffer at end of each call).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;
use wasmtime::*;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::capabilities;
use crate::output::OutputCapture;
use crate::{indent_lines, EvalResponse, Runtime, PYTHONPATH_GUEST, WISP_LIB_GUEST};

/// What a session thread receives. Dropping the corresponding sender
/// ends the loop (rx.recv() returns Err).
enum SessionJob {
    Eval {
        code: String,
        reply: std::sync::mpsc::Sender<Result<EvalResponse>>,
    },
    Snapshot {
        reply: std::sync::mpsc::Sender<Result<BranchSnapshot>>,
    },
}

/// Upper bound on branches per call — backstop against a runaway
/// client, far above any realistic sweep batch.
const MAX_BRANCHES: usize = 4096;

pub(crate) fn validate_branch_codes(codes: &[String]) -> std::result::Result<(), String> {
    if codes.is_empty() {
        Err("branch: empty codes array".into())
    } else if codes.len() > MAX_BRANCHES {
        Err(format!(
            "branch: {} codes exceeds cap {MAX_BRANCHES}",
            codes.len()
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct UnbranchableResources;

impl std::fmt::Display for UnbranchableResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("branch cannot copy open WASI file/directory handles or modified standard/preopen descriptors; close opened handles before branching and reopen files inside each child")
    }
}
impl std::error::Error for UnbranchableResources {}

#[derive(Default)]
struct OpenHandles {
    opened: BTreeSet<u32>,
    modified_base: bool,
}

/// Daemon-side handle. Held in the Sessions map; cloning is cheap
/// (Arc + channel sender).
pub struct SessionHandle {
    pub id: String,
    sender: std::sync::mpsc::Sender<SessionJob>,
    runtime: Arc<Runtime>,
    last_activity: Mutex<Instant>,
    created_at: Instant,
}

impl SessionHandle {
    pub fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }

    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    pub fn eval(&self, code: String) -> Result<EvalResponse> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.sender
            .send(SessionJob::Eval { code, reply: tx })
            .map_err(|e| anyhow!("session worker disconnected: {e}"))?;
        self.touch();
        rx.recv()
            .map_err(|e| anyhow!("session reply channel closed: {e}"))?
    }

    /// Fork the session's CURRENT state into one COW child per code
    /// string, run them in parallel, and return the results in order.
    /// The parent session is untouched (children map the state
    /// MAP_PRIVATE) and blocked only while the fork snapshot is
    /// written.
    pub fn branch(&self, codes: Vec<String>) -> Result<BranchResponse> {
        validate_branch_codes(&codes).map_err(|e| anyhow!(e))?;
        let (tx, rx) = std::sync::mpsc::channel();
        self.sender
            .send(SessionJob::Snapshot { reply: tx })
            .map_err(|e| anyhow!("session worker disconnected: {e}"))?;
        self.touch();
        let snapshot = rx
            .recv()
            .map_err(|e| anyhow!("session reply channel closed: {e}"))??;
        // The session thread can now process parent evals while this
        // request's blocking thread runs children against the immutable file.
        let result = run_branches(&self.runtime, snapshot, codes);
        self.touch();
        result
    }
}

/// The global registry. Held in AppState (Arc<Sessions>).
pub struct Sessions {
    pub map: Mutex<HashMap<String, Arc<SessionHandle>>>,
}

impl Sessions {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(HashMap::new()),
        })
    }

    pub fn create(&self, runtime: Arc<Runtime>) -> Result<Arc<SessionHandle>> {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = std::sync::mpsc::channel::<SessionJob>();
        let handle = Arc::new(SessionHandle {
            id: id.clone(),
            sender: tx,
            runtime: runtime.clone(),
            last_activity: Mutex::new(Instant::now()),
            created_at: Instant::now(),
        });
        let id_for_thread = id.clone();
        std::thread::Builder::new()
            .name(format!("wisp-session-{}", &id[..8]))
            .spawn(move || {
                if let Err(e) = session_loop(runtime, rx) {
                    warn!(session=%id_for_thread, "session loop exited with error: {e:#}");
                }
            })?;
        self.map.lock().unwrap().insert(id.clone(), handle.clone());
        info!(session=%id, "session created");
        Ok(handle)
    }

    pub fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.map.lock().unwrap().get(id).cloned()
    }

    /// Returns true if the session existed and was removed. Dropping
    /// the SessionHandle drops its sender, which ends the session
    /// thread's rx.recv() loop.
    pub fn delete(&self, id: &str) -> bool {
        let removed = self.map.lock().unwrap().remove(id).is_some();
        if removed {
            info!(session=%id, "session deleted");
        }
        removed
    }
}

/// What a session thread holds for its lifetime.
struct SessionState {
    store: Store<WasiP1Ctx>,
    instance: Instance,
    stdout_pipe: OutputCapture,
    stderr_pipe: OutputCapture,
    open_handles: Arc<Mutex<OpenHandles>>,
}

type OpenFunction = TypedFunc<(i32, i32, i32, i32, i32, i64, i64, i32, i32), i32>;

struct HandleCalls {
    open: OpenFunction,
    close: TypedFunc<i32, i32>,
    renumber: TypedFunc<(i32, i32), i32>,
}

struct HandleProxy {
    module: Module,
    originals: Vec<Extern>,
    calls: Arc<OnceLock<HandleCalls>>,
}

impl HandleProxy {
    fn initialize(self, store: &mut Store<WasiP1Ctx>, memory: Memory) -> Result<()> {
        let mut imports = vec![Extern::Memory(memory)];
        imports.extend(self.originals);
        let proxy = Instance::new(&mut *store, &self.module, &imports)?;
        self.calls
            .set(HandleCalls {
                open: proxy.get_typed_func(&mut *store, "path_open")?,
                close: proxy.get_typed_func(&mut *store, "fd_close")?,
                renumber: proxy.get_typed_func(&mut *store, "fd_renumber")?,
            })
            .map_err(|_| anyhow!("descriptor proxy already initialized"))
    }
}

/// Track real WASI descriptor operations, including os.open/os.close;
/// Python object inspection would miss raw or untracked descriptors.
fn track_open_handles(
    linker: &mut Linker<WasiP1Ctx>,
    store: &mut Store<WasiP1Ctx>,
    handles: Arc<Mutex<OpenHandles>>,
) -> Result<HandleProxy> {
    const WASI: &str = "wasi_snapshot_preview1";
    let originals = ["path_open", "fd_close", "fd_renumber"]
        .into_iter()
        .map(|name| {
            linker
                .get(&mut *store, WASI, name)
                .ok_or_else(|| anyhow!("missing WASI import {name}"))
        })
        .collect::<Result<Vec<_>>>()?;
    // WASI imports resolve memory via Caller::get_export. Calling the
    // original host Func directly from another host Func loses that
    // caller instance. A tiny Wasm proxy exports the SAME guest memory
    // and preserves that context without copying or allocating memory.
    let module = Module::new(
        store.engine(),
        r#"(module
        (import "guest" "memory" (memory 0))
        (export "memory" (memory 0))
        (import "wasi" "path_open" (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi" "fd_close" (func $close (param i32) (result i32)))
        (import "wasi" "fd_renumber" (func $renumber (param i32 i32) (result i32)))
        (func (export "path_open") (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)
            local.get 0 local.get 1 local.get 2 local.get 3 local.get 4
            local.get 5 local.get 6 local.get 7 local.get 8 call $open)
        (func (export "fd_close") (param i32) (result i32) local.get 0 call $close)
        (func (export "fd_renumber") (param i32 i32) (result i32)
            local.get 0 local.get 1 call $renumber)
    )"#,
    )?;
    let calls = Arc::new(OnceLock::<HandleCalls>::new());
    linker.allow_shadowing(true);
    let opened = handles.clone();
    let open_calls = calls.clone();
    linker.func_wrap(
        WASI,
        "path_open",
        move |mut caller: Caller<'_, WasiP1Ctx>,
              fd: i32,
              flags: i32,
              path: i32,
              len: i32,
              oflags: i32,
              base: i64,
              inheriting: i64,
              fdflags: i32,
              result: i32|
              -> Result<i32> {
            let rc = open_calls
                .get()
                .ok_or_else(|| anyhow!("descriptor proxy not initialized"))?
                .open
                .call(
                    &mut caller,
                    (
                        fd, flags, path, len, oflags, base, inheriting, fdflags, result,
                    ),
                )?;
            if rc == 0 {
                let memory = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                    .ok_or_else(|| anyhow!("no memory export while tracking path_open"))?;
                let mut bytes = [0; 4];
                memory.read(&caller, result as usize, &mut bytes)?;
                opened
                    .lock()
                    .unwrap()
                    .opened
                    .insert(u32::from_le_bytes(bytes));
            }
            Ok(rc)
        },
    )?;
    let closed = handles.clone();
    let close_calls = calls.clone();
    linker.func_wrap(
        WASI,
        "fd_close",
        move |mut caller: Caller<'_, WasiP1Ctx>, fd: i32| -> Result<i32> {
            let rc = close_calls
                .get()
                .ok_or_else(|| anyhow!("descriptor proxy not initialized"))?
                .close
                .call(&mut caller, fd)?;
            if rc == 0 {
                let mut handles = closed.lock().unwrap();
                if !handles.opened.remove(&(fd as u32)) {
                    handles.modified_base = true;
                }
            }
            Ok(rc)
        },
    )?;
    let renumber_calls = calls.clone();
    linker.func_wrap(
        WASI,
        "fd_renumber",
        move |mut caller: Caller<'_, WasiP1Ctx>, from: i32, to: i32| -> Result<i32> {
            let rc = renumber_calls
                .get()
                .ok_or_else(|| anyhow!("descriptor proxy not initialized"))?
                .renumber
                .call(&mut caller, (from, to))?;
            if rc == 0 && from != to {
                let mut handles = handles.lock().unwrap();
                // A moved base descriptor cannot be reconstructed by a fresh
                // child. Renumbering onto stdio/preopens also changes base state.
                if !handles.opened.remove(&(from as u32)) || to < 5 {
                    handles.modified_base = true;
                }
                handles.opened.insert(to as u32);
            }
            Ok(rc)
        },
    )?;
    linker.allow_shadowing(false);
    Ok(HandleProxy {
        module,
        originals,
        calls,
    })
}

fn session_loop(rt: Arc<Runtime>, rx: std::sync::mpsc::Receiver<SessionJob>) -> Result<()> {
    let mut state = build_session_state(&rt)?;
    while let Ok(job) = rx.recv() {
        match job {
            SessionJob::Eval { code, reply } => {
                let result = run_eval_in_session(&rt, &mut state, &code);
                let _ = reply.send(result);
            }
            SessionJob::Snapshot { reply } => {
                let result = capture_branch(&mut state);
                let _ = reply.send(result);
            }
        }
    }
    // rx ended (sender dropped via Sessions::delete) — clean exit.
    Ok(())
}

fn build_session_state(rt: &Runtime) -> Result<SessionState> {
    let stdout_pipe = OutputCapture::new(1 << 20);
    let stderr_pipe = OutputCapture::new(1 << 20);

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
            capabilities::host_call_bridge(
                &caps,
                &mut caller,
                name_ptr,
                name_len,
                payload_ptr,
                payload_len,
                result_ptr,
                result_max,
            )
        },
    )?;
    crate::accel::add_to_linker(&mut linker, rt.accel)?;

    let mut b = WasiCtxBuilder::new();
    b.stdout(stdout_pipe.clone())
        .stderr(stderr_pipe.clone())
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(&rt.host_root, "/", DirPerms::READ, FilePerms::READ)?;
    if let Some(lib) = &rt.wisp_lib_root {
        b.preopened_dir(lib, WISP_LIB_GUEST, DirPerms::READ, FilePerms::READ)?;
    }
    let wasi = b.build_p1();
    let mut store = Store::new(&rt.engine, wasi);
    let open_handles = Arc::new(Mutex::new(OpenHandles::default()));
    let handle_proxy = track_open_handles(&mut linker, &mut store, open_handles.clone())?;
    let instance = linker.instantiate(&mut store, &rt.module)?;

    // Mmap-reset once: bring the Instance's linear memory to the
    // post-`wisp_init`+pre-imports snapshot state. From here onward
    // it evolves through user code without further reset.
    let mem = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export on session instance"))?;
    handle_proxy.initialize(&mut store, mem)?;
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
            "session: re-mmap snapshot failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let cur = mem.size(&store);
    if cur < rt.snapshot_pages {
        mem.grow(&mut store, rt.snapshot_pages - cur)?;
    }

    Ok(SessionState {
        store,
        instance,
        stdout_pipe,
        stderr_pipe,
        open_handles,
    })
}

fn run_eval_in_session(rt: &Runtime, state: &mut SessionState, code: &str) -> Result<EvalResponse> {
    let t = Instant::now();
    let alloc = state
        .instance
        .get_typed_func::<i32, i32>(&mut state.store, "wisp_alloc")?;
    let eval = state
        .instance
        .get_typed_func::<(i32, i32), i32>(&mut state.store, "wisp_eval")?;
    let mem = state
        .instance
        .get_memory(&mut state.store, "memory")
        .ok_or_else(|| anyhow!("no memory export on session instance"))?;

    let wrapped = format!(
        "import sys\ntry:\n{}\nfinally:\n    sys.stdout.flush(); sys.stderr.flush()\n",
        indent_lines(code, "    ")
    );
    let bytes = wrapped.as_bytes();
    let ptr = alloc.call(&mut state.store, bytes.len() as i32)?;
    mem.data_mut(&mut state.store)[ptr as usize..ptr as usize + bytes.len()].copy_from_slice(bytes);
    let result = eval.call(&mut state.store, (ptr, bytes.len() as i32));
    let free = state
        .instance
        .get_typed_func::<i32, ()>(&mut state.store, "wisp_free")?;
    free.call(&mut state.store, ptr)?;
    // Drain even if the guest trapped, so failed eval output cannot leak
    // into a later response or permanently consume capture capacity.
    let stdout = state.stdout_pipe.take();
    let stderr = state.stderr_pipe.take();
    let rc = result?;

    let _ = rt; // unused but kept for future use (cap-bridge override)
    Ok(EvalResponse {
        rc,
        stdout,
        stderr,
        elapsed_us: t.elapsed().as_micros() as u64,
    })
}

// ---- branch: fork the session state N ways ------------------------------

/// Snapshot the session's current linear memory to a temp file, then
/// run each code string in its own COW child instance mapped from that
/// snapshot. Children share all pre-branch pages physically (dirty
/// pages only per child); the parent is never mutated.
struct BranchSnapshot {
    file: std::fs::File,
    len: usize,
    pages: u64,
    snapshot_us: u64,
    started: Instant,
}

fn capture_branch(state: &mut SessionState) -> Result<BranchSnapshot> {
    let t0 = Instant::now();
    let handles = state.open_handles.lock().unwrap();
    if handles.modified_base || !handles.opened.is_empty() {
        return Err(UnbranchableResources.into());
    }
    drop(handles);

    let mem = state
        .instance
        .get_memory(&mut state.store, "memory")
        .ok_or_else(|| anyhow!("no memory export on session instance"))?;
    let branch_pages = mem.size(&state.store);
    let data = mem.data(&state.store);
    let branch_len = data.len();

    // The session thread is parked in this call, so the memory is
    // quiescent while we copy it out.
    // Anonymous, owner-only temp file: no stale files on any error path,
    // and no world-readable copy of the parent's Python state.
    let mut file = tempfile::tempfile()?;
    file.write_all(data)?;
    let snapshot_us = t0.elapsed().as_micros() as u64;
    Ok(BranchSnapshot {
        file,
        len: branch_len,
        pages: branch_pages,
        snapshot_us,
        started: t0,
    })
}

fn run_branches(
    rt: &Runtime,
    snapshot: BranchSnapshot,
    codes: Vec<String>,
) -> Result<BranchResponse> {
    let fd = snapshot.file.as_raw_fd();

    let n = codes.len();
    let queue: Mutex<Vec<(usize, String)>> = Mutex::new(codes.into_iter().enumerate().collect());
    let results: Mutex<Vec<Option<BranchItem>>> = Mutex::new((0..n).map(|_| None).collect());
    let workers = std::env::var("WISP_BRANCH_PARALLEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
        })
        .clamp(1, n.max(1));

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let job = queue.lock().unwrap().pop();
                let Some((idx, code)) = job else { break };
                let item = match run_branch_one(rt, fd, snapshot.len, snapshot.pages, &code) {
                    Ok(item) => item,
                    Err(e) => BranchItem {
                        rc: -1000,
                        stdout: String::new(),
                        stderr: String::new(),
                        elapsed_us: 0,
                        error: Some(format!("{e:#}")),
                    },
                };
                results.lock().unwrap()[idx] = Some(item);
            });
        }
    });

    let results = results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every branch index filled"))
        .collect();
    Ok(BranchResponse {
        results,
        snapshot_us: snapshot.snapshot_us,
        elapsed_us: snapshot.started.elapsed().as_micros() as u64,
    })
}

/// One COW child: fresh Instance on the daemon's COW engine, grown to
/// the branch snapshot's size, then MAP_FIXED-remapped so its memory
/// IS the branch snapshot (private mapping — writes stay local).
/// Ordering matters: grow first (grow_to would re-map boot-snapshot
/// content over anything below snapshot_len), then remap.
fn run_branch_one(
    rt: &Runtime,
    branch_fd: RawFd,
    branch_len: usize,
    branch_pages: u64,
    code: &str,
) -> Result<BranchItem> {
    let t = Instant::now();
    let stdout_pipe = OutputCapture::new(1 << 20);
    let stderr_pipe = OutputCapture::new(1 << 20);

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
            capabilities::host_call_bridge(
                &caps,
                &mut caller,
                name_ptr,
                name_len,
                payload_ptr,
                payload_len,
                result_ptr,
                result_max,
            )
        },
    )?;
    crate::accel::add_to_linker(&mut linker, rt.accel)?;

    let mut b = WasiCtxBuilder::new();
    b.stdout(stdout_pipe.clone())
        .stderr(stderr_pipe.clone())
        .env("PYTHONPATH", PYTHONPATH_GUEST)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHOME", "/")
        .preopened_dir(&rt.host_root, "/", DirPerms::READ, FilePerms::READ)?;
    if let Some(lib) = &rt.wisp_lib_root {
        b.preopened_dir(lib, WISP_LIB_GUEST, DirPerms::READ, FilePerms::READ)?;
    }
    let wasi = b.build_p1();
    let mut store = Store::new(&rt.engine, wasi);
    let instance = linker.instantiate(&mut store, &rt.module)?;
    let mem = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export on branch instance"))?;

    let cur = mem.size(&store);
    if cur < branch_pages {
        mem.grow(&mut store, branch_pages - cur)?;
    }
    let base = mem.data_ptr(&mut store);
    let r = unsafe {
        libc::mmap(
            base as *mut libc::c_void,
            branch_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            branch_fd,
            0,
        )
    };
    if r == libc::MAP_FAILED {
        return Err(anyhow!(
            "branch: re-mmap snapshot failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "wisp_alloc")?;
    let eval = instance.get_typed_func::<(i32, i32), i32>(&mut store, "wisp_eval")?;
    let wrapped = format!(
        "import sys\ntry:\n{}\nfinally:\n    sys.stdout.flush(); sys.stderr.flush()\n",
        indent_lines(code, "    ")
    );
    let bytes = wrapped.as_bytes();
    let ptr = alloc.call(&mut store, bytes.len() as i32)?;
    mem.data_mut(&mut store)[ptr as usize..ptr as usize + bytes.len()].copy_from_slice(bytes);
    let rc = eval.call(&mut store, (ptr, bytes.len() as i32))?;
    drop(store);

    Ok(BranchItem {
        rc,
        stdout: stdout_pipe.take(),
        stderr: stderr_pipe.take(),
        elapsed_us: t.elapsed().as_micros() as u64,
        error: None,
    })
}

/// Background GC thread. Periodically scans the Sessions map and
/// removes any session that's been idle longer than `idle_timeout`.
pub fn spawn_gc(sessions: Arc<Sessions>, idle_timeout: Duration) {
    std::thread::Builder::new()
        .name("wisp-session-gc".into())
        .spawn(move || gc_loop(sessions, idle_timeout))
        .expect("failed to spawn session GC thread");
}

fn gc_loop(sessions: Arc<Sessions>, idle_timeout: Duration) {
    // Wake every 30s (or sooner if idle_timeout is small).
    let interval = idle_timeout
        .min(Duration::from_secs(30))
        .max(Duration::from_secs(5));
    loop {
        std::thread::sleep(interval);
        let to_remove: Vec<String> = {
            let map = sessions.map.lock().unwrap();
            map.iter()
                .filter(|(_, h)| h.idle_for() > idle_timeout)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in to_remove {
            if sessions.delete(&id) {
                info!(session=%id, "session reaped (idle)");
            }
        }
    }
}

// ---- HTTP request/response shapes ---------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionEvalRequest {
    pub code: String,
    #[serde(default = "default_timeout_ms")]
    #[allow(dead_code)]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
pub struct BranchRequest {
    pub codes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BranchItem {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BranchResponse {
    pub results: Vec<BranchItem>,
    /// Time to write the fork snapshot (parent blocked for this long).
    pub snapshot_us: u64,
    /// Total branch call time including all children.
    pub elapsed_us: u64,
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub age_ms: u64,
    pub idle_ms: u64,
}

impl From<&SessionHandle> for SessionInfo {
    fn from(h: &SessionHandle) -> Self {
        Self {
            id: h.id.clone(),
            age_ms: h.age().as_millis() as u64,
            idle_ms: h.idle_for().as_millis() as u64,
        }
    }
}

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
//!   * stdout/stderr are captured by MemoryOutputPipe buffers held by
//!     the session. Each eval reads only the *new* bytes appended
//!     since the previous high-water mark.
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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;
use wasmtime::*;
use wasmtime_wasi::pipe::MemoryOutputPipe;
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::capabilities::{self, Capabilities};
use crate::{indent_lines, EvalResponse, Runtime, PYTHONPATH_GUEST, WISP_LIB_GUEST};

/// What a session thread receives. Dropping the corresponding sender
/// ends the loop (rx.recv() returns Err).
enum SessionJob {
    Eval {
        code: String,
        reply: std::sync::mpsc::Sender<Result<EvalResponse>>,
    },
}

/// Daemon-side handle. Held in the Sessions map; cloning is cheap
/// (Arc + channel sender).
pub struct SessionHandle {
    pub id: String,
    sender: std::sync::mpsc::Sender<SessionJob>,
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
        self.map
            .lock()
            .unwrap()
            .insert(id.clone(), handle.clone());
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

    pub fn count(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

/// What a session thread holds for its lifetime.
struct SessionState {
    store: Store<WasiP1Ctx>,
    instance: Instance,
    stdout_pipe: MemoryOutputPipe,
    stderr_pipe: MemoryOutputPipe,
    /// High-water marks — bytes already returned in earlier evals.
    /// `pipe.contents().len() - offset` is what's new.
    stdout_offset: usize,
    stderr_offset: usize,
}

fn session_loop(
    rt: Arc<Runtime>,
    rx: std::sync::mpsc::Receiver<SessionJob>,
) -> Result<()> {
    let mut state = build_session_state(&rt)?;
    while let Ok(job) = rx.recv() {
        match job {
            SessionJob::Eval { code, reply } => {
                let result = run_eval_in_session(&rt, &mut state, &code);
                let _ = reply.send(result);
            }
        }
    }
    // rx ended (sender dropped via Sessions::delete) — clean exit.
    Ok(())
}

fn build_session_state(rt: &Runtime) -> Result<SessionState> {
    let stdout_pipe = MemoryOutputPipe::new(1 << 20);
    let stderr_pipe = MemoryOutputPipe::new(1 << 20);

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
              -> i32 { host_call_impl(&caps, &mut caller, name_ptr, name_len, payload_ptr, payload_len, result_ptr, result_max) },
    )?;

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

    // Mmap-reset once: bring the Instance's linear memory to the
    // post-`wisp_init`+pre-imports snapshot state. From here onward
    // it evolves through user code without further reset.
    let mem = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| anyhow!("no memory export on session instance"))?;
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
        stdout_offset: 0,
        stderr_offset: 0,
    })
}

fn run_eval_in_session(
    rt: &Runtime,
    state: &mut SessionState,
    code: &str,
) -> Result<EvalResponse> {
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
    mem.data_mut(&mut state.store)[ptr as usize..ptr as usize + bytes.len()]
        .copy_from_slice(bytes);
    let rc = eval.call(&mut state.store, (ptr, bytes.len() as i32))?;

    // Slice out only the NEW bytes since the previous eval.
    let stdout_all = state.stdout_pipe.contents();
    let stderr_all = state.stderr_pipe.contents();
    let new_stdout = if stdout_all.len() > state.stdout_offset {
        stdout_all[state.stdout_offset..].to_vec()
    } else {
        Vec::new()
    };
    let new_stderr = if stderr_all.len() > state.stderr_offset {
        stderr_all[state.stderr_offset..].to_vec()
    } else {
        Vec::new()
    };
    state.stdout_offset = stdout_all.len();
    state.stderr_offset = stderr_all.len();
    drop(stdout_all);
    drop(stderr_all);

    let _ = rt; // unused but kept for future use (cap-bridge override)
    Ok(EvalResponse {
        rc,
        stdout: String::from_utf8_lossy(&new_stdout).into_owned(),
        stderr: String::from_utf8_lossy(&new_stderr).into_owned(),
        elapsed_us: t.elapsed().as_micros() as u64,
    })
}

fn host_call_impl(
    caps: &Capabilities,
    caller: &mut Caller<'_, WasiP1Ctx>,
    name_ptr: i32,
    name_len: i32,
    payload_ptr: i32,
    payload_len: i32,
    result_ptr: i32,
    result_max: i32,
) -> i32 {
    let mem = match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => return -100,
    };
    let mut name_buf = vec![0u8; name_len as usize];
    if mem.read(&*caller, name_ptr as usize, &mut name_buf).is_err() {
        return -101;
    }
    let mut payload_buf = vec![0u8; payload_len as usize];
    if mem
        .read(&*caller, payload_ptr as usize, &mut payload_buf)
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
                .write(caller, result_ptr as usize, &response)
                .is_err()
            {
                return -103;
            }
            response.len() as i32
        }
        Err(rc) => rc,
    }
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
    let interval = idle_timeout.min(Duration::from_secs(30)).max(Duration::from_secs(5));
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

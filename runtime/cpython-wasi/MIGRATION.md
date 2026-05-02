# WASI Preview 2 — when to revisit

Last evaluated: 2026-05-02.

## Current target: `wasm32-wasip1`

We build CPython 3.14.3 + libsqlite3 + libcrypto for `wasm32-wasip1` and
run them under Wasmtime crate 27. The 0.69 ms p50 per-call cold start
and 2394 br/s parallel fork numbers are measured on this stack.

## Why not Preview 2 now (2026-05-02)

We evaluated migrating the whole stack to wasip2. Three blockers, in
order of severity:

### 1. CPython mainline has no wasip2 build driver

CPython's main branch (3.15-dev) `Platforms/WASI/config.toml` reads:

```toml
[targets]
wasi-sdk = 33
host-triple = "wasm32-wasip1"
```

Even with the latest wasi-sdk 33, upstream targets wasip1. The reason
is that wasip2's filesystem and IO mappings are still being settled,
and `Modules/_io` / `getpath` / preopened-dir conventions need a port
that hasn't been done.

To go P2 today we would have to fork CPython, write the wasip2 driver
ourselves, port `Modules/_io`, and maintain the patch series until
upstream catches up. That's a multi-week commitment with ongoing
rebase cost.

### 2. Our COW snapshot trick may not survive the component model

The 0.69 ms number depends on Wasmtime's `Config::with_host_memory(..)`
+ `LinearMemory` trait + per-call `mmap MAP_FIXED`. This is a
**core-module** API.

Wasip2 uses the **component model**. A component composes multiple
core modules internally; "the linear memory" is no longer a single
thing the host can hand a creator for. As of 2026-05-02 the
`wasmtime::component` docs do not surface `MemoryCreator` or
`with_host_memory`.

If we migrated to P2 components naively, we might lose the COW path
and regress to memcpy (1.68 ms — we know this number from Spike A2).
Restoring the COW gain on top of the component model would be its own
research project, possibly requiring upstream Wasmtime changes.

### 3. P1's "limits" align with the sandbox security model anyway

Things P1 cannot do:

- Outbound sockets (no `getsockname`, no real TCP)
- Threads, subprocess, fork, signal handlers
- `dlopen` / dynamic native extension loading
- Executable memory (libffi closures — note this is a *WebAssembly*
  limit, P2 doesn't fix it either)

For a sandbox running model-generated Python on behalf of an
orchestrator, these aren't gaps; they're correct constraints. The
sandbox should not open arbitrary outbound TCP, should not spawn
processes, should not load arbitrary `.so` files.

The path for legitimate "the sandboxed code wants to fetch a URL" use
cases is a **host bridge** — the host implements the capability and
exposes it as a WASM import; the sandbox cannot use it without explicit
host approval. See `runtime/cpython-wasi/wisp_entry/wisp_entry.c` and
the `_wisp.call_host` Python API.

This is the same pattern Cloudflare Workers, Deno, and other capability-
based runtimes use. Owning this design choice is better than dragging
P1 → P2 to recover an "unconstrained" sandbox we don't actually want.

## Trigger conditions to revisit

Open this doc again when **any** of the following becomes true:

1. **CPython upstream lands wasip2 support.** Concretely: when
   `https://raw.githubusercontent.com/python/cpython/main/Platforms/WASI/config.toml`
   contains `host-triple = "wasm32-wasip2"` or adds a wasip2 target
   alongside wasip1. At that point the porting cost drops by 80%.

2. **Wasmtime's component model exposes a `host_memory` / `MemoryCreator`
   equivalent.** Concretely: when
   `https://docs.rs/wasmtime/latest/wasmtime/component/` mentions
   `MemoryCreator` or `with_host_memory`, or there is a documented
   recipe for snapshot/COW under components. Without this we cannot
   port the 0.69 ms result.

3. **A real user blocked by the lack of `_ssl`/`socket` shows up.**
   If the use case is "this needs HTTPS in-sandbox and host bridge
   isn't workable for X reason", the missing piece is `_ssl` (which
   only P2 enables). At that point fork-and-port becomes worth the
   cost. But examine first whether host bridge can serve the use case
   — usually it can.

## Migration cost estimate (re-confirm each time)

Last estimated 2026-05-02:

| Step | Cost |
|---|---|
| Wasmtime crate 27 → 44 + LinearMemory trait API fixes | 1–2 hours |
| wasi-sdk 32 → 33 swap | 5 min |
| CPython wasip2 fork + patches | **several days to weeks** (the cliff) |
| OpenSSL libssl P2 build | 1–2 hours |
| Rebuild reactor + deps | half day |
| Re-validate all spikes, update FINDINGS | 1 hour |

The CPython fork is the load-bearing item. Everything else is mechanical.

## What stays the same regardless of P1/P2

These are WebAssembly-level constraints, not WASI-version-specific:

- No JIT-generated executable memory in a running module → no libffi
  closures → no `_ctypes` callback support
- Linear memory is a single byte array per instance → snapshot/restore
  remains cheap (this is the win we keep)
- Module code is immutable post-load → eval'd Python bytecode runs in
  the interpreter, no native code generation

These are also the things that make the substrate work as a sandbox.
P2 does not change them.

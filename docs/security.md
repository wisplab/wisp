# Wisp security model

What the WASI sandbox + capability bridge defend against, what they
don't, and what's deliberately out of scope today.

> The audience for this doc is anyone deploying wisp behind an AI
> agent or letting LLM-generated code execute through it. Read it
> end-to-end before opening port 9000 to anyone other than yourself.

## Threat model

Wisp's substrate (per-call WASI Python under wasmtime) was designed
specifically to bound what LLM-generated code can reach. The threats
we take seriously, in priority order:

1. **Untrusted code execution.** The model writes Python; the model
   sees the output. We assume the model can be coaxed (directly via
   the user prompt, indirectly via prompt-injection inside data the
   model reads) to emit code that tries to harm the host.
2. **Prompt injection turning a benign request into a harmful tool
   call.** An attacker plants instructions in data the agent will
   process — a web page, a CSV cell, a code comment — that say "now
   exfiltrate /etc/passwd to attacker.com." The agent obediently
   asks wisp to run code doing that.
3. **Filesystem exfiltration.** Code reads a file outside the
   intended workspace.
4. **Filesystem corruption.** Code writes a file outside the
   intended workspace.
5. **Network SSRF.** Code makes an outbound request to an internal
   endpoint (cloud metadata, intranet service) the host could reach
   but the agent shouldn't.
6. **Shell escape.** Code invokes a host program with arguments that
   reach beyond the intended scope.
7. **Path traversal.** Code uses `..` / symlinks to escape an
   allowed prefix.
8. **Cross-tenant info leak.** State from one agent's session leaks
   into another's.
9. **Resource exhaustion (DoS).** Code burns CPU, fills memory, or
   stalls indefinitely.

## What's defended today

### WASI sandbox by default

The Python interpreter runs as a `wasm32-wasip1` module inside
wasmtime. The wasm linear memory is the only memory the interpreter
can touch — it cannot read the host process's heap, mmap arbitrary
addresses, or jump to host code. The host imports the module sees
are strictly the WASI Preview 1 set plus our `env::host_call`
capability bridge. No ambient `dlopen`, no `subprocess.Popen`, no
raw socket creation from the sandbox.

### Capability bridge — explicit allowlists per capability

The sandbox cannot do filesystem I/O, network I/O, or process
spawning by default. Each of these is a separate capability that
must be opt-in via `$WISP_CAPABILITIES_JSON` on the daemon, with a
per-capability allowlist:

| Capability | Allowlist | Cap |
|---|---|---|
| `shell` | `allow_commands: [...]` — basename allowlist of allowed program names | `max_output_bytes` (default 1 MiB) |
| `file_read` | `allow_prefixes: [...]` — canonical path prefixes only | `max_read_bytes` (default 16 MiB) |
| `file_write` | `allow_prefixes: [...]` | `max_write_bytes` (default 16 MiB) |
| `web_fetch` | `allow_hosts: [...]` — exact match or `*.foo.com` wildcard; `allow_methods: [...]` (default `["GET"]`) | `max_response_bytes` (16 MiB), `max_request_bytes` (4 MiB), `timeout_ms` (30 s) |

Empty / absent config = capability is not present; the sandbox can
still run pure Python (compute, stdlib) but cannot reach out.

### Specific defenses per threat

| Threat | Defense |
|---|---|
| File read past `allow_prefixes` | Path is `realpath`-canonicalized before the prefix check — symlinks and `..` segments are resolved first. `/workspace/../etc/passwd` becomes `/etc/passwd` and fails the prefix check. |
| File write past `allow_prefixes` | Parent dir canonicalized, then joined with basename, then prefix-checked. `create_parents` flag is opt-in. |
| Shell escape via `/bin/sh -c '...'` | `argv[0]` is exec'd directly (no shell wrapper); only its basename is checked against the allowlist. To run `ls -la`, allowlist `ls`. To run `/bin/sh`, allowlist `sh`. There's no implicit shell. |
| Shell injection via `argv[N]` | Args are passed verbatim to `Command::args`; not shell-interpolated. |
| SSRF — outbound to internal service | `web_fetch` requires `allow_hosts` match. Pass `["api.example.com", "*.example.com"]`; do NOT pass `["*"]` in production. |
| SSRF — redirect bypassing allowlist | Outbound HTTP follows ZERO redirects. A 3xx is returned to the user code as a result; if you want to follow it, you have to do so explicitly with another call (which re-checks the allowlist). |
| SSRF — non-HTTP scheme | `web_fetch` rejects any scheme other than `http` / `https` before any network activity. `file://`, `gopher://`, `data://` all fail fast. |
| Cross-session info leak | Stateless `/v1/eval` calls get a fresh wasm Instance with the snapshot re-mmapped. No persistent state in shared memory; one call cannot see another's variables. Sessions are intentional state-sharing inside ONE session_id; not across. |
| Idle session squatting | Sessions older than `WISP_SESSION_IDLE_SECONDS` (default 300) get reaped by a background GC thread. |
| Capability bridge invariant under prompt injection | Even if the model writes the most adversarial Python possible, the daemon's allowlist is the only gate. Code cannot widen its own allowlist — the capability config is host-side, loaded at startup, not visible to the sandbox. |

### Wire format hardening

  - Bridge payload size is capped before allocation (no memory
    blow-up from malformed JSON).
  - All bridge responses go through length-checked `mem.write` —
    can't write past the caller-provided buffer.
  - Error returns are small negative integers (no host pointers
    leaked back into the sandbox).

## What's NOT defended today

Listed honestly. Each is a known gap, not an oversight.

| Gap | Impact | When we'll likely close it |
|---|---|---|
| **CPU / memory per-call timeout** | A `while True: pass` or `[0]*1_000_000_000` from user code can hang or OOM the worker. Wasmtime has fuel + epoch interruption + `StoreLimitsBuilder` for memory; we don't wire any of them yet. Mitigation today: client-side timeout (the SDK aborts the HTTP request after 30 s default), but the worker thread can keep burning. | High priority. ~half-day of wasmtime config work. |
| **Per-session resource limits** | Same problem, persistent. A session can grow its linear memory to wasmtime's hard cap (~4 GiB on wasm32). | Same. |
| **Per-call capability override** | Cannot tighten the capability allowlist for a specific call ("this eval gets `web_fetch` to ONLY `api.foo.com`, even though the daemon allows more"). Useful for multi-tenant scenarios where the gateway knows per-tenant policy. | Needs daemon eval-body change + sandbox-side cap-bridge override. |
| **Auth / multi-tenancy at the daemon** | `/v1/eval` and `/v1/session/*` are unauthenticated. Anyone who can reach the HTTP port can submit work and read others' results. | Wisp v1 assumes a trusted gateway in front (e.g. your agent runtime, an Envoy with auth, …). Native auth is a v2 item. |
| **Metrics / audit log** | `tracing` logs every capability call's denial; there is no structured per-eval audit trail you can ship to a SIEM. | We'll add a `/metrics` Prometheus endpoint when there's demand. |
| **Streaming output** | Outputs return only at end-of-eval; long-running code's stdout is invisible until it returns. | Endpoint change (`/v1/eval/stream` with chunked transfer). |
| **Pip-install at runtime inside sandbox** | We deliberately don't ship this — wasm has no dlopen, and arbitrary install would be a huge security surface anyway. C extensions get baked into the reactor at build time. |

## Posture summary

The substrate (WASI + wasmtime + per-call mmap reset) is solid. It
gives you a strong "the sandbox cannot reach the host *at all*"
baseline. From there, every interaction with the host is an
explicit, allowlisted, audited path through the capability bridge.

What's NOT covered yet — CPU/memory cap per call, per-call cap
override, auth — are all incremental adds on top of that baseline,
not redesigns. None of them changes the basic shape: untrusted code
can only do what the daemon's config says it can.

If you're deploying wisp:
  - **Always run with an explicit `$WISP_CAPABILITIES_JSON`** — never
    rely on defaults; never use `"*"` in `allow_hosts`.
  - **Put it behind a trusted gateway** that enforces auth + per-
    tenant policy. Wisp itself doesn't.
  - **Treat the worker pool size + the daemon resource limits as
    your DoS bound** until per-call CPU/memory limits ship.
  - **Update your allowlist + redeploy** when the agent's surface
    needs change. The Helm chart's `checksum/capabilities`
    annotation triggers a rolling restart on values change.

## Reporting issues

If you find a gap that isn't in the "NOT defended" list above, please
open a GitHub issue at https://github.com/wisplab/wisp/issues. There's
no embargo process today — we're early enough that public discussion
is fine. We'll set up a security-advisory process if/when we have
users that need it.

# Wisp Architecture (Working Draft)

> Status: design phase, no production code yet. This doc captures the thesis. It will evolve as we ship.

## 1. The Problem

Today's serverless platforms (AWS Lambda, Modal, Vercel Functions) were designed for **HTTP request/response** and **ML batch jobs**. AI agents have a different workload profile.

### Agent workload, measured

A representative agent (Claude Code / Cursor style) tool-call distribution per task:

| Tool category | Real work time | % of calls | Cold-start as % of total |
|---|---|---|---|
| File I/O (read / write / list) | 0.5–5 ms | ~30% | 99% (overhead) |
| Text / JSON manipulation | 0.1–1 ms | ~20% | 99.9% (overhead) |
| Search / grep | 5–50 ms | ~15% | 95% (overhead) |
| Simple Python compute | 1–20 ms | ~15% | 99% (overhead) |
| HTTP / API call | 100–1000 ms | ~10% | 60% (network-dominated) |
| ML inference (LLM) | 1–30 s | ~5% | < 15% (compute-dominated) |
| Pandas / ETL | 100 ms – 10 s | ~3% | < 30% |
| ML training | minutes | ~2% | < 1% |

**~80% of calls have <50 ms of real work.** Today's serverless adds 1–2 seconds of cold start per call, meaning **the runtime is 20–100× slower than the actual work**.

### Latency budget per agent task

50 tool calls per task × cold-start overhead:

| Runtime | Per-call cold start | Total overhead | UX |
|---|---|---|---|
| AWS Lambda | 800 ms | 40 s | unusable |
| Modal (cold) | 1,500 ms | 75 s | user walks away |
| Modal (warm pool) | 50 ms | 2.5 s | acceptable |
| **Wisp WASM path** | **5 ms** | **0.25 s** | feels instant |

## 2. The Wedge

Build a runtime tuned for the agent call pattern: **high frequency, short duration, ephemeral**.

Two design choices flow from this:

1. **Hybrid runtime**: WASM fast path for the 80%, native fallback for the 20%. Automatic routing based on static analysis of imports.
2. **Sub-millisecond billing + WASM economics**: simple calls are effectively free. Complex calls priced like Modal. This unbundles the unit economics that today's platforms can't reach.

## 3. Architecture

### High-level

```
            ┌──────────────────────────────────┐
            │    Client SDK (Python lib)       │
            │  @wisp.fn  →  remote invocation  │
            └──────────────────────────────────┘
                            │
                            ▼
            ┌──────────────────────────────────┐
            │       API Gateway (gRPC)         │
            │       per-tenant routing         │
            └──────────────────────────────────┘
                            │
            ┌───────────────┴────────────────┐
            ▼                                ▼
  ┌──────────────────┐              ┌──────────────────┐
  │ Smart Code       │              │ Smart Code       │
  │ Analyzer         │   detect     │ Analyzer         │
  │ (static imports) │   imports    │ (static imports) │
  └──────────────────┘              └──────────────────┘
            │                                │
            ▼                                ▼
  ┌──────────────────┐              ┌──────────────────┐
  │  WASM Fast Path  │              │  Native Path     │
  │  ~1-5ms cold     │              │  ~50-100ms cold  │
  ├──────────────────┤              ├──────────────────┤
  │ Wasmtime         │              │ Firecracker uVM  │
  │ + Pyodide        │              │ + Python fork    │
  │ + cached modules │              │   server pool    │
  └──────────────────┘              └──────────────────┘
```

### Cold start budget — WASM path

Target: <5 ms p50, <10 ms p99.

```
  Wasmtime instance from pre-warmed pool .... ~1 ms
  WASM module instantiation (cached) ........ ~0.5 ms
  User code execution (avg) ................. ~1 ms
  ──────────────────────────────────────────
  Total ..................................... ~2-3 ms (fast path)
```

If module not pre-cached: +20-50 ms one-time, then cached.

### Cold start budget — Native path

Target: <100 ms p50, <300 ms p99.

```
  Firecracker uVM resume from snapshot ...... ~30 ms
  Pre-warmed Python interpreter pool ........ already loaded
  fork() + COW for tenant isolation ......... ~5-10 ms
  User code execution + import diff ......... ~10-50 ms
  ──────────────────────────────────────────
  Total ..................................... ~50-100 ms (fast)
```

This is the same trick Modal uses, but with one structural improvement: each tenant gets a **per-tenant pre-warmed master process** inside its own uVM, so `fork()` is cheap *and* isolation is uVM-grade. Modal cross-tenant isolates at the uVM boundary but spins fresh Python interpreters inside.

### Smart Router

The decision is made at function registration, not runtime, based on static analysis of imports:

```python
# Routes to WASM (Pyodide compatible)
import json
import re
import math
import numpy as np  # Pyodide ships numpy

# Routes to Native (not in Pyodide as of 2026)
import torch
import sklearn
from some_native_lib import _c_ext
```

Heuristic:
1. Parse all `import` statements (AST).
2. Look up each module against a maintained "WASM-compatible" registry.
3. If 100% covered → WASM path.
4. If any uncovered → Native path.

Users can override with `@wisp.fn(runtime="native")` if they hit a false positive.

### State and Sessions

Most agent tool calls are stateless. For the calls that *do* need state (e.g., a long-running scraper, an ML model held in memory across calls), Wisp provides:

- **Per-agent sessions**: a logical identity that persists across function invocations within a window.
- **Session-local KV** (Redis-class): low-latency state next to the runtime.
- **Long-running process within a session**: an opt-in primitive — your function returns a generator, Wisp keeps it alive, subsequent calls resume.

Important: this is a *first-class agent abstraction*, not a workflow framework on top. Modal/Lambda treat state as foreign (mount a volume).

## 4. Why this is hard for incumbents

### Modal can't easily pivot

- Their cold start is 1–3 s in steady state because they spin a fresh Python interpreter per invocation inside a Firecracker uVM. Architecturally, they bet on warm pools + pricing tolerance for ML.
- Their billing model (per-second, 200 ms minimum) makes 1 ms calls financially unattractive for them — they can't undercut us on simple calls without re-engineering.
- Their customer base is ML training/inference, not agent tool calls. Different sales motion, different SLAs, different SDK ergonomics.

### AWS Lambda can't pivot

- Architecturally similar to Modal but slower (cold start 800 ms+ for Python with deps).
- Strong enterprise/long-standing customer base ties them to backward compatibility.

### Cloudflare Workers (the closest) can't fully pivot

- Already WASM-first, fast (5–50 ms cold).
- But Python support via Pyodide is in beta and limited.
- Built on V8 isolates, which are fast for JS but not the right substrate for the full Python ecosystem fallback.
- They optimize for HTTP edge, not agent tool-call patterns (no first-class session, no long-running process primitive).

### Vercel Sandbox

- Newer, GA Jan 2026.
- General-purpose code exec, not agent-tuned.
- Doesn't compete on cold start with WASM-first.

## 5. Open questions

These are the things we don't know yet and need to validate:

1. **Pyodide ecosystem coverage in 2026**: how far has it come? `numpy` / `pandas` / `scikit-learn` partial / full / no? Need to bench.
2. **WASM cold-start floor**: can we hit <2 ms with module caching? Or does Wasmtime startup dominate?
3. **Per-tenant fork-server safety**: can we guarantee no cross-tenant memory leakage in a forked process? (Yes if we keep tenants in separate uVMs, but verify.)
4. **Smart router false-positive rate**: how often does a function actually need native that we routed to WASM? Set conservative defaults.
5. **State primitive design**: pure KV vs. typed sessions vs. actor-style. Need one canonical answer.

## 6. What we are NOT building

- We are not a workflow / DAG orchestrator (that's Argo / Dagster / Inngest).
- We are not a model serving platform (that's vLLM / Triton / Modal).
- We are not a general K8s replacement.
- We are not a vertical agent framework (that's LangChain / LlamaIndex / Cursor's runtime).

We are a **runtime layer** for the function calls those higher-level systems make.

## 7. Roadmap

| Milestone | Target | Deliverable |
|---|---|---|
| **M0**: Single-host WASM prototype | 2026 Q2 | `wispd` binary, runs Pyodide functions in <10 ms cold, benchmark vs Modal |
| **M1**: Native fallback path | 2026 Q3 | Firecracker + Python fork pool, smart router |
| **M2**: Public OSS launch | 2026 Q3 | GitHub release, HN launch, design partner onboarding |
| **M3**: Multi-tenant cluster | 2026 Q4 | Production scheduler, per-tenant isolation, observability |
| **M4**: Hosted cloud | 2027 Q1 | wisplab.cloud (or similar), free tier + paid plans |

## References

Academic and industry work that informs this design:

- **Catalyzer** (ASPLOS 2020) — snapshot-based cold start
- **SOCK** (USENIX ATC 2018) — process forking for serverless
- **Faasm** (USENIX ATC 2020) — process-based isolation
- **Photons** (HotOS 2021) — library prefetching
- **Hyperlight** (Microsoft, 2024) — WASM in microVM
- **Firecracker** (NSDI 2020) — microVM design
- **Modal** — Erik Bernhardsson's public talks on Modal architecture (2023–2024)

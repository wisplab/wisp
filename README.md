# Wisp

> Serverless runtime for AI agents.

**Status**: 🚧 Under active design. First public prototype targeting Q3 2026.

## Why Wisp

Today's serverless platforms were designed for HTTP request/response and ML batch workloads (seconds-to-minutes execution). AI agents make a fundamentally different call pattern: **millions of small, tight, ephemeral tool calls** — read a file, parse JSON, run a quick search, format a string.

The cold-start overhead in current serverless platforms is ~0.8–2 seconds per call. In a 50-call agent task, that's 75 seconds of pure friction. From the user's perspective: "the agent is slow."

Wisp is built for this call pattern.

## Goals

- **<5ms cold start** for the 80% of agent tool calls that don't need the full Python ecosystem (string ops, JSON, basic compute)
- **Native fallback** for the 20% that does (PyTorch, sklearn, native C extensions) — comparable to Modal cold start (~100–500ms)
- **Zero configuration**: write a Python function, decorate it, run it. No `Dockerfile`, no `modal.Image.pip_install(...)`, no YAML.
- **Open source runtime** under Apache 2.0. Closed-source orchestration / cloud forms the commercial layer (à la Vercel/Next.js).
- **Sub-millisecond billing** so high-frequency simple calls are effectively free.

## Architecture (working draft)

See [`docs/architecture.md`](docs/architecture.md) for the full design.

```
                 ┌────────────────────────────┐
                 │    Smart Code Analyzer     │
                 │  (WASM or Native path?)    │
                 └────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
    ┌──────────────────┐         ┌──────────────────┐
    │  WASM fast path  │         │  Native fallback │
    │  (~1–5 ms)       │         │  (~50–100 ms)    │
    ├──────────────────┤         ├──────────────────┤
    │ Wasmtime +       │         │ Firecracker +    │
    │ Pyodide / WASI   │         │ Python fork pool │
    └──────────────────┘         └──────────────────┘
```

Routing decision is automatic — based on a static analysis of the user's `import` statements. Pure stdlib + `numpy` / `pandas`-light → WASM. Imports `torch` / native C extensions → fallback.

## Status

| Layer | State |
|---|---|
| Architecture doc | ✏️ in progress |
| WASM fast-path prototype | ⏳ next |
| Native fallback (Firecracker) | ⏳ |
| Smart router | ⏳ |
| Python SDK | ⏳ |
| Multi-tenant scheduler | 🔜 |

## License

Apache 2.0 (planned for first public release).

## Contact

- Site: [wisplab.org](https://wisplab.org)
- Email: [hello@wisplab.org](mailto:hello@wisplab.org)

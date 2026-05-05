import { tool } from "@opencode-ai/plugin"

const DEFAULT_DAEMON_URL = "http://localhost:9000"

export default tool({
  description:
    "Execute Python code in a per-call WASI sandbox via the local Wisp " +
    "daemon. Each call gets a fresh CPython 3.14 interpreter — globals " +
    "DO NOT persist across calls. Use this anywhere you'd otherwise reach " +
    "for `bash python -c '...'` on agent-generated code: it isolates the " +
    "filesystem and network from the host. The sandbox has stdlib (zlib, " +
    "sqlite3, hashlib, xxhash) plus the `wisp` host-bridge module exposing " +
    "opt-in capabilities (shell, file_read, file_write, web_fetch). Only " +
    "the capabilities the daemon's $WISP_CAPABILITIES_JSON allowlists are " +
    "actually reachable; everything else fails fast.",
  args: {
    code: tool.schema
      .string()
      .describe(
        "Python source. Top-level statements run in module scope. " +
        "Use print(...) to surface values to the agent — the last " +
        "expression is NOT auto-printed (this isn't a REPL). " +
        "To use the host bridge: `import wisp; wisp.web_fetch(url)`.",
      ),
    timeout_ms: tool.schema
      .number()
      .optional()
      .describe(
        "Per-call timeout in milliseconds. Default 30000. The daemon " +
        "currently does NOT enforce this server-side; this is a " +
        "client-side abort to keep stuck calls from hanging the agent.",
      ),
  },
  async execute(args) {
    const url =
      process.env.WISP_DAEMON_URL?.replace(/\/+$/, "") ?? DEFAULT_DAEMON_URL
    const timeout = args.timeout_ms ?? 30_000
    const controller = new AbortController()
    const abortTimer = setTimeout(() => controller.abort(), timeout + 5_000)
    let resp: Response
    try {
      resp = await fetch(`${url}/v1/eval`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ code: args.code, timeout_ms: timeout }),
        signal: controller.signal,
      })
    } catch (e: any) {
      const cause = e?.cause?.code ?? ""
      const isUnreachable =
        cause === "ECONNREFUSED" ||
        cause === "ENOTFOUND" ||
        /fetch failed/i.test(String(e?.message ?? e))
      if (isUnreachable) {
        return formatError(
          `Wisp daemon not reachable at ${url}.\n` +
            `Start it from the wisp repo:\n` +
            `  cargo run --release -p wisp-runtime\n` +
            `Or set WISP_DAEMON_URL to a remote daemon.`,
        )
      }
      if (e?.name === "AbortError") {
        return formatError(
          `Wisp call exceeded ${timeout} ms client-side timeout.`,
        )
      }
      return formatError(`Wisp request failed: ${e?.message ?? String(e)}`)
    } finally {
      clearTimeout(abortTimer)
    }
    if (!resp.ok) {
      const body = await resp.text().catch(() => "")
      return formatError(
        `Wisp daemon returned HTTP ${resp.status}${body ? `: ${body}` : ""}`,
      )
    }
    const data = (await resp.json()) as {
      rc: number
      stdout: string
      stderr: string
      elapsed_us: number
    }
    return formatResult(data)
  },
})

function formatResult(r: {
  rc: number
  stdout: string
  stderr: string
  elapsed_us: number
}): string {
  const ms = (r.elapsed_us / 1000).toFixed(2)
  const head = `[wisp sandbox: rc=${r.rc}, ${ms} ms]`
  const parts = [head]
  if (r.stdout) parts.push(`--- stdout ---\n${r.stdout.replace(/\n$/, "")}`)
  if (r.stderr) parts.push(`--- stderr ---\n${r.stderr.replace(/\n$/, "")}`)
  if (!r.stdout && !r.stderr) parts.push("(no output)")
  return parts.join("\n")
}

function formatError(msg: string): string {
  return `[wisp sandbox: ERROR]\n${msg}`
}

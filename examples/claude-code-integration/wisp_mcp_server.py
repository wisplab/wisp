"""Wisp MCP server — exposes the local wisp-runtime daemon as an MCP tool.

Run as a stdio MCP server. Claude Code (or any other MCP client) launches
this process, talks JSON-RPC over its stdin/stdout, and routes
tool-calls into the Wisp WASI Python sandbox running at $WISP_DAEMON_URL
(default http://localhost:9000).

Single tool: `python_sandbox(code, timeout_ms=)` — fresh CPython 3.14
interpreter per call, host filesystem and network walled off behind
capability-bridge allowlists. Same daemon contract as the OpenCode
custom tool in `examples/opencode-integration/`; this is just the MCP
adapter for clients that speak MCP.

Single-file by design — copy-pasteable into the user's MCP config.
The Wisp HTTP call is inlined so this script doesn't need the wisp
Python SDK installed; only `mcp` is required.
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.request

from mcp.server.fastmcp import FastMCP


DEFAULT_DAEMON_URL = "http://localhost:9000"

mcp = FastMCP("wisp")


@mcp.tool()
def python_sandbox(code: str, timeout_ms: int = 30000) -> str:
    """Execute Python code in a per-call WASI sandbox via the local Wisp daemon.

    Each call gets a fresh CPython 3.14 interpreter — globals DO NOT
    persist across calls. Use this anywhere you'd otherwise reach for
    `python -c '...'` on agent-generated code: it isolates the
    filesystem and network from the host. The sandbox has stdlib (zlib,
    sqlite3, hashlib, xxhash) plus the `wisp` host-bridge module
    exposing opt-in capabilities (shell, file_read, file_write,
    web_fetch). Only the capabilities the daemon's
    $WISP_CAPABILITIES_JSON allowlists are actually reachable;
    everything else fails fast with a WispError.

    Args:
        code: Python source. Top-level statements run in module scope.
            Use `print(...)` to surface values to the caller — the last
            expression is NOT auto-printed (this isn't a REPL).
            To use the host bridge: `import wisp; wisp.web_fetch(url)`.
        timeout_ms: Per-call timeout in milliseconds. Default 30000.
            Currently enforced only client-side here; the daemon does
            not yet enforce server-side timeouts.
    """
    url = os.environ.get("WISP_DAEMON_URL", DEFAULT_DAEMON_URL).rstrip("/")
    payload = json.dumps({"code": code, "timeout_ms": timeout_ms}).encode("utf-8")
    req = urllib.request.Request(
        f"{url}/v1/eval",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=(timeout_ms / 1000) + 5) as resp:
            body = json.loads(resp.read().decode("utf-8"))
    except urllib.error.URLError as e:
        reason = getattr(e, "reason", str(e))
        if "Connection refused" in str(reason) or isinstance(
            getattr(e, "reason", None), ConnectionRefusedError
        ):
            return _format_error(
                f"Wisp daemon not reachable at {url}.\n"
                f"Start it from the wisp repo:\n"
                f"  cargo run --release -p wisp-runtime\n"
                f"Or set WISP_DAEMON_URL to a remote daemon."
            )
        return _format_error(f"Wisp request failed: {reason}")
    except urllib.error.HTTPError as e:
        try:
            err_body = e.read().decode("utf-8")
        except Exception:
            err_body = ""
        return _format_error(
            f"Wisp daemon returned HTTP {e.code}"
            + (f": {err_body}" if err_body else "")
        )
    except TimeoutError:
        return _format_error(
            f"Wisp call exceeded {timeout_ms} ms client-side timeout."
        )

    return _format_result(body)


def _format_result(r: dict) -> str:
    rc = int(r.get("rc", -1))
    elapsed_ms = int(r.get("elapsed_us", 0)) / 1000
    stdout = r.get("stdout", "")
    stderr = r.get("stderr", "")
    head = f"[wisp sandbox: rc={rc}, {elapsed_ms:.2f} ms]"
    parts = [head]
    if stdout:
        parts.append(f"--- stdout ---\n{stdout.rstrip(chr(10))}")
    if stderr:
        parts.append(f"--- stderr ---\n{stderr.rstrip(chr(10))}")
    if not stdout and not stderr:
        parts.append("(no output)")
    return "\n".join(parts)


def _format_error(msg: str) -> str:
    return f"[wisp sandbox: ERROR]\n{msg}"


if __name__ == "__main__":
    # FastMCP.run() defaults to stdio transport — exactly what Claude
    # Code (and other MCP clients) expect when they spawn the process
    # via a config entry.
    mcp.run()

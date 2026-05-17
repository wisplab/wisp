"""Wisp MCP server — exposes the local wisp-runtime daemon as MCP tools.

Run as a stdio MCP server. Claude Code (or any other MCP client) launches
this process, talks JSON-RPC over its stdin/stdout, and routes tool-calls
into the Wisp WASI Python sandbox running at $WISP_DAEMON_URL (default
http://localhost:9000).

Five tools:
  python_sandbox(code, timeout_ms)            — stateless per-call eval
  create_session()                            — start a stateful session
  session_eval(session_id, code, timeout_ms)  — eval in that session
  list_sessions()                             — what sessions are live
  delete_session(session_id)                  — close a session

Stateless vs session:
  python_sandbox       fresh interpreter every call (no state survives)
  session_eval         state survives across calls within the session
                       (sys.modules, user variables, etc.)

Same daemon contract as the OpenCode custom tool in
`examples/opencode-integration/`; this is just the MCP adapter for
clients that speak MCP.

Single-file by design — copy-pasteable into the user's MCP config. The
HTTP calls are inlined so this script doesn't need the wisp Python SDK
installed; only `mcp` is required.
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.request

from mcp.server.fastmcp import FastMCP


DEFAULT_DAEMON_URL = "http://localhost:9000"

mcp = FastMCP("wisp")


# ---- HTTP helpers --------------------------------------------------------

def _daemon_url() -> str:
    return os.environ.get("WISP_DAEMON_URL", DEFAULT_DAEMON_URL).rstrip("/")


def _request(method: str, path: str, body: dict | None = None, timeout: float = 35.0):
    """Send an HTTP request to the daemon. Returns parsed JSON or raises
    WispError-string on transport / HTTP error (kept as plain str so the
    caller can return it straight to the model)."""
    url = f"{_daemon_url()}{path}"
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {"Content-Type": "application/json"} if data else {}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            text = resp.read().decode("utf-8")
            return json.loads(text) if text else {}
    except urllib.error.HTTPError as e:
        # HTTPError MUST be caught before URLError — it's a subclass.
        try:
            err_body = e.read().decode("utf-8")
        except Exception:
            err_body = ""
        raise RuntimeError(
            f"Wisp daemon returned HTTP {e.code}"
            + (f": {err_body}" if err_body else "")
        ) from None
    except urllib.error.URLError as e:
        reason = getattr(e, "reason", str(e))
        if "Connection refused" in str(reason) or isinstance(
            getattr(e, "reason", None), ConnectionRefusedError
        ):
            raise RuntimeError(
                f"Wisp daemon not reachable at {_daemon_url()}.\n"
                f"Start it from the wisp repo:\n"
                f"  cargo run --release -p wisp-runtime\n"
                f"Or set WISP_DAEMON_URL to a remote daemon."
            ) from None
        raise RuntimeError(f"Wisp request failed: {reason}") from None


def _format_eval(r: dict) -> str:
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


def _err(msg: str) -> str:
    return f"[wisp sandbox: ERROR]\n{msg}"


# ---- Stateless eval ------------------------------------------------------

@mcp.tool()
def python_sandbox(code: str, timeout_ms: int = 30000) -> str:
    """Execute Python code in a PER-CALL WASI sandbox (no state carries over).

    Each call gets a fresh CPython 3.14 interpreter — sys.modules,
    user-defined names, and any imports are thrown away when the call
    returns. Use this anywhere you'd otherwise reach for `python -c '...'`
    on agent-generated code: the host filesystem and network are walled
    off behind the capability bridge (only what the daemon's
    $WISP_CAPABILITIES_JSON allowlists is reachable).

    For workflows that need state to survive across calls (notebook-style
    exploration, multi-step setup), use create_session + session_eval
    instead.

    Args:
        code: Python source. Top-level statements run in module scope.
            Use `print(...)` to surface values — the last expression is
            NOT auto-printed (not a REPL). Use `import wisp` to reach
            the host bridge (web_fetch, shell, file_read/write).
        timeout_ms: Per-call timeout in milliseconds. Default 30000.
    """
    try:
        body = _request("POST", "/v1/eval",
                        {"code": code, "timeout_ms": timeout_ms},
                        timeout=(timeout_ms / 1000) + 5)
    except RuntimeError as e:
        return _err(str(e))
    return _format_eval(body)


# ---- Sessions ------------------------------------------------------------

@mcp.tool()
def create_session() -> str:
    """Start a stateful Wisp session.

    Returns a session_id (UUID) for subsequent session_eval calls. The
    session holds a long-lived CPython interpreter — Python variables,
    imports, and module state survive across every session_eval call
    until you delete_session it (or the daemon's idle GC reaps it,
    default 5 min idle).

    Use sessions when consecutive evals need to share state (e.g. "load
    this dataset, then run several analyses on it"). For one-shot
    evals where state doesn't matter, python_sandbox is faster (no
    session bookkeeping).
    """
    try:
        body = _request("POST", "/v1/session")
    except RuntimeError as e:
        return _err(str(e))
    sid = body.get("session_id", "?")
    return f"[wisp session created]\nsession_id: {sid}"


@mcp.tool()
def session_eval(session_id: str, code: str, timeout_ms: int = 30000) -> str:
    """Execute Python code WITHIN an existing session — state survives.

    Variables, imports, and module state from earlier session_eval calls
    in the SAME session_id are still in scope. Each call's stdout/stderr
    is just what THAT call produced (not cumulative).

    Args:
        session_id: From create_session. 404 if it's expired or wrong.
        code: Python source. Same shape as python_sandbox.
        timeout_ms: Per-call timeout in milliseconds. Default 30000.
    """
    try:
        body = _request("POST", f"/v1/session/{session_id}/eval",
                        {"code": code, "timeout_ms": timeout_ms},
                        timeout=(timeout_ms / 1000) + 5)
    except RuntimeError as e:
        msg = str(e)
        if "HTTP 404" in msg:
            return _err(
                f"Session {session_id} not found "
                f"(expired, never created, or already deleted). "
                f"Call create_session to start a new one."
            )
        return _err(msg)
    return _format_eval(body)


@mcp.tool()
def list_sessions() -> str:
    """List currently-live Wisp sessions with their age and idle time.

    Returns one line per session: session_id, age (ms since created),
    idle (ms since last eval). Useful for diagnostics — usually the
    agent doesn't need to call this.
    """
    try:
        body = _request("GET", "/v1/sessions")
    except RuntimeError as e:
        return _err(str(e))
    if not body:
        return "[wisp sessions]\n(none)"
    lines = ["[wisp sessions]"]
    for s in body:
        lines.append(
            f"  {s.get('id', '?')}  "
            f"age={s.get('age_ms', '?')} ms, "
            f"idle={s.get('idle_ms', '?')} ms"
        )
    return "\n".join(lines)


@mcp.tool()
def delete_session(session_id: str) -> str:
    """Close a Wisp session. Frees the interpreter immediately.

    Idempotent — deleting a session that's already gone returns OK,
    not an error.
    """
    try:
        _request("DELETE", f"/v1/session/{session_id}")
    except RuntimeError as e:
        msg = str(e)
        if "HTTP 404" in msg:
            return f"[wisp session deleted]\n{session_id} (was already gone)"
        return _err(msg)
    return f"[wisp session deleted]\n{session_id}"


if __name__ == "__main__":
    # FastMCP.run() defaults to stdio transport — exactly what Claude
    # Code (and other MCP clients) expect when they spawn the process
    # via a config entry.
    mcp.run()

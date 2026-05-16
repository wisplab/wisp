"""Wisp Python client SDK.

Talks to a `wisp-runtime` daemon over HTTP. The daemon executes the code
in a fresh WASM Python sandbox and returns stdout/stderr/rc.

Usage:

    import wisp

    # Sync
    client = wisp.Client()                       # default localhost:9000
    result = client.eval('print("hi")')
    print(result.stdout, result.rc)

    # Async
    import asyncio
    async def main():
        async with wisp.AsyncClient() as c:
            r = await c.eval('print(2+2)')
            print(r.stdout)
    asyncio.run(main())

    # Context manager (sync) for symmetry
    with wisp.Client() as c:
        print(c.eval('print("hi")').stdout)

Stdlib only — no external deps. Async client wraps the sync HTTP call
on the asyncio default thread executor; that's enough concurrency to
saturate the daemon on a single host without pulling in aiohttp.

Pending (require daemon-side work):
  - Streaming output (chunked-encoded /v1/eval/stream endpoint)
  - Per-call capability override (daemon needs to accept caps in body)
"""
from __future__ import annotations

import asyncio
import json
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Optional


__version__ = "0.2.0"


@dataclass
class EvalResult:
    rc: int
    stdout: str
    stderr: str
    elapsed_us: int

    @property
    def ok(self) -> bool:
        return self.rc == 0

    @property
    def elapsed_ms(self) -> float:
        return self.elapsed_us / 1000.0


class WispError(RuntimeError):
    """Raised when the daemon returns a non-2xx response, or when the
    daemon isn't reachable.
    """


def _post_eval(base_url: str, code: str, timeout_ms: int, timeout: float) -> EvalResult:
    payload = json.dumps({"code": code, "timeout_ms": timeout_ms}).encode("utf-8")
    req = urllib.request.Request(
        f"{base_url}/v1/eval",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        try:
            err = json.loads(e.read().decode("utf-8"))
            msg = err.get("error", str(e))
        except Exception:
            msg = str(e)
        raise WispError(f"daemon returned HTTP {e.code}: {msg}") from None
    except urllib.error.URLError as e:
        raise WispError(f"daemon unreachable at {base_url}: {e.reason}") from None
    return EvalResult(
        rc=int(body["rc"]),
        stdout=body.get("stdout", ""),
        stderr=body.get("stderr", ""),
        elapsed_us=int(body.get("elapsed_us", 0)),
    )


def _get_healthz(base_url: str, timeout: float) -> bool:
    try:
        with urllib.request.urlopen(f"{base_url}/healthz", timeout=timeout) as r:
            return r.status == 200
    except Exception:
        return False


class Client:
    """Synchronous HTTP client for the wisp-runtime daemon.

    Usable as a context manager (no resources to release today — the
    context manager is provided for symmetry with AsyncClient and
    forward-compatibility with future connection pooling).
    """

    def __init__(self, base_url: str = "http://localhost:9000", timeout: float = 10.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_exc) -> None:
        pass

    def eval(self, code: str, timeout_ms: int = 5000) -> EvalResult:
        return _post_eval(self.base_url, code, timeout_ms, self.timeout)

    def healthz(self) -> bool:
        return _get_healthz(self.base_url, self.timeout)


class AsyncClient:
    """Asyncio-friendly client. Wraps the sync HTTP call via run_in_executor.

    Pure stdlib (no aiohttp dep). Concurrent calls are bounded by the
    default executor's thread count (asyncio's default ThreadPoolExecutor
    grows up to `min(32, os.cpu_count() + 4)`). For higher concurrency
    pass a custom executor via `asyncio.get_event_loop().set_default_executor(...)`.
    """

    def __init__(self, base_url: str = "http://localhost:9000", timeout: float = 10.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *_exc) -> None:
        pass

    async def eval(self, code: str, timeout_ms: int = 5000) -> EvalResult:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None, _post_eval, self.base_url, code, timeout_ms, self.timeout
        )

    async def healthz(self) -> bool:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None, _get_healthz, self.base_url, self.timeout
        )


# Module-level convenience: `wisp.eval(...)` without manually constructing
# a Client. Uses the default localhost:9000 base_url. Most non-trivial
# usage should explicitly create a Client for connection reuse later.
def eval(code: str, timeout_ms: int = 5000, base_url: Optional[str] = None) -> EvalResult:
    """One-shot eval against the default daemon. Convenience wrapper."""
    return Client(base_url or "http://localhost:9000").eval(code, timeout_ms)

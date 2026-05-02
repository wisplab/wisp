"""Wisp Python client SDK (V1 minimal).

Talks to a `wisp-runtime` daemon over HTTP. The daemon executes the code
in a fresh WASM Python sandbox and returns stdout/stderr/rc.

Usage:

    import wisp
    client = wisp.Client()              # default localhost:9000
    result = client.eval('print("hi")')
    print(result.stdout, result.rc)

Future versions will add: streaming output, capability whitelist per call,
per-request resource limits, async client. V1 keeps the surface small.
"""
from __future__ import annotations

import json
import urllib.error
import urllib.request
from dataclasses import dataclass


__version__ = "0.1.0"


@dataclass
class EvalResult:
    rc: int
    stdout: str
    stderr: str
    elapsed_us: int

    @property
    def ok(self) -> bool:
        return self.rc == 0


class WispError(RuntimeError):
    """Raised when the daemon returns a non-2xx response."""


class Client:
    def __init__(self, base_url: str = "http://localhost:9000", timeout: float = 10.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def eval(self, code: str, timeout_ms: int = 5000) -> EvalResult:
        payload = json.dumps({"code": code, "timeout_ms": timeout_ms}).encode("utf-8")
        req = urllib.request.Request(
            f"{self.base_url}/v1/eval",
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                body = json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            try:
                err = json.loads(e.read().decode("utf-8"))
                msg = err.get("error", str(e))
            except Exception:
                msg = str(e)
            raise WispError(f"daemon returned HTTP {e.code}: {msg}") from None
        return EvalResult(
            rc=int(body["rc"]),
            stdout=body.get("stdout", ""),
            stderr=body.get("stderr", ""),
            elapsed_us=int(body.get("elapsed_us", 0)),
        )

    def healthz(self) -> bool:
        try:
            with urllib.request.urlopen(f"{self.base_url}/healthz", timeout=self.timeout) as r:
                return r.status == 200
        except Exception:
            return False

"""Wisp sandbox-side helpers.

User code in the sandbox (Python) calls these. Each wraps the underlying
`_wisp.call_host(name: bytes, payload: bytes) -> bytes` import with a
friendlier signature: positional / keyword args, JSON encoding, base64
binary handling. Capabilities the host hasn't enabled raise WispError.

Examples:

    import wisp

    out = wisp.shell(["ls", "-la"])
    print(out.stdout, out.rc)

    text = wisp.file_read("/workspace/data.txt").decode()
    wisp.file_write("/workspace/output.json", b'{"ok": true}',
                    create_parents=True)

    raw = wisp.call_host("custom_capability", {"foo": "bar"})

This module is built into the runtime image (lives on PYTHONPATH inside
the snapshot). It's not on PyPI — it's only meaningful inside a wisp
sandbox, where `_wisp` is available.
"""
from __future__ import annotations

import base64 as _base64
import json as _json
from dataclasses import dataclass
from typing import Iterable, Mapping, Optional, Sequence, Union

import _wisp  # provided by the runtime; not importable outside wisp


__all__ = [
    "WispError",
    "ShellResult",
    "WebFetchResult",
    "shell",
    "file_read",
    "file_write",
    "web_fetch",
    "call_host",
]


class WispError(RuntimeError):
    """Raised when a capability call fails or isn't configured."""


@dataclass
class ShellResult:
    rc: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.rc == 0


@dataclass
class WebFetchResult:
    """Result of `wisp.web_fetch(...)`.

    `body` is always raw bytes — caller decodes (`.decode("utf-8")`,
    `json.loads(r.body)`, etc.). Headers are a dict of lower-cased
    keys. The host doesn't follow redirects, so 3xx responses are
    surfaced as-is — read `headers["location"]` to follow manually.
    """

    status: int
    headers: dict
    body: bytes

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300

    def json(self):
        """Decode body as JSON. Raises ValueError on non-JSON."""
        import json as _j
        return _j.loads(self.body)

    def text(self, encoding: str = "utf-8", errors: str = "strict") -> str:
        return self.body.decode(encoding, errors)


def call_host(
    name: str,
    payload: Union[bytes, str, Mapping, Sequence, None] = None,
) -> bytes:
    """Low-level: send `payload` to the host capability `name`, return raw bytes.

    `payload` accepts:
      - bytes        : passed through verbatim
      - str          : encoded as UTF-8
      - dict / list  : JSON-encoded, then UTF-8
      - None         : empty bytes

    Higher-level helpers (`shell`, `file_read`, ...) build on this.
    """
    if payload is None:
        body = b""
    elif isinstance(payload, bytes):
        body = payload
    elif isinstance(payload, str):
        body = payload.encode("utf-8")
    elif isinstance(payload, (Mapping, list, tuple)):
        body = _json.dumps(payload).encode("utf-8")
    else:
        raise TypeError(f"unsupported payload type: {type(payload).__name__}")
    try:
        return _wisp.call_host(name.encode("utf-8"), body)
    except RuntimeError as e:
        # Translate `_wisp.call_host(...) failed: host returned -N` into
        # something with the capability name on it for nicer tracebacks.
        raise WispError(f"capability {name!r}: {e}") from None


def shell(
    argv: Sequence[str],
    *,
    stdin: Optional[str] = None,
    cwd: Optional[str] = None,
) -> ShellResult:
    """Run an external command via the host's `shell` capability.

    The host enforces a command allowlist. `argv[0]` is exec'd directly —
    no shell metacharacters. To compose pipelines, use Python's `|` over
    multiple shell() calls instead.
    """
    payload: dict = {"argv": list(argv)}
    if stdin is not None:
        payload["stdin"] = stdin
    if cwd is not None:
        payload["cwd"] = cwd
    raw = call_host("shell", payload)
    obj = _json.loads(raw)
    return ShellResult(rc=int(obj["rc"]), stdout=obj["stdout"], stderr=obj["stderr"])


def file_read(path: str) -> bytes:
    """Read a file from the host. Path must be inside the host's allowlist."""
    raw = call_host("file_read", {"path": path})
    obj = _json.loads(raw)
    return _base64.b64decode(obj["contents_b64"])


def file_write(
    path: str,
    contents: Union[bytes, str],
    *,
    create_parents: bool = False,
) -> int:
    """Write to a file on the host. Returns bytes written."""
    if isinstance(contents, str):
        contents = contents.encode("utf-8")
    raw = call_host(
        "file_write",
        {
            "path": path,
            "contents_b64": _base64.b64encode(contents).decode("ascii"),
            "create_parents": create_parents,
        },
    )
    obj = _json.loads(raw)
    return int(obj["bytes_written"])


def web_fetch(
    url: str,
    *,
    method: str = "GET",
    headers: Optional[Mapping[str, str]] = None,
    data: Union[bytes, str, Mapping, list, tuple, None] = None,
) -> WebFetchResult:
    """Make an outbound HTTP(S) request via the host's `web_fetch` capability.

    The host enforces a host allowlist (e.g. `*.openai.com`) and a
    method allowlist (default GET-only). Redirects are NOT followed —
    if you need to handle them, check `result.status` and the
    `result.headers["location"]` value.

    `data` accepts the same shapes as `call_host`: bytes (verbatim),
    str (UTF-8 encoded), or dict/list/tuple (JSON-encoded; the
    `Content-Type: application/json` header is set automatically if not
    already present).
    """
    payload: dict = {"url": url, "method": method.upper()}
    final_headers = dict(headers) if headers else {}
    if data is None:
        body_bytes = b""
    elif isinstance(data, bytes):
        body_bytes = data
    elif isinstance(data, str):
        body_bytes = data.encode("utf-8")
    elif isinstance(data, (Mapping, list, tuple)):
        body_bytes = _json.dumps(data).encode("utf-8")
        if not any(k.lower() == "content-type" for k in final_headers):
            final_headers["Content-Type"] = "application/json"
    else:
        raise TypeError(f"unsupported data type: {type(data).__name__}")
    if body_bytes:
        payload["body_b64"] = _base64.b64encode(body_bytes).decode("ascii")
    if final_headers:
        payload["headers"] = final_headers
    raw = call_host("web_fetch", payload)
    obj = _json.loads(raw)
    body = _base64.b64decode(obj["body_b64"]) if obj.get("body_b64") else b""
    # Lower-case header keys for ergonomic case-insensitive lookup.
    hdrs = {k.lower(): v for k, v in (obj.get("headers") or {}).items()}
    return WebFetchResult(status=int(obj["status"]), headers=hdrs, body=body)

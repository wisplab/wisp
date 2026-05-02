# wisp-client (Python SDK V1)

Minimal HTTP client for the `wisp-runtime` daemon.

```python
import wisp
client = wisp.Client()  # default http://localhost:9000
r = client.eval("print('hello from sandbox'); 2 + 2")
print(r.rc, r.stdout, f"{r.elapsed_us} us")
```

## Install (dev)

```sh
cd wisp/sdks/python
pip install -e .
```

## What V1 does NOT have

- Streaming output (POST /v1/eval is one-shot)
- Per-call capability whitelist (server-side capabilities are global for now)
- Async client
- Auth (server is localhost-only by default)

These come once the daemon API stabilizes.

# Wisp deployment patterns

How to run the daemon at varying scales, from solo dev to multi-host
cluster. The runtime is intentionally simple — there's no built-in
clustering, no shared state, no leader election. Multi-host is a
*deployment pattern* on top of standard infrastructure, not a
feature of the runtime.

## Tier 1 — single process, local

For OpenCode / Claude Code integration on a developer laptop, or for
a small bot running on a single VPS:

```sh
WISP_CAPABILITIES_JSON=/etc/wisp/capabilities.json \
  wisp-runtime
```

One process holds:
  - The CPython wasm reactor (~38 MB).
  - A linear-memory snapshot mmapped from disk (~27 MB with numpy
    pre-imported).
  - A worker pool of `WISP_WORKERS` threads (default = available
    parallelism).
  - The sessions registry (process-local).

Capacity ceiling: roughly the worker pool count for concurrent
stateless evals, plus however many parallel sessions you spawn (each
session owns its own thread + its own copy of the wasm linear
memory).

## Tier 2 — multiple replicas behind a load balancer

For stateless workloads where every call is a fresh sandbox anyway,
horizontal scaling is the standard "N replicas + LB" pattern. The
Helm chart in `charts/wisp/` ships ready for this:

```sh
helm install wisp ./charts/wisp \
  --set replicaCount=5 \
  --set service.type=ClusterIP
```

Pods are interchangeable for `/v1/eval`. Each pod independently
captures its own snapshot at startup; they take ~30 seconds to become
ready (numpy pre-import). Bump `readinessProbe.initialDelaySeconds`
if your pods are starved.

Routing: any L7 HTTP load balancer works (nginx, Envoy, AWS ALB,
GCP HTTPS LB). No sticky session needed for `/v1/eval`.

### What this doesn't give you

  - Session affinity. `POST /v1/session` creates state on whichever
    pod handles the request, but subsequent `POST /v1/session/{id}/eval`
    must reach the *same* pod. Without sticky routing, the second
    call gets a 404 from the wrong pod.

## Tier 3 — replicas + sticky routing for sessions

For workloads that use sessions (notebook-style, multi-step agent
flows), add sticky routing by session_id:

### Option A — sticky by cookie (cheapest)

Have your client send the session_id back as a cookie:

```python
import wisp, requests
s = requests.Session()
resp = s.post("http://wisp.example.com/v1/session")
sid = resp.json()["session_id"]
s.cookies["wisp-session"] = sid
# Now all subsequent calls on `s` carry the cookie.
s.post(f"http://wisp.example.com/v1/session/{sid}/eval", json={...})
```

Configure the LB to do sticky sessions on the `wisp-session` cookie.
Works on AWS ALB (target group sticky sessions), nginx
(`hash $cookie_wisp_session`), and most L7 LBs.

### Option B — consistent-hash routing on the URL

Have the LB hash `:id` from `/v1/session/:id/eval` and route to the
same backend. nginx:

```nginx
upstream wisp_backends {
    hash $arg_session_id consistent;
    server wisp-0:9000;
    server wisp-1:9000;
    server wisp-2:9000;
}
```

Requires plumbing the id into the routing layer somehow (URL param,
header, etc.). More fragile than cookies but works without client
cooperation.

### Option C — gateway in front

Run a thin Rust/Go gateway between clients and the wisp pods. The
gateway holds the session_id → pod mapping and proxies accordingly.
This is the "build a real scheduler" path; about 200 lines of code.
Pattern:

```
[client] → [gateway: HashMap<session_id, pod_addr>] → [wisp pod]
```

When `/v1/session` is created, the gateway picks a pod (round-robin
or least-loaded), records the mapping, returns the id. Subsequent
calls look up the pod and proxy.

We don't ship this gateway today. The decision was: standard L7 LB
sticky routing handles ~95% of the real-world need at zero code
cost; a dedicated gateway only pays off at higher session-density
than we've seen.

## Capability config across hosts

The capability allowlist (`$WISP_CAPABILITIES_JSON`) should be
identical across replicas — otherwise the same eval gets different
"allowed" answers depending on which pod served it. Mount the same
ConfigMap into every pod (the Helm chart does this).

When you update capabilities, the Helm chart's
`checksum/capabilities` annotation forces a rolling restart so the
new config is picked up cleanly.

## Resource sizing per pod

| Workload | CPU | Memory |
|---|---|---|
| Pure-Python evals (no numpy) | 0.2 / 1 | 128 Mi / 256 Mi |
| Numpy-using evals | 0.5 / 2 | 256 Mi / 512 Mi |
| Sessions, ~10 concurrent | 1 / 4 | 512 Mi / 1 Gi |

Memory is dominated by per-Instance linear memory. Snapshot is
mmapped MAP_PRIVATE, so the snapshot bytes are shared read-only
across all Instances; only pages a per-call Instance modifies become
unique to that Instance.

## What's intentionally absent

  - No `daemon HA` mode. If one daemon crashes, in-flight sessions
    on it are lost. Standard k8s pod restart behavior.
  - No internal queue, no async job submission. Each request is
    handled within the request lifetime (subject to client / LB
    timeouts).
  - No metrics endpoint yet. `tracing` logs go to stdout; pipe to
    your aggregator of choice.
  - No multi-tenancy below the daemon. One process serves all
    tenants reachable to it. For per-tenant isolation, run separate
    daemons (separate Helm releases, separate namespaces).

These are deliberate v1 simplifications, not architectural dead-ends.
Adding any of them is incremental work on top of what's here.

# OpenCode + Wisp — reference integration

Drop-in OpenCode custom tool that routes Python execution through a local
Wisp WASI sandbox daemon. Each tool call gets a fresh CPython 3.14
interpreter — host filesystem and network are walled off; the agent
reaches the outside world only through capabilities the daemon's config
explicitly allows.

## Why bother

OpenCode's built-in `bash` tool lets the model do anything the user can.
That's fine for trusted local workflows but wrong for:

- running model-generated code that grew from "just compute this real
  quick" into "now `pip install requests && curl ...`"
- batch tool-call loops on a remote machine where the agent shouldn't
  touch anything outside `/tmp/agent-workspace`
- any time you'd want the explicit per-call answer to "what could this
  actually reach"

This integration adds a `python_sandbox` tool. Existing `bash` stays;
the agent learns to prefer the sandbox for Python and reaches for
`bash` only when the sandbox is the wrong fit.

## Layout

```
.opencode/tools/python_sandbox.ts   ← the tool itself
package.json                        ← @opencode-ai/plugin for type-check
tsconfig.json
wisp-capabilities.example.json      ← starting-point capability config
```

`.opencode/tools/` is OpenCode's project-local convention. Globally,
the same file works in `~/.config/opencode/tools/`.

## One-time setup

1. Build the Wisp daemon (from the wisp repo root):
   ```sh
   cargo build --release -p wisp-runtime
   ```
   This produces `target/release/wisp-runtime`.

2. Copy this example into the OpenCode-managed project where you want
   sandboxed Python:
   ```sh
   cp -r examples/opencode-integration/.opencode /path/to/your/project/
   ```
   Or, for global use, copy `python_sandbox.ts` into
   `~/.config/opencode/tools/`.

3. (Optional but recommended.) Author a capability config for the kind
   of work the agent will do:
   ```sh
   cp examples/opencode-integration/wisp-capabilities.example.json \
      ~/wisp-capabilities.json
   $EDITOR ~/wisp-capabilities.json
   ```
   The example allows shell `{echo, ls, wc, head, cat}`,
   `file_read/file_write` under `/tmp/wisp-workspace`, and `web_fetch`
   to GitHub + the major LLM APIs. Tighten or loosen as needed.

4. (Optional.) Type-check the tool locally:
   ```sh
   cd examples/opencode-integration
   pnpm install      # or npm / bun / yarn
   npx tsc --noEmit
   ```

## Run

In one terminal, start the daemon:

```sh
WISP_CAPABILITIES_JSON=$HOME/wisp-capabilities.json \
  cargo run --release -p wisp-runtime
# → "listening on http://127.0.0.1:9000"
```

In another, drive OpenCode normally. Once a session loads in the
project that has `.opencode/tools/python_sandbox.ts`, the new tool is
visible to the model alongside the built-ins.

## What the model sees

Tool name: `python_sandbox`

Args:
- `code: string` — Python source. `print(...)` to surface output.
- `timeout_ms?: number` — default 30000.

Return shape (rendered as plain text for the model):

```
[wisp sandbox: rc=0, 1.93 ms]
--- stdout ---
hello from a fresh python interpreter
```

Errors are clearly labeled:

```
[wisp sandbox: ERROR]
Wisp daemon not reachable at http://localhost:9000.
Start it from the wisp repo:
  cargo run --release -p wisp-runtime
```

## Configuration

| Env var             | Default                  | Notes                          |
|---------------------|--------------------------|--------------------------------|
| `WISP_DAEMON_URL`   | `http://localhost:9000`  | Set to point at a remote node  |

Daemon-side (set when starting `wisp-runtime`):

| Env var                    | Notes                                                      |
|----------------------------|------------------------------------------------------------|
| `WISP_CAPABILITIES_JSON`   | Path to capability config; absent = pure-Python only       |
| `WISP_BIND`                | Listen address; default `127.0.0.1:9000`                   |
| `WISP_WORKERS`             | Worker thread count; defaults to available parallelism     |

## Verifying it actually works

Try these prompts in OpenCode after the daemon is running. Expected
behavior is in italics.

> Use `python_sandbox` to print 2+2.

*Returns rc=0, stdout `4`, in 1–2 ms.*

> Use `python_sandbox` to read `/etc/passwd` and print the first line.

*With the example capability config: WispError "host returned -5"
because `/etc/passwd` is not under `/tmp/wisp-workspace`.*

> Use `python_sandbox` to fetch
> `https://api.github.com/zen` via `wisp.web_fetch` and print the
> response body.

*With the example config: returns the GitHub Zen aphorism. Without
`web_fetch` configured: WispError -2 (capability not enabled).*

> Use `python_sandbox` to compute the SHA-256 of "hello world".

*Returns
`b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9`.
Confirms `hashlib` works inside the sandbox.*

## What this integration deliberately does NOT do

- **State across calls.** Per-call fresh sandbox is the whole point.
  If the agent needs persistent state, stash it in `/tmp/wisp-workspace`
  via `wisp.file_write` and read it back next call. Persistent
  sessions are on the Wisp roadmap (E2B-style) but not shipped.
- **Streaming output.** The sandbox runs to completion, then returns.
  For long-running code this can hide useful progress. Splitting work
  into smaller tool calls is the workaround.
- **Replacing OpenCode's `bash`.** OpenCode lets a custom tool with
  the same name override built-ins, but `bash` is a different
  contract (full shell access, not sandboxed). Conflating the two
  would surprise users. Keep them separate.

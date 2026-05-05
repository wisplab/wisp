# Claude Code + Wisp — reference integration

Single-file MCP server that exposes the local `wisp-runtime` daemon as
a `python_sandbox` tool to Claude Code (or any other MCP client). Each
tool call gets a fresh CPython 3.14 interpreter — host filesystem and
network are walled off; the agent reaches the outside world only
through capabilities the daemon's config explicitly allows.

## Why route Claude Code's Python through Wisp

Claude Code's built-in `Bash` tool runs anything as the user. That's
fine for trusted local workflows but wrong for:

- running model-generated code that grew from "just compute this" into
  "now `pip install ... && curl ...`"
- batch loops where the agent shouldn't touch anything outside an
  explicit workspace
- pair-programming sessions where you want a hard answer to "what
  could this snippet actually reach"

This integration adds a `python_sandbox` MCP tool. `Bash` stays — the
model learns to prefer the sandbox for Python and reaches for `Bash`
only when the sandbox is the wrong fit.

## Layout

```
wisp_mcp_server.py          single-file stdio MCP server (~100 lines)
requirements.txt            mcp>=1.0
```

The server inlines the HTTP call to the daemon, so no need to
`pip install` Wisp's Python SDK separately.

## One-time setup

1. Build the Wisp daemon (from the wisp repo root):
   ```sh
   cargo build --release -p wisp-runtime
   ```

2. Create a venv and install `mcp`:
   ```sh
   cd examples/claude-code-integration
   python3 -m venv .venv
   ./.venv/bin/pip install -r requirements.txt
   ```

3. (Optional but recommended.) Author a capability config — defines
   what the sandbox can reach. A minimal example:
   ```sh
   cat > ~/wisp-capabilities.json <<'EOF'
   {
     "shell": {"allow_commands": ["echo", "ls", "wc", "head", "cat"]},
     "file_read":  {"allow_prefixes": ["/tmp/wisp-workspace"]},
     "file_write": {"allow_prefixes": ["/tmp/wisp-workspace"]},
     "web_fetch": {
       "allow_hosts": ["api.github.com", "raw.githubusercontent.com"],
       "allow_methods": ["GET"]
     }
   }
   EOF
   ```
   Without a config the sandbox can run pure Python (stdlib only) but
   cannot reach out at all.

4. Register the MCP server with Claude Code:
   ```sh
   claude mcp add wisp \
     -- /absolute/path/to/wisp/examples/claude-code-integration/.venv/bin/python \
        /absolute/path/to/wisp/examples/claude-code-integration/wisp_mcp_server.py
   ```
   Equivalent manual edit to `~/.claude.json`:
   ```json
   {
     "mcpServers": {
       "wisp": {
         "command": "/abs/path/.venv/bin/python",
         "args": ["/abs/path/wisp_mcp_server.py"],
         "env": {
           "WISP_DAEMON_URL": "http://localhost:9000"
         }
       }
     }
   }
   ```

## Run

In one terminal, start the daemon:

```sh
WISP_CAPABILITIES_JSON=$HOME/wisp-capabilities.json \
  cargo run --release -p wisp-runtime
# → "listening on http://127.0.0.1:9000"
```

Then start Claude Code in any project. The MCP tool is named
`mcp__wisp__python_sandbox` from Claude Code's perspective and shows
up in the available-tools list automatically.

## What the model sees

Tool name (in Claude Code): `mcp__wisp__python_sandbox`

Args:
- `code: string` — Python source. `print(...)` to surface output.
- `timeout_ms?: number` — default 30000.

Return shape (rendered as plain text):

```
[wisp sandbox: rc=0, 1.93 ms]
--- stdout ---
hello from a fresh python interpreter
```

Errors are clearly labeled, not raised:

```
[wisp sandbox: ERROR]
Wisp daemon not reachable at http://localhost:9000.
Start it from the wisp repo:
  cargo run --release -p wisp-runtime
```

## Verifying it actually works

In a Claude Code session, ask:

> Use the wisp python_sandbox to print 2+2.

*Expected: `[wisp sandbox: rc=0, 1.x ms]` with stdout `4`.*

> Use the wisp python_sandbox to read /etc/passwd via wisp.file_read.

*With the example capability config: `WispError "host returned -5"`
because /etc/passwd isn't under /tmp/wisp-workspace.*

> Use the wisp python_sandbox to fetch
> https://api.github.com/zen via wisp.web_fetch and print the response.

*With the example config: returns the GitHub Zen aphorism. Without
web_fetch configured: WispError -2 (capability disabled).*

## Configuration

| Env var (server)    | Default                  | Notes                          |
|---------------------|--------------------------|--------------------------------|
| `WISP_DAEMON_URL`   | `http://localhost:9000`  | Set in the MCP server's `env:` block to point at a remote daemon |

| Env var (daemon)           | Notes                                                |
|----------------------------|------------------------------------------------------|
| `WISP_CAPABILITIES_JSON`   | Path to capability config; absent = pure-Python only |
| `WISP_BIND`                | Listen address; default `127.0.0.1:9000`             |
| `WISP_WORKERS`             | Worker thread count                                  |

## What this integration deliberately does NOT do

- **Persistent state across calls.** Per-call freshness is the point.
  Stash state in `/tmp/wisp-workspace` via `wisp.file_write` and read
  it back next call. E2B-style persistent sessions are on the Wisp
  roadmap but not shipped.
- **Streaming output.** Sandbox runs to completion, then returns.
  Splitting work into smaller tool calls is the workaround.
- **Replacing Claude Code's `Bash`.** Different contracts (full shell
  access vs sandboxed Python). Conflating them would surprise users.
  Keep them separate.

## Same as the OpenCode integration?

Yes — same daemon contract, different transport. `examples/opencode-integration/`
exposes the daemon to OpenCode as a TS custom tool; this one exposes
it to Claude Code (and any MCP client) as an MCP server. The host
side (daemon, capabilities, snapshot) is identical.

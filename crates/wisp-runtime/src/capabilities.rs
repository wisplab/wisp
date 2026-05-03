//! Capability bridge: what the sandboxed Python can ask the host to do.
//!
//! Sandbox-side: `_wisp.call_host(name: bytes, payload: bytes) -> bytes`.
//! See `runtime/cpython-wasi/wisp_entry/wisp_entry.c` for the WASM import
//! and the Python module exposing it.
//!
//! Host-side (this file): a `Capabilities` registry. Each capability is
//! a name plus an allowlist; unknown or disabled names return -2 (the
//! "unknown capability" error code). The agent framework configures
//! which capabilities are exposed and what their allowlists look like.
//!
//! Wire format: payload and response are JSON strings (encoded as UTF-8
//! bytes). The sandbox-side `wisp` Python module wraps this so user
//! code calls `wisp.shell(["ls"])` instead of building JSON by hand.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tracing::warn;

/// Negative return codes the bridge uses. Mirrors the constants in
/// `examples/host-bridge/src/main.rs`; sandbox-side Python sees these
/// as `RuntimeError("host returned -N")`.
pub const RC_UNKNOWN_CAPABILITY: i32 = -2;
pub const RC_RESPONSE_TOO_LARGE: i32 = -3;
pub const RC_BAD_PAYLOAD: i32 = -4;
pub const RC_NOT_ALLOWED: i32 = -5;
pub const RC_INTERNAL: i32 = -6;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub shell: Option<ShellConfig>,
    #[serde(default)]
    pub file_read: Option<FileReadConfig>,
    #[serde(default)]
    pub file_write: Option<FileWriteConfig>,
    #[serde(default)]
    pub web_fetch: Option<WebFetchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellConfig {
    /// Allowlist of program basenames. `["ls", "grep"]` allows `ls -la`
    /// but not `/bin/sh -c 'ls'`. To allow shell metacharacters, the
    /// agent should compose them via Python — not via this bridge.
    pub allow_commands: Vec<String>,
    /// Cap on combined stdout+stderr the host returns. Default 1 MB.
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadConfig {
    /// Path-prefix allowlist. A request is allowed iff the canonical
    /// requested path begins with one of these prefixes (after symlink
    /// resolution). Glob support is intentionally NOT here — prefix is
    /// auditable; glob makes the policy harder to reason about.
    pub allow_prefixes: Vec<PathBuf>,
    #[serde(default = "default_max_read_bytes")]
    pub max_read_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWriteConfig {
    pub allow_prefixes: Vec<PathBuf>,
    #[serde(default = "default_max_write_bytes")]
    pub max_write_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchConfig {
    /// Host allowlist. Each entry is either an exact hostname
    /// (`api.openai.com`) or a wildcard prefix (`*.example.com`, which
    /// matches `api.example.com` and `foo.bar.example.com` but not
    /// `example.com` itself). The literal `*` allows any host — only
    /// useful for development.
    pub allow_hosts: Vec<String>,
    /// Allowed HTTP methods (case-insensitive). Defaults to GET only.
    #[serde(default = "default_web_methods")]
    pub allow_methods: Vec<String>,
    /// Cap on response body size. Default 16 MB.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    /// Cap on request body size. Default 4 MB.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Per-request timeout in milliseconds. Default 30 s.
    #[serde(default = "default_web_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_max_output() -> usize {
    1 << 20
}
fn default_max_read_bytes() -> usize {
    16 << 20 // 16 MB
}
fn default_max_write_bytes() -> usize {
    16 << 20
}
fn default_web_methods() -> Vec<String> {
    vec!["GET".to_string()]
}
fn default_max_response_bytes() -> usize {
    16 << 20
}
fn default_max_request_bytes() -> usize {
    4 << 20
}
fn default_web_timeout_ms() -> u64 {
    30_000
}

impl Capabilities {
    /// Dispatch a single host call. Returns the bytes the host wrote
    /// into the sandbox's response buffer, or a negative rc.
    pub fn dispatch(&self, name: &str, payload: &[u8]) -> Result<Vec<u8>, i32> {
        match name {
            "echo" => Ok(payload.to_vec()),
            "shell" => match &self.shell {
                Some(cfg) => dispatch_shell(cfg, payload),
                None => Err(RC_UNKNOWN_CAPABILITY),
            },
            "file_read" => match &self.file_read {
                Some(cfg) => dispatch_file_read(cfg, payload),
                None => Err(RC_UNKNOWN_CAPABILITY),
            },
            "file_write" => match &self.file_write {
                Some(cfg) => dispatch_file_write(cfg, payload),
                None => Err(RC_UNKNOWN_CAPABILITY),
            },
            "web_fetch" => match &self.web_fetch {
                Some(cfg) => dispatch_web_fetch(cfg, payload),
                None => Err(RC_UNKNOWN_CAPABILITY),
            },
            _ => Err(RC_UNKNOWN_CAPABILITY),
        }
    }
}

// ---- shell ---------------------------------------------------------------

#[derive(Deserialize)]
struct ShellRequest {
    /// Argv. First element is the program; subsequent are args. No shell
    /// wrapping — `argv[0]` is exec'd directly.
    argv: Vec<String>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Serialize)]
struct ShellResponse {
    rc: i32,
    stdout: String,
    stderr: String,
}

fn dispatch_shell(cfg: &ShellConfig, payload: &[u8]) -> Result<Vec<u8>, i32> {
    let req: ShellRequest = serde_json::from_slice(payload).map_err(|e| {
        warn!("shell: bad payload: {e}");
        RC_BAD_PAYLOAD
    })?;
    if req.argv.is_empty() {
        return Err(RC_BAD_PAYLOAD);
    }
    let basename = Path::new(&req.argv[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let allow: HashSet<&str> = cfg.allow_commands.iter().map(|s| s.as_str()).collect();
    if !allow.contains(basename) {
        warn!("shell: command {basename:?} not in allowlist");
        return Err(RC_NOT_ALLOWED);
    }

    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..]);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(stdin) = &req.stdin {
        use std::io::Write;
        use std::process::Stdio;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            warn!("shell spawn failed: {e}");
            RC_INTERNAL
        })?;
        if let Some(mut sin) = child.stdin.take() {
            let _ = sin.write_all(stdin.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| {
            warn!("shell wait failed: {e}");
            RC_INTERNAL
        })?;
        return encode_shell_output(out, cfg.max_output_bytes);
    }
    let out = cmd.output().map_err(|e| {
        warn!("shell run failed: {e}");
        RC_INTERNAL
    })?;
    encode_shell_output(out, cfg.max_output_bytes)
}

fn encode_shell_output(out: std::process::Output, max: usize) -> Result<Vec<u8>, i32> {
    let trim = |b: &[u8]| -> String {
        let take = b.len().min(max);
        String::from_utf8_lossy(&b[..take]).into_owned()
    };
    let resp = ShellResponse {
        rc: out.status.code().unwrap_or(-1),
        stdout: trim(&out.stdout),
        stderr: trim(&out.stderr),
    };
    serde_json::to_vec(&resp).map_err(|_| RC_INTERNAL)
}

// ---- file_read -----------------------------------------------------------

#[derive(Deserialize)]
struct FileReadRequest {
    path: String,
}

#[derive(Serialize)]
struct FileReadResponse {
    /// base64-encoded file contents. base64 instead of raw bytes so the
    /// JSON payload is text-safe (the sandbox-side wrapper decodes it).
    contents_b64: String,
    bytes: usize,
}

fn dispatch_file_read(cfg: &FileReadConfig, payload: &[u8]) -> Result<Vec<u8>, i32> {
    use base64::Engine;
    let req: FileReadRequest = serde_json::from_slice(payload).map_err(|_| RC_BAD_PAYLOAD)?;
    let path = canonical_or_err(&req.path)?;
    if !path_is_allowed(&path, &cfg.allow_prefixes) {
        warn!("file_read: {path:?} not in allowlist");
        return Err(RC_NOT_ALLOWED);
    }
    let meta = fs::metadata(&path).map_err(|_| RC_INTERNAL)?;
    if meta.len() as usize > cfg.max_read_bytes {
        return Err(RC_RESPONSE_TOO_LARGE);
    }
    let bytes = fs::read(&path).map_err(|_| RC_INTERNAL)?;
    let resp = FileReadResponse {
        contents_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        bytes: bytes.len(),
    };
    serde_json::to_vec(&resp).map_err(|_| RC_INTERNAL)
}

// ---- file_write ----------------------------------------------------------

#[derive(Deserialize)]
struct FileWriteRequest {
    path: String,
    contents_b64: String,
    #[serde(default)]
    create_parents: bool,
}

#[derive(Serialize)]
struct FileWriteResponse {
    bytes_written: usize,
}

fn dispatch_file_write(cfg: &FileWriteConfig, payload: &[u8]) -> Result<Vec<u8>, i32> {
    use base64::Engine;
    let req: FileWriteRequest = serde_json::from_slice(payload).map_err(|_| RC_BAD_PAYLOAD)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.contents_b64)
        .map_err(|_| RC_BAD_PAYLOAD)?;
    if bytes.len() > cfg.max_write_bytes {
        return Err(RC_RESPONSE_TOO_LARGE);
    }
    let path = PathBuf::from(&req.path);
    // For writes we check against the requested path before normalizing,
    // because the file may not exist yet (canonicalize would fail). We
    // still resolve the parent directory and the requested path's prefix.
    let parent = path.parent().ok_or(RC_BAD_PAYLOAD)?;
    let parent_canon = if req.create_parents {
        fs::create_dir_all(parent).map_err(|_| RC_INTERNAL)?;
        fs::canonicalize(parent).map_err(|_| RC_INTERNAL)?
    } else {
        fs::canonicalize(parent).map_err(|_| RC_INTERNAL)?
    };
    let target = parent_canon.join(path.file_name().ok_or(RC_BAD_PAYLOAD)?);
    if !path_is_allowed(&target, &cfg.allow_prefixes) {
        warn!("file_write: {target:?} not in allowlist");
        return Err(RC_NOT_ALLOWED);
    }
    fs::write(&target, &bytes).map_err(|_| RC_INTERNAL)?;
    let resp = FileWriteResponse {
        bytes_written: bytes.len(),
    };
    serde_json::to_vec(&resp).map_err(|_| RC_INTERNAL)
}

// ---- helpers -------------------------------------------------------------

fn canonical_or_err(p: &str) -> Result<PathBuf, i32> {
    fs::canonicalize(p).map_err(|_| RC_INTERNAL)
}

/// True iff `path` (assumed canonical) starts with any of the allow
/// prefixes (after canonicalizing each prefix). Symlink-resolved
/// equality avoids the classic `/workspace/../etc/passwd` bypass.
fn path_is_allowed(path: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|p| {
        let p_canon = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        path.starts_with(p_canon)
    })
}

// ---- web_fetch -----------------------------------------------------------

#[derive(Deserialize)]
struct WebFetchRequest {
    url: String,
    #[serde(default)]
    method: Option<String>,
    /// Header map. Pass `Host` and `Content-Length` are stripped — ureq
    /// sets those itself based on the URL and body.
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    /// base64-encoded request body. Empty / absent means no body.
    #[serde(default)]
    body_b64: Option<String>,
}

#[derive(Serialize)]
struct WebFetchResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body_b64: String,
    bytes: usize,
}

fn dispatch_web_fetch(cfg: &WebFetchConfig, payload: &[u8]) -> Result<Vec<u8>, i32> {
    use base64::Engine;
    let req: WebFetchRequest = serde_json::from_slice(payload).map_err(|e| {
        warn!("web_fetch: bad payload: {e}");
        RC_BAD_PAYLOAD
    })?;

    // Parse URL — reject schemes other than http/https up front so we
    // don't accidentally serve `file://` or `gopher://` from the host.
    let parsed = url::Url::parse(&req.url).map_err(|e| {
        warn!("web_fetch: bad url {:?}: {e}", req.url);
        RC_BAD_PAYLOAD
    })?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            warn!("web_fetch: rejected scheme {other:?}");
            return Err(RC_NOT_ALLOWED);
        }
    }
    let host = parsed.host_str().ok_or_else(|| {
        warn!("web_fetch: url has no host");
        RC_BAD_PAYLOAD
    })?;
    if !host_allowed(host, &cfg.allow_hosts) {
        warn!("web_fetch: host {host:?} not in allowlist");
        return Err(RC_NOT_ALLOWED);
    }

    let method = req
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let allow_methods: HashSet<String> = cfg
        .allow_methods
        .iter()
        .map(|m| m.to_ascii_uppercase())
        .collect();
    if !allow_methods.contains(&method) {
        warn!("web_fetch: method {method:?} not in allowlist");
        return Err(RC_NOT_ALLOWED);
    }

    let body = if let Some(b64) = req.body_b64.as_deref().filter(|s| !s.is_empty()) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| {
                warn!("web_fetch: bad body base64: {e}");
                RC_BAD_PAYLOAD
            })?;
        if decoded.len() > cfg.max_request_bytes {
            return Err(RC_RESPONSE_TOO_LARGE);
        }
        Some(decoded)
    } else {
        None
    };

    // Fresh agent per call. Disable redirects: a redirect to a different
    // host would silently bypass our allowlist. Let user code follow them
    // explicitly by checking the status code and Location header.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .redirects(0)
        .user_agent("wisp/0.1")
        .build();

    let mut req_builder = agent.request(&method, parsed.as_str());
    if let Some(headers) = &req.headers {
        for (k, v) in headers {
            // Skip headers ureq must set itself. Comparison is
            // case-insensitive per HTTP spec.
            if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
                continue;
            }
            req_builder = req_builder.set(k, v);
        }
    }

    let response = match body {
        Some(b) => req_builder.send_bytes(&b),
        None => req_builder.call(),
    };

    // ureq treats >= 400 as Err(Status). For a fetch capability we want
    // to surface the HTTP response itself (status, headers, body) rather
    // than translating 404 into RC_NOT_ALLOWED. Recover the response
    // from the Status error.
    let resp = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => {
            warn!("web_fetch: transport error: {t}");
            return Err(RC_INTERNAL);
        }
    };

    let status = resp.status();
    let mut headers = BTreeMap::new();
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            headers.insert(name, v.to_string());
        }
    }
    // Cap how many bytes we'll actually read so a hostile server can't
    // OOM the daemon by streaming forever.
    let mut reader = resp.into_reader().take((cfg.max_response_bytes + 1) as u64);
    let mut buf = Vec::new();
    use std::io::Read;
    if let Err(e) = reader.read_to_end(&mut buf) {
        warn!("web_fetch: read body failed: {e}");
        return Err(RC_INTERNAL);
    }
    if buf.len() > cfg.max_response_bytes {
        warn!(
            "web_fetch: response body exceeded max_response_bytes ({} > {})",
            buf.len(),
            cfg.max_response_bytes
        );
        return Err(RC_RESPONSE_TOO_LARGE);
    }

    let resp_obj = WebFetchResponse {
        status,
        headers,
        body_b64: base64::engine::general_purpose::STANDARD.encode(&buf),
        bytes: buf.len(),
    };
    serde_json::to_vec(&resp_obj).map_err(|_| RC_INTERNAL)
}

/// Host allowlist match. Patterns:
///   `*`             — wildcard, matches anything
///   `*.example.com` — matches `api.example.com`, `a.b.example.com`,
///                     but NOT bare `example.com`
///   `api.example.com` — exact match (case-insensitive)
fn host_allowed(host: &str, allow: &[String]) -> bool {
    for pattern in allow {
        if pattern == "*" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix("*.") {
            // Wildcard requires at least one label before the suffix.
            if let Some(rest) = host.strip_suffix(suffix) {
                if rest.ends_with('.') && rest.len() > 1 {
                    return true;
                }
            }
            continue;
        }
        if pattern.eq_ignore_ascii_case(host) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allowed_exact_and_wildcard() {
        let allow = vec![
            "api.openai.com".to_string(),
            "*.example.com".to_string(),
        ];
        assert!(host_allowed("api.openai.com", &allow));
        assert!(host_allowed("API.OpenAI.com", &allow));
        assert!(host_allowed("api.example.com", &allow));
        assert!(host_allowed("a.b.example.com", &allow));
        assert!(!host_allowed("example.com", &allow));
        assert!(!host_allowed("evil.com", &allow));
        assert!(!host_allowed("notexample.com", &allow));
    }

    #[test]
    fn host_allowed_wildcard_all() {
        let allow = vec!["*".to_string()];
        assert!(host_allowed("anything.example", &allow));
        assert!(host_allowed("localhost", &allow));
    }

    #[test]
    fn host_allowed_empty() {
        assert!(!host_allowed("api.openai.com", &[]));
    }
}

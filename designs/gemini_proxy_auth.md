# Gemini Proxy & Keyless Authentication Design

## 1. Overview & Motivation

Developers using managed corporate workstations or local LLM proxy sidecars (such as `litellm` or local HTTP-to-RPC gateways) often authenticate via ambient workstation credentials (such as Application Default Credentials, OAuth, or mTLS) rather than static `GEMINI_API_KEY` strings.

To make Sashiko work seamlessly out of the box with workstation Gemini quota without requiring an explicit API key, while keeping the Rust codebase 100% upstreamable and free of vendor-internal endpoints:

1. **Keyless HTTP Proxy & Bearer Auth in `GeminiClient`:**
   - Support running `GeminiClient` without `GEMINI_API_KEY` when targeting a custom `base_url` or managed local proxy.
   - Support optional `Authorization: Bearer <token>` headers via `GEMINI_AUTH_TOKEN` or a configurable `auth_token_command` (e.g., `gcloud auth print-access-token`).
2. **Managed Local Proxy Lifecycle (`proxy_command`):**
   - Allow `GeminiClient` to spawn and supervise a local HTTP proxy subprocess on demand when `proxy_command` (or `GEMINI_PROXY_COMMAND`) is configured.
   - Automatically substitute `{port_file}` in the command, wait asynchronously for the proxy to write its bound TCP port, and route requests to `http://localhost:<port>`.
   - Ensure the proxy process is automatically terminated when `GeminiClient` is dropped or if the parent process exits (`PR_SET_PDEATHSIG(SIGTERM)` on Linux).
3. **Zero-Hassle Auto-Detection via External Helper Script:**
   - Provide `scripts/gemini-proxy-helper.sh` which implements `--check` and `--port-file <path>`.
   - When `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, and `proxy_command` are all unset, `GeminiClient` checks if `scripts/gemini-proxy-helper.sh --check` succeeds. If so, it automatically uses `scripts/gemini-proxy-helper.sh --port-file {port_file}` as the proxy command.
   - This keeps `src/ai/gemini.rs` completely vendor-neutral while providing a zero-configuration experience on supported workstations.

## 2. Configuration (`Settings.toml` & Environment Variables)

### `Settings.toml` (`[ai.gemini]`)
```toml
[ai.gemini]
# Optional custom base URL (overrides default https://generativelanguage.googleapis.com)
# base_url = "http://localhost:8000"

# Optional shell command to launch a local HTTP proxy daemon.
# Supports {port_file} placeholder for dynamic port discovery.
# proxy_command = "scripts/gemini-proxy-helper.sh --port-file {port_file}"

# Optional shell command to fetch a Bearer token for Authorization header.
# auth_token_command = "gcloud auth print-access-token"
```

### Environment Variables
- `GEMINI_API_KEY` / `LLM_API_KEY`: Standard API key (sent as `x-goog-api-key` header if non-empty).
- `GOOGLE_GEMINI_BASE_URL` / `GEMINI_BASE_URL`: Custom HTTP base URL.
- `GEMINI_PROXY_COMMAND`: Command to launch a managed local proxy.
- `GEMINI_AUTH_TOKEN`: Static OAuth/Bearer token (sent as `Authorization: Bearer <token>`).
- `GEMINI_AUTH_TOKEN_COMMAND`: Command to dynamically obtain an OAuth/Bearer token.

## 3. Proxy Lifecycle & Process Supervision

To avoid orphaned proxy processes or port collisions:
- **Lazy Async Initialization:** Proxy startup (which can take ~2-3 seconds for self-extracting archives) occurs lazily on the first AI request using `tokio::sync::OnceCell<String>`, keeping CLI help and unit tests instantaneous.
- **Process Cleanup (`ProxyGuard`):**
  - The spawned `std::process::Child` is held in an `Arc<Mutex<Option<Child>>>` inside a `ProxyGuard` struct attached to `GeminiClient`.
  - When `ProxyGuard` is dropped, it sends `SIGTERM`/`SIGKILL` to the child process and removes the temporary port file.
  - On Linux (`#[cfg(target_os = "linux")]`), `pre_exec` sets `libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM)` so the kernel automatically reaps the proxy if Sashiko terminates abruptly.

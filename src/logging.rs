use std::io::{self, Write};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Command execution mode for selecting default log verbosity when `RUST_LOG`
/// and `--debug` are not explicitly set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogCommandKind {
    /// Long-running daemon or benchmark process (`Settings.log_level` applies,
    /// defaulting to `"warn"` if settings failed to load).
    Daemon,
    /// Interactive local review, notes, or codebase scan (`"warn"` by default).
    InteractiveCli,
    /// Long-running repository sync (`"warn,sashiko=info"` by default).
    RepoSync,
    /// Worker subprocess (`Settings.log_level` applies, falling back to
    /// `"warn,sashiko=info"` when settings failed to load).
    Worker,
}

/// Scopes a bare log level (`"info"`, `"debug"`, `"trace"`) to `sashiko` while
/// clamping third-party dependencies (`hyper`, `reqwest`, `h2`, `rustls`,
/// `libsql`, `lettre`) to `warn`. Explicit `EnvFilter` directive strings
/// containing `=` or `,` are returned verbatim.
pub fn scope_log_level(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('=') || trimmed.contains(',') {
        return trimmed.to_string();
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "trace" => "warn,sashiko=trace".to_string(),
        "debug" => "warn,sashiko=debug".to_string(),
        "info" => "warn,sashiko=info".to_string(),
        "warn" => "warn".to_string(),
        "error" => "error".to_string(),
        "off" => "off".to_string(),
        _ => trimmed.to_string(),
    }
}

/// Resolves the fallback `EnvFilter` specification when `RUST_LOG` is unset.
pub fn resolve_log_filter_spec(
    debug_flag: bool,
    kind: LogCommandKind,
    configured_level: Option<&str>,
) -> String {
    if debug_flag {
        return "warn,sashiko=debug".to_string();
    }
    match kind {
        LogCommandKind::InteractiveCli => "warn".to_string(),
        LogCommandKind::RepoSync => "warn,sashiko=info".to_string(),
        LogCommandKind::Daemon => configured_level
            .map(scope_log_level)
            .unwrap_or_else(|| "warn".to_string()),
        LogCommandKind::Worker => configured_level
            .map(scope_log_level)
            .unwrap_or_else(|| "warn,sashiko=info".to_string()),
    }
}

/// Builds a non-panicking [`EnvFilter`] honoring `RUST_LOG` first and falling
/// back to `fallback_spec`, returning an optional warning message if either
/// filter string was malformed.
pub fn build_env_filter(
    rust_log_env: Option<&str>,
    fallback_spec: &str,
) -> (EnvFilter, Option<String>) {
    let mut warning = None;
    if let Some(env_val) = rust_log_env.map(str::trim).filter(|s| !s.is_empty()) {
        match EnvFilter::try_new(env_val) {
            Ok(filter) => return (filter, None),
            Err(e) => {
                warning = Some(format!(
                    "Invalid RUST_LOG filter {env_val:?} ({e}); falling back to {fallback_spec:?}"
                ));
            }
        }
    }

    match EnvFilter::try_new(fallback_spec) {
        Ok(filter) => (filter, warning),
        Err(e) => (
            EnvFilter::new("warn,sashiko=info"),
            Some(format!(
                "Invalid log_level filter {fallback_spec:?} ({e}); falling back to \"warn,sashiko=info\""
            )),
        ),
    }
}

pub struct IgnoreBrokenPipe<W>(pub W);

impl<'a, W> MakeWriter<'a> for IgnoreBrokenPipe<W>
where
    W: MakeWriter<'a> + 'a,
{
    type Writer = IgnoreBrokenPipeWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        IgnoreBrokenPipeWriter(self.0.make_writer())
    }
}

pub struct IgnoreBrokenPipeWriter<W>(W);

impl<W: Write> Write for IgnoreBrokenPipeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.write(buf) {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(buf.len()),
            res => res,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.flush() {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            res => res,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct TestBuf(Arc<Mutex<Vec<u8>>>);

    struct TestWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for TestBuf {
        type Writer = TestWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TestWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn test_scope_log_level_clamps_dependencies_and_preserves_directives() {
        assert_eq!(scope_log_level("info"), "warn,sashiko=info");
        assert_eq!(scope_log_level(" DEBUG "), "warn,sashiko=debug");
        assert_eq!(scope_log_level("trace"), "warn,sashiko=trace");
        assert_eq!(scope_log_level("warn"), "warn");
        assert_eq!(scope_log_level("error"), "error");
        assert_eq!(scope_log_level("off"), "off");
        assert_eq!(scope_log_level("info,hyper=debug"), "info,hyper=debug");
        assert_eq!(scope_log_level("sashiko::ai=trace"), "sashiko::ai=trace");
    }

    #[test]
    fn test_resolve_log_filter_spec_by_command_and_debug_flag() {
        assert_eq!(
            resolve_log_filter_spec(false, LogCommandKind::Daemon, Some("info")),
            "warn,sashiko=info"
        );
        assert_eq!(
            resolve_log_filter_spec(true, LogCommandKind::Daemon, Some("info")),
            "warn,sashiko=debug"
        );
        assert_eq!(
            resolve_log_filter_spec(false, LogCommandKind::InteractiveCli, Some("info")),
            "warn"
        );
        assert_eq!(
            resolve_log_filter_spec(true, LogCommandKind::InteractiveCli, Some("info")),
            "warn,sashiko=debug"
        );
        assert_eq!(
            resolve_log_filter_spec(false, LogCommandKind::RepoSync, Some("warn")),
            "warn,sashiko=info"
        );
        assert_eq!(
            resolve_log_filter_spec(false, LogCommandKind::Worker, None),
            "warn,sashiko=info"
        );
    }

    #[test]
    fn test_build_env_filter_scopes_sashiko_and_handles_invalid_inputs() {
        let (default_filter, warn_msg) = build_env_filter(None, "warn,sashiko=info");
        assert!(warn_msg.is_none());

        let buf = TestBuf::default();
        let subscriber = tracing_subscriber::registry().with(default_filter).with(
            tracing_subscriber::fmt::layer()
                .with_writer(buf.clone())
                .with_ansi(false),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "sashiko::reviewer", "daemon milestone");
            tracing::debug!(target: "sashiko::reviewer", "daemon internal step");
            tracing::info!(target: "hyper::proto::h1", "http frame noise");
            tracing::warn!(target: "hyper::proto::h1", "http transport warning");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("daemon milestone"));
        assert!(!out.contains("daemon internal step"));
        assert!(!out.contains("http frame noise"));
        assert!(out.contains("http transport warning"));

        let (debug_filter, debug_warn) = build_env_filter(None, "warn,sashiko=debug");
        assert!(debug_warn.is_none());
        let debug_buf = TestBuf::default();
        let debug_sub = tracing_subscriber::registry().with(debug_filter).with(
            tracing_subscriber::fmt::layer()
                .with_writer(debug_buf.clone())
                .with_ansi(false),
        );
        tracing::subscriber::with_default(debug_sub, || {
            tracing::debug!(target: "sashiko::reviewer", "daemon internal step");
            tracing::debug!(target: "hyper::proto::h1", "http debug noise");
        });
        let debug_out = String::from_utf8(debug_buf.0.lock().unwrap().clone()).unwrap();
        assert!(debug_out.contains("daemon internal step"));
        assert!(!debug_out.contains("http debug noise"));

        // Invalid RUST_LOG or log_level never panics and surfaces a diagnostic warning.
        let (_, bad_env_warn) = build_env_filter(Some("=invalid["), "warn,sashiko=info");
        assert!(bad_env_warn.is_some());
        let (_, bad_cfg_warn) = build_env_filter(None, "=invalid[");
        assert!(bad_cfg_warn.is_some());
    }
}

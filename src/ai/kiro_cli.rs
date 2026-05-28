// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! AI provider that shells out to `kiro-cli acp`.
//!
//! By default, Kiro runs under an isolated temporary agent with all native
//! tools disabled. A deny-all pre-tool hook acts as a defensive backstop.
//! This makes the provider a pure completion backend: Sashiko's own ToolBox
//! remains the only tool execution layer.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::claude_cli::{build_prompt, parse_inner_response};
use super::token_budget::TokenBudget;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiUsage, ClassifyAiError, DEFAULT_RETRY_AFTER,
    ProviderCapabilities,
};
use crate::utils::redact_secret;

pub struct KiroCliProvider {
    pub(crate) model: String,
    pub(crate) binary: String,
    pub(crate) agent: Option<String>,
    pub(crate) context_window_size: usize,
    state: Arc<KiroProviderState>,
    #[cfg(test)]
    turn_timeout_override: Option<Duration>,
    #[cfg(test)]
    idle_timeout_override: Option<Duration>,
}

type StderrPreview = Arc<Mutex<String>>;
type StdoutPreview = Arc<Mutex<String>>;
const STDERR_PREVIEW_LIMIT: usize = 4096;
const STDOUT_PREVIEW_LIMIT: usize = 4096;

struct KiroChildGuard {
    child: Option<Child>,
}

impl KiroChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> Result<&mut Child> {
        self.child.as_mut().context("kiro child already cleaned up")
    }

    async fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl Drop for KiroChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
        } else {
            let _ = child.start_kill();
        }
    }
}

// Kiro-specific containment policy.
//
// Runtime `kiro-cli acp -vvv` on Kiro 2.4.1 reports:
//   MinimumThroughputBodyOptions: 1 byte/sec, 5s grace, 1s check window.
//   RetryConfig: adaptive, 3 max attempts, 1s initial backoff, 10s max backoff.
//   TimeoutConfig: all four SDK timeouts follow api.timeout, e.g. 900000ms -> 900s.
//
// The 5s stalled-stream guard is hardcoded in Kiro's client path and is
// independent of api.timeout. These constants therefore bound Sashiko's
// retries around a provider that can drop otherwise valid long LLM streams
// after only a few seconds below the throughput floor.
const KIRO_RETRY_AFTER: Duration = Duration::from_secs(1);
// Classification only needs enough provider text to catch known Kiro/AWS error
// markers. Error data can include verbose payloads, so keep marker scans bounded.
const KIRO_MAX_CLASSIFICATION_TEXT_CHARS: usize = 64 * 1024;

// Count Sashiko-level retries of the same logical prompt. Each one may already
// include Kiro's internal SDK retry budget, so this is intentionally low. These
// defaults can be overridden with the matching SASHIKO__AI__KIRO_* env vars.
const KIRO_MAX_TRANSIENT_ATTEMPTS_PER_TURN: usize = 3;

// Same failure class twice usually means the same Kiro stalled-stream condition
// repeated, not that another identical replay is likely to add useful signal.
const KIRO_MAX_SAME_TRANSIENT_STREAK_PER_TURN: usize = 2;

// Keep one pathological AI turn below the review active-time guard while still
// allowing legitimate long kernel-review turns to finish.
const KIRO_MAX_TURN_WALL_CLOCK: Duration = Duration::from_secs(20 * 60);

// This is ACP JSON-RPC stdout-line idle time, not Kiro's upstream HTTP-byte
// idle time. Successful benchmark turns have shown ACP line gaps above 300s and
// up to roughly 460s, so this must stay well above Kiro's 5s HTTP guard.
const KIRO_ACP_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

// Provider-level breaker. The values are operational containment knobs, not
// Kiro protocol constants: enough failures to avoid tripping on a single bad
// turn, low enough to stop all workers joining a Kiro outage.
const KIRO_CIRCUIT_FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
const KIRO_CIRCUIT_FAILURE_THRESHOLD: usize = 8;
const KIRO_CIRCUIT_COOLDOWN: Duration = Duration::from_secs(10 * 60);

// Keep terminal Kiro errors bounded while still retaining enough provider
// request IDs for upstream correlation.
const KIRO_MAX_RETAINED_REQUEST_IDS: usize = 12;
// Retry budgets are keyed by prompt fingerprint. Successful and terminal paths
// clear them explicitly; this TTL bounds abandoned entries in long-lived
// processes if an outer layer stops replaying a prompt after a transient.
const KIRO_BUDGET_ENTRY_TTL: Duration = Duration::from_secs(60 * 60);
const KIRO_MAX_ACP_ERROR_DATA_CHARS: usize = 4096;

// The review binary can recreate providers for whole-review retries. Sharing
// Kiro state in-process keeps the circuit breaker effective across those
// recreations without changing generic worker/review retry code. This is a
// process-local breaker, not a daemon-wide or benchmark-wide coordination
// mechanism across separate review worker processes.
#[cfg(not(test))]
static KIRO_PROVIDER_STATE: OnceLock<Arc<KiroProviderState>> = OnceLock::new();

const KIRO_PERMANENT_MARKERS: &[&str] = &[
    "validationerror",
    "validationexception",
    "accessdeniederror",
    "accessdeniedexception",
    "servicequotaexceedederror",
    "contentlengthexceedsthreshold",
    "invalidconversationid",
    "invalidmodelid",
    "invalid conversation history",
    "prompt is too long",
    "contextwindowoverflow",
    "monthlylimitreached",
    "unauthorized",
    "autherror",
];
const KIRO_STREAM_CONTEXT_MARKERS: &[&str] = &[
    "codewhispererchatresponsestream",
    "qdeveloperchatresponsestream",
    "chatresponsestream",
    "chatresponsestreamerror",
    "chatresponsestreamconversestreamerror",
    "chatresponsestreamunmarshaller",
];
const KIRO_STRONG_STREAM_FAILURE_MARKERS: &[&str] = &[
    // Kiro wraps AWS Smithy/reqwest stream failures in JSON-RPC -32603 errors.
    // Include both the older wrapper strings and the runtime -vvv throughput
    // strings so classification still works if Kiro changes the outer wording.
    "recverrorstreamtimeout",
    "unexpectedeof",
    "unexpected eof",
    "failed to receive the next event",
    "failed to receive the next message",
    "encountered an error in the response stream",
    "request or response body error",
    "connection closed before message completed",
    "throughputbelowminimum",
    "throughput below minimum",
    "minimumthroughputbody",
    "minimumthroughputdownloadbody",
    "minimum throughput",
    "minimum upload throughput",
    "stalled stream",
    "stalled-stream",
    "grace period ended",
];
const KIRO_WEAK_TRANSIENT_MARKERS: &[&str] = &[
    "recverrorunknown",
    "dispatchfailure",
    "timedout",
    "timeouterror",
    "operation timed out",
    "dispatch failure",
    "error trying to connect",
];
const KIRO_RATE_LIMIT_MARKERS: &[&str] = &[
    "throttlingerror",
    "throttlingexception",
    "too many requests",
    "rate limit",
    "ratelimit",
];
const KIRO_PROVIDER_TRANSIENT_MARKERS: &[&str] = &[
    "serviceunavailableerror",
    "modeltemporarilyunavailable",
    "modeloverloadederror",
    "requesttimeoutexception",
];

#[cfg(test)]
fn kiro_env_usize(_name: &str, default: usize) -> usize {
    default
}

#[cfg(not(test))]
fn kiro_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[cfg(test)]
fn kiro_env_duration_secs(_name: &str, default: Duration) -> Duration {
    default
}

#[cfg(not(test))]
fn kiro_env_duration_secs(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

fn kiro_max_transient_attempts_per_turn() -> usize {
    kiro_env_usize(
        "SASHIKO__AI__KIRO_MAX_TRANSIENT_ATTEMPTS_PER_TURN",
        KIRO_MAX_TRANSIENT_ATTEMPTS_PER_TURN,
    )
}

fn kiro_max_same_transient_streak_per_turn() -> usize {
    kiro_env_usize(
        "SASHIKO__AI__KIRO_MAX_SAME_TRANSIENT_STREAK_PER_TURN",
        KIRO_MAX_SAME_TRANSIENT_STREAK_PER_TURN,
    )
}

fn kiro_max_turn_wall_clock() -> Duration {
    kiro_env_duration_secs(
        "SASHIKO__AI__KIRO_MAX_TURN_WALL_CLOCK_SECS",
        KIRO_MAX_TURN_WALL_CLOCK,
    )
}

fn kiro_acp_idle_timeout() -> Duration {
    kiro_env_duration_secs(
        "SASHIKO__AI__KIRO_ACP_IDLE_TIMEOUT_SECS",
        KIRO_ACP_IDLE_TIMEOUT,
    )
}

fn kiro_acp_turn_timeout() -> Duration {
    // The generic provider timeout defaults to 300s, which is shorter than
    // Kiro's benchmark-observed ACP idle gaps and the Kiro turn wall-clock
    // budget. Kiro therefore uses its provider-specific wall-clock guard as
    // the outer timeout; tune it with SASHIKO__AI__KIRO_MAX_TURN_WALL_CLOCK_SECS.
    kiro_max_turn_wall_clock()
}

fn kiro_circuit_failure_window() -> Duration {
    kiro_env_duration_secs(
        "SASHIKO__AI__KIRO_CIRCUIT_FAILURE_WINDOW_SECS",
        KIRO_CIRCUIT_FAILURE_WINDOW,
    )
}

fn kiro_circuit_failure_threshold() -> usize {
    kiro_env_usize(
        "SASHIKO__AI__KIRO_CIRCUIT_FAILURE_THRESHOLD",
        KIRO_CIRCUIT_FAILURE_THRESHOLD,
    )
}

fn kiro_circuit_cooldown() -> Duration {
    kiro_env_duration_secs(
        "SASHIKO__AI__KIRO_CIRCUIT_COOLDOWN_SECS",
        KIRO_CIRCUIT_COOLDOWN,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KiroFailureKind {
    Stream,
    ProviderAvailability,
    Timeout,
    OtherTransient,
}

#[derive(Default)]
struct KiroProviderState {
    turn_budgets: Mutex<HashMap<u64, KiroTurnRetryBudget>>,
    circuit_breaker: Mutex<KiroCircuitBreaker>,
}

struct KiroTurnRetryBudget {
    first_seen: Instant,
    transient_attempts: usize,
    same_failure_streak: usize,
    last_failure_kind: Option<KiroFailureKind>,
    request_ids: Vec<String>,
    last_error: String,
}

struct KiroTransientFailure<'a> {
    request_key: u64,
    model: &'a str,
    prompt_chars: usize,
    prompt_tokens: usize,
    failure_kind: KiroFailureKind,
    error_message: String,
    request_ids: Vec<String>,
}

struct KiroTurnFailureSnapshot {
    exhausted_reason: Option<String>,
    transient_attempts: usize,
    same_failure_streak: usize,
    elapsed: Duration,
    request_ids: Vec<String>,
    last_error: String,
}

#[derive(Default)]
struct KiroCircuitBreaker {
    failures: VecDeque<Instant>,
    open_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct KiroRpcReadStats {
    lines_seen: usize,
    max_idle_gap: Duration,
}

#[derive(Default)]
struct RpcReadOptions<'a> {
    idle_timeout: Option<Duration>,
    read_stats: Option<&'a mut KiroRpcReadStats>,
    chunks: Option<&'a mut Vec<String>>,
}

impl KiroRpcReadStats {
    fn record_line(&mut self, idle_gap: Duration) {
        self.lines_seen += 1;
        self.max_idle_gap = self.max_idle_gap.max(idle_gap);
    }
}

impl KiroTurnRetryBudget {
    fn new(now: Instant) -> Self {
        Self {
            first_seen: now,
            transient_attempts: 0,
            same_failure_streak: 0,
            last_failure_kind: None,
            request_ids: Vec::new(),
            last_error: String::new(),
        }
    }

    fn record_failure(
        &mut self,
        now: Instant,
        failure: &KiroTransientFailure<'_>,
    ) -> KiroTurnFailureSnapshot {
        self.transient_attempts += 1;
        if self.last_failure_kind == Some(failure.failure_kind) {
            self.same_failure_streak += 1;
        } else {
            self.same_failure_streak = 1;
            self.last_failure_kind = Some(failure.failure_kind);
        }

        for request_id in &failure.request_ids {
            if !self.request_ids.iter().any(|seen| seen == request_id) {
                self.request_ids.push(request_id.clone());
            }
        }
        if self.request_ids.len() > KIRO_MAX_RETAINED_REQUEST_IDS {
            let excess = self.request_ids.len() - KIRO_MAX_RETAINED_REQUEST_IDS;
            // Keep the newest request IDs; older IDs are less useful once the
            // per-turn retry budget has accumulated many provider failures.
            self.request_ids.drain(..excess);
        }

        self.last_error = truncate_for_log(&failure.error_message, 1200);
        let elapsed = now.duration_since(self.first_seen);
        KiroTurnFailureSnapshot {
            exhausted_reason: self.exhausted_reason(elapsed, failure.failure_kind),
            transient_attempts: self.transient_attempts,
            same_failure_streak: self.same_failure_streak,
            elapsed,
            request_ids: self.request_ids.clone(),
            last_error: self.last_error.clone(),
        }
    }

    fn exhausted_reason(&self, elapsed: Duration, failure_kind: KiroFailureKind) -> Option<String> {
        let max_attempts = kiro_max_transient_attempts_per_turn();
        let max_same_streak = kiro_max_same_transient_streak_per_turn();
        let max_wall_clock = kiro_max_turn_wall_clock();

        if self.transient_attempts >= max_attempts {
            Some(format!("transient retry attempts reached {}", max_attempts))
        } else if self.same_failure_streak >= max_same_streak {
            Some(format!(
                "same transient failure kind repeated {} times ({:?})",
                max_same_streak, failure_kind
            ))
        } else if elapsed >= max_wall_clock {
            Some(format!(
                "turn wall-clock budget exceeded {:.2}s",
                max_wall_clock.as_secs_f64()
            ))
        } else {
            None
        }
    }

    fn is_stale(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.first_seen) >= KIRO_BUDGET_ENTRY_TTL
    }
}

impl KiroProviderState {
    async fn start_turn(&self, request_key: u64, started_at: Instant) -> Duration {
        self.prune_stale_request_state(started_at).await;
        let mut turn_budgets = self.turn_budgets.lock().await;
        let first_seen = turn_budgets
            .entry(request_key)
            .or_insert_with(|| KiroTurnRetryBudget::new(started_at))
            .first_seen;
        kiro_max_turn_wall_clock().saturating_sub(started_at.saturating_duration_since(first_seen))
    }

    async fn reject_if_circuit_open(&self) -> Result<()> {
        let now = Instant::now();
        let mut circuit = self.circuit_breaker.lock().await;
        if let Some(open_until) = circuit.open_until {
            if open_until > now {
                let remaining = open_until.duration_since(now);
                return Err(kiro_terminal_error(format!(
                    "KiroCircuitBreakerOpen: kiro-cli ACP stream failures exceeded provider budget; retrying new Kiro turns is paused for {:.2}s",
                    remaining.as_secs_f64()
                )));
            }

            circuit.open_until = None;
            circuit.failures.clear();
            warn!(
                provider = "kiro-acp",
                "kiro-cli ACP circuit breaker cooldown elapsed"
            );
        }
        Ok(())
    }

    async fn report_success(&self, request_key: u64) {
        self.clear_request_state(request_key).await;
    }

    async fn clear_request_state(&self, request_key: u64) {
        self.turn_budgets.lock().await.remove(&request_key);
    }

    async fn prune_stale_request_state(&self, now: Instant) {
        self.turn_budgets
            .lock()
            .await
            .retain(|_, budget| !budget.is_stale(now));
    }

    async fn report_transient_failure(&self, failure: KiroTransientFailure<'_>) -> Result<()> {
        let now = Instant::now();
        let snapshot = self.record_turn_failure(now, &failure).await;

        if let Some(reason) = &snapshot.exhausted_reason {
            if let Some(cooldown) = self.record_circuit_failure(now).await {
                Self::log_circuit_opened(&failure, cooldown);
            }
            return Err(Self::turn_budget_exhausted_error(
                &failure, &snapshot, reason,
            ));
        }

        if let Some(cooldown) = self.record_circuit_failure(now).await {
            self.clear_request_state(failure.request_key).await;
            return Err(Self::circuit_open_error(&failure, &snapshot, cooldown));
        }

        Self::log_retained_transient_failure(&failure, &snapshot);
        Ok(())
    }

    async fn record_turn_failure(
        &self,
        now: Instant,
        failure: &KiroTransientFailure<'_>,
    ) -> KiroTurnFailureSnapshot {
        let mut turn_budgets = self.turn_budgets.lock().await;
        let budget = turn_budgets
            .entry(failure.request_key)
            .or_insert_with(|| KiroTurnRetryBudget::new(now));

        let snapshot = budget.record_failure(now, failure);
        if snapshot.exhausted_reason.is_some() {
            turn_budgets.remove(&failure.request_key);
        }
        snapshot
    }

    fn turn_budget_exhausted_error(
        failure: &KiroTransientFailure<'_>,
        snapshot: &KiroTurnFailureSnapshot,
        reason: &str,
    ) -> anyhow::Error {
        warn!(
            provider = "kiro-acp",
            request_key = request_key_for_log(failure.request_key),
            transient_attempts = snapshot.transient_attempts,
            same_failure_streak = snapshot.same_failure_streak,
            elapsed_secs = snapshot.elapsed.as_secs_f64(),
            failure_kind = ?failure.failure_kind,
            request_ids = ?snapshot.request_ids,
            "kiro-cli ACP per-turn transient retry budget exhausted"
        );
        kiro_terminal_error(format!(
            "KiroTransientBudgetExceeded: {} for one AI turn (model={}, prompt_chars={}, estimated_prompt_tokens={}, transient_attempts={}, same_failure_streak={}, elapsed_secs={:.2}, failure_kind={:?}, request_ids={:?}, last_error={})",
            reason,
            failure.model,
            failure.prompt_chars,
            failure.prompt_tokens,
            snapshot.transient_attempts,
            snapshot.same_failure_streak,
            snapshot.elapsed.as_secs_f64(),
            failure.failure_kind,
            snapshot.request_ids,
            snapshot.last_error
        ))
    }

    fn log_circuit_opened(failure: &KiroTransientFailure<'_>, cooldown: Duration) {
        warn!(
            provider = "kiro-acp",
            request_key = request_key_for_log(failure.request_key),
            cooldown_secs = cooldown.as_secs_f64(),
            failure_kind = ?failure.failure_kind,
            "kiro-cli ACP process-local circuit breaker opened"
        );
    }

    fn circuit_open_error(
        failure: &KiroTransientFailure<'_>,
        snapshot: &KiroTurnFailureSnapshot,
        cooldown: Duration,
    ) -> anyhow::Error {
        Self::log_circuit_opened(failure, cooldown);
        kiro_terminal_error(format!(
            "KiroCircuitBreakerOpen: kiro-cli ACP saw at least {} transient stream failures within {:.2}s; pausing Kiro turns for {:.2}s (last_failure_kind={:?}, last_error={})",
            kiro_circuit_failure_threshold(),
            kiro_circuit_failure_window().as_secs_f64(),
            cooldown.as_secs_f64(),
            failure.failure_kind,
            snapshot.last_error
        ))
    }

    fn log_retained_transient_failure(
        failure: &KiroTransientFailure<'_>,
        snapshot: &KiroTurnFailureSnapshot,
    ) {
        warn!(
            provider = "kiro-acp",
            request_key = request_key_for_log(failure.request_key),
            transient_attempts = snapshot.transient_attempts,
            same_failure_streak = snapshot.same_failure_streak,
            elapsed_secs = snapshot.elapsed.as_secs_f64(),
            failure_kind = ?failure.failure_kind,
            request_ids = ?snapshot.request_ids,
            "kiro-cli ACP transient failure retained within per-turn budget"
        );
    }

    async fn record_circuit_failure(&self, now: Instant) -> Option<Duration> {
        let mut circuit = self.circuit_breaker.lock().await;
        circuit.prune(now, kiro_circuit_failure_window());
        circuit.failures.push_back(now);
        if circuit.failures.len() < kiro_circuit_failure_threshold() {
            return None;
        }

        circuit.failures.clear();
        let cooldown = kiro_circuit_cooldown();
        circuit.open_until = Some(now + cooldown);
        Some(cooldown)
    }
}

impl KiroCircuitBreaker {
    fn prune(&mut self, now: Instant, window: Duration) {
        while let Some(oldest) = self.failures.front().copied() {
            if now.duration_since(oldest) <= window {
                break;
            }
            self.failures.pop_front();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KiroMarkerClass {
    Permanent,
    StrongStreamFailure,
    StreamContextWeakTransient,
    RateLimit,
    ProviderAvailability,
    Unclassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KiroAcpErrorClassification {
    class: AiErrorClass,
    marker_class: KiroMarkerClass,
    matched_marker: Option<&'static str>,
    retry_blocked_by_side_effect_gate: bool,
}

impl KiroAcpErrorClassification {
    fn new(
        class: AiErrorClass,
        marker_class: KiroMarkerClass,
        matched_marker: Option<&'static str>,
    ) -> Self {
        Self {
            class,
            marker_class,
            matched_marker,
            retry_blocked_by_side_effect_gate: false,
        }
    }

    fn apply_side_effect_gate(mut self, side_effects_seen: bool) -> Self {
        if side_effects_seen
            && matches!(
                self.class,
                AiErrorClass::RateLimit { .. } | AiErrorClass::Transient { .. }
            )
        {
            self.class = AiErrorClass::Fatal;
            self.retry_blocked_by_side_effect_gate = true;
        }
        self
    }
}

fn find_marker(text: &str, markers: &'static [&'static str]) -> Option<&'static str> {
    markers.iter().copied().find(|marker| text.contains(marker))
}

fn normalized_acp_error_text(message: &str, data: Option<&Value>) -> String {
    let mut text = String::new();
    let mut text_chars = 0;
    push_ascii_lowercase_bounded(
        &mut text,
        &mut text_chars,
        message,
        KIRO_MAX_CLASSIFICATION_TEXT_CHARS,
    );
    if let Some(data) = data {
        push_ascii_lowercase_bounded(
            &mut text,
            &mut text_chars,
            "\n",
            KIRO_MAX_CLASSIFICATION_TEXT_CHARS,
        );
        match data {
            Value::String(s) => push_ascii_lowercase_bounded(
                &mut text,
                &mut text_chars,
                s,
                KIRO_MAX_CLASSIFICATION_TEXT_CHARS,
            ),
            _ => push_json_value_for_marker_scan(
                &mut text,
                &mut text_chars,
                data,
                KIRO_MAX_CLASSIFICATION_TEXT_CHARS,
            ),
        }
    }
    text
}

fn push_ascii_lowercase_bounded(
    buffer: &mut String,
    buffer_chars: &mut usize,
    text: &str,
    max_chars: usize,
) {
    let mut remaining = max_chars.saturating_sub(*buffer_chars);
    for ch in text.chars() {
        if remaining == 0 {
            break;
        }
        buffer.push(ch.to_ascii_lowercase());
        *buffer_chars += 1;
        remaining -= 1;
    }
}

fn push_json_value_for_marker_scan(
    buffer: &mut String,
    buffer_chars: &mut usize,
    value: &Value,
    max_chars: usize,
) {
    if *buffer_chars >= max_chars {
        return;
    }

    match value {
        Value::Null => push_ascii_lowercase_bounded(buffer, buffer_chars, "null", max_chars),
        Value::Bool(value) => {
            push_ascii_lowercase_bounded(buffer, buffer_chars, &value.to_string(), max_chars)
        }
        Value::Number(value) => {
            push_ascii_lowercase_bounded(buffer, buffer_chars, &value.to_string(), max_chars)
        }
        Value::String(value) => {
            push_ascii_lowercase_bounded(buffer, buffer_chars, value, max_chars)
        }
        Value::Array(values) => {
            push_ascii_lowercase_bounded(buffer, buffer_chars, "[", max_chars);
            for value in values {
                if *buffer_chars >= max_chars {
                    break;
                }
                push_json_value_for_marker_scan(buffer, buffer_chars, value, max_chars);
                push_ascii_lowercase_bounded(buffer, buffer_chars, ",", max_chars);
            }
            push_ascii_lowercase_bounded(buffer, buffer_chars, "]", max_chars);
        }
        Value::Object(values) => {
            push_ascii_lowercase_bounded(buffer, buffer_chars, "{", max_chars);
            for (key, value) in values {
                if *buffer_chars >= max_chars {
                    break;
                }
                push_ascii_lowercase_bounded(buffer, buffer_chars, key, max_chars);
                push_ascii_lowercase_bounded(buffer, buffer_chars, ":", max_chars);
                push_json_value_for_marker_scan(buffer, buffer_chars, value, max_chars);
                push_ascii_lowercase_bounded(buffer, buffer_chars, ",", max_chars);
            }
            push_ascii_lowercase_bounded(buffer, buffer_chars, "}", max_chars);
        }
    }
}

#[cfg(test)]
fn classify_kiro_acp_error(code: i64, message: &str, data: Option<&Value>) -> AiErrorClass {
    classify_kiro_acp_error_with_details(code, message, data, false).class
}

fn classify_kiro_acp_error_with_details(
    code: i64,
    message: &str,
    data: Option<&Value>,
    side_effects_seen: bool,
) -> KiroAcpErrorClassification {
    if code != -32603 {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Fatal,
            KiroMarkerClass::Unclassified,
            None,
        );
    }

    let text = normalized_acp_error_text(message, data);
    if let Some(marker) = find_marker(&text, KIRO_PERMANENT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Fatal,
            KiroMarkerClass::Permanent,
            Some(marker),
        );
    }

    // Rate-limit markers win over stream markers. Kiro can wrap throttling in
    // generic response-stream text, and the reviewer should honor the slower
    // quota backoff rather than the short stream-failure retry delay.
    if let Some(marker) = find_marker(&text, KIRO_RATE_LIMIT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::RateLimit {
                retry_after: DEFAULT_RETRY_AFTER,
            },
            KiroMarkerClass::RateLimit,
            Some(marker),
        )
        .apply_side_effect_gate(side_effects_seen);
    }

    if let Some(marker) = find_marker(&text, KIRO_STRONG_STREAM_FAILURE_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::StrongStreamFailure,
            Some(marker),
        )
        .apply_side_effect_gate(side_effects_seen);
    }

    if find_marker(&text, KIRO_STREAM_CONTEXT_MARKERS).is_some()
        && let Some(marker) = find_marker(&text, KIRO_WEAK_TRANSIENT_MARKERS)
    {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::StreamContextWeakTransient,
            Some(marker),
        )
        .apply_side_effect_gate(side_effects_seen);
    }

    if let Some(marker) = find_marker(&text, KIRO_PROVIDER_TRANSIENT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::ProviderAvailability,
            Some(marker),
        )
        .apply_side_effect_gate(side_effects_seen);
    }

    KiroAcpErrorClassification::new(AiErrorClass::Fatal, KiroMarkerClass::Unclassified, None)
}

fn acp_error_data_shape(data: Option<&Value>) -> &'static str {
    match data {
        None => "missing",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct KiroCliError {
    message: String,
    class: AiErrorClass,
    request_ids: Vec<String>,
}

impl KiroCliError {
    fn new(message: String, class: AiErrorClass) -> Self {
        Self {
            message,
            class,
            request_ids: Vec::new(),
        }
    }

    fn with_request_ids(message: String, class: AiErrorClass, request_ids: Vec<String>) -> Self {
        Self {
            message,
            class,
            request_ids,
        }
    }
}

impl ClassifyAiError for KiroCliError {
    fn ai_error_class(&self) -> AiErrorClass {
        self.class
    }
}

fn kiro_terminal_error(message: String) -> anyhow::Error {
    // Generic worker/review retry loops already fail fast on ReviewError.
    // Attach that context here so Kiro budget/circuit exhaustion remains a
    // Kiro-local implementation detail instead of adding provider-specific
    // checks to generic retry code. Keeping KiroCliError as the inner error
    // also preserves provider-specific classification and tests.
    let review_context = crate::worker::prompts::ReviewError::BudgetExceeded(format!(
        "kiro backend terminal failure: {message}"
    ));

    Err::<(), KiroCliError>(KiroCliError::new(message, AiErrorClass::Fatal))
        .context(review_context)
        .unwrap_err()
}

fn request_key_for_log(request_key: u64) -> String {
    format!("{request_key:016x}")
}

/// Agent JSON for the isolated no-tool Sashiko provider agent.
const AGENT_JSON: &str = r#"{
  "name": "sashiko-provider",
  "description": "Stateless Sashiko completion backend. Native Kiro tools are disabled.",
  "prompt": "Follow the user-provided instructions exactly.",
  "mcpServers": {},
  "tools": [],
  "allowedTools": [],
  "resources": [],
  "includeMcpJson": false,
  "hooks": {
    "preToolUse": [
      {
        "command": ".kiro/hooks/deny-all-tools.sh"
      }
    ]
  }
}"#;

/// Shell script that denies all Kiro native tool invocations.
const DENY_ALL_HOOK: &str = "#!/bin/sh\n\
echo \"Kiro native tools are disabled for the Sashiko provider\" >&2\n\
exit 1\n";

/// Build the kiro-cli command arguments.
fn build_args(model: &str, agent: &str) -> Vec<String> {
    vec![
        "acp".to_string(),
        "--agent".to_string(),
        agent.to_string(),
        "--model".to_string(),
        model.to_string(),
    ]
}

/// Optional diagnostic verbosity for Kiro ACP.
///
/// Kiro writes `-v`/`-vvv` SDK traces to stdout interleaved with ACP JSON-RPC,
/// so this is opt-in. `read_rpc_response` treats those trace lines as
/// malformed stdout and does not let them reset ACP idle metrics.
fn verbose_args_from_env() -> Vec<String> {
    let Ok(raw) = std::env::var("KIRO_ACP_VERBOSE") else {
        return Vec::new();
    };
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || matches!(value.as_str(), "0" | "false" | "no" | "off") {
        return Vec::new();
    }

    let level = value
        .parse::<usize>()
        .ok()
        .or_else(|| {
            value
                .chars()
                .all(|ch| ch == 'v')
                .then_some(value.chars().count())
        })
        .unwrap_or(1)
        .clamp(1, 3);

    (0..level).map(|_| "-v".to_string()).collect()
}

/// Create the isolated temporary workspace with a no-tool agent and deny-all hook.
/// Returns the TempDir (must be kept alive for the duration of the process).
fn create_isolated_workspace() -> Result<TempDir> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join(".kiro/agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join("sashiko-provider.json"), AGENT_JSON)?;

    let hooks_dir = tmp.path().join(".kiro/hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("deny-all-tools.sh");
    std::fs::write(&hook_path, DENY_ALL_HOOK)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(tmp)
}

async fn write_rpc(stdin: &mut ChildStdin, id: u64, method: &str, params: Value) -> Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&msg)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn write_rpc_checked(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: Value,
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
) -> Result<()> {
    if let Err(e) = write_rpc(stdin, id, method, params).await {
        anyhow::bail!(
            "kiro-cli ACP write failed for {}: {}{}",
            method,
            e,
            diagnostic_context(stderr_preview, stdout_preview).await
        );
    }
    Ok(())
}

fn timeout_error_class(side_effects_seen: bool) -> AiErrorClass {
    if side_effects_seen {
        AiErrorClass::Fatal
    } else {
        AiErrorClass::Transient {
            retry_after: DEFAULT_RETRY_AFTER,
        }
    }
}

fn kiro_idle_timeout_error(
    idle_timeout: Duration,
    target_id: u64,
    side_effects_seen: &AtomicBool,
) -> KiroCliError {
    let side_effects_seen = side_effects_seen.load(Ordering::Relaxed);
    KiroCliError::new(
        format!(
            "kiro-cli ACP idle timeout after {:.2}s waiting for response {} (side_effects_seen={})",
            idle_timeout.as_secs_f64(),
            target_id,
            side_effects_seen
        ),
        timeout_error_class(side_effects_seen),
    )
}

fn is_acp_json_rpc_message(msg: &Value) -> bool {
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }

    let is_request_or_notification = msg.get("method").and_then(Value::as_str).is_some();
    let is_response =
        msg.get("id").is_some() && (msg.get("result").is_some() ^ msg.get("error").is_some());

    is_request_or_notification || is_response
}

fn is_acp_response_for_target(msg: &Value, target_id: u64) -> bool {
    msg.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && msg.get("id").and_then(Value::as_u64) == Some(target_id)
        && (msg.get("result").is_some() ^ msg.get("error").is_some())
}

async fn read_rpc_response(
    lines: &mut Lines<BufReader<ChildStdout>>,
    target_id: u64,
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
    side_effects_seen: &AtomicBool,
    mut options: RpcReadOptions<'_>,
) -> Result<Value> {
    let mut last_acp_line_at = Instant::now();

    loop {
        // Timeout against valid ACP JSON-RPC traffic, not any stdout byte. This
        // matters because Kiro verbose mode writes human trace lines to stdout;
        // those lines are useful diagnostics but must not make the ACP stream
        // look alive for Sashiko's protocol-level watchdog.
        let next_line = async { lines.next_line().await };
        let next_line = if let Some(idle_timeout) = options.idle_timeout {
            let elapsed_since_acp = last_acp_line_at.elapsed();
            let Some(remaining) = idle_timeout.checked_sub(elapsed_since_acp) else {
                return Err(
                    kiro_idle_timeout_error(idle_timeout, target_id, side_effects_seen).into(),
                );
            };
            timeout(remaining, next_line)
                .await
                .map_err(|_| kiro_idle_timeout_error(idle_timeout, target_id, side_effects_seen))?
        } else {
            next_line.await
        };

        let line = match next_line {
            Ok(Some(line)) => line,
            Ok(None) => {
                anyhow::bail!(
                    "kiro-cli ACP exited before response {}{}",
                    target_id,
                    diagnostic_context(stderr_preview, stdout_preview).await
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "kiro-cli ACP stdout read failed before response {}: {}{}",
                    target_id,
                    e,
                    diagnostic_context(stderr_preview, stdout_preview).await
                );
            }
        };
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let redacted_line = record_malformed_stdout_line(stdout_preview, &line).await;
                debug!(
                    "Ignoring malformed ACP stdout line: {} ({})",
                    redacted_line, e
                );
                continue;
            }
        };

        let targets_response_id = msg.get("id").and_then(Value::as_u64) == Some(target_id)
            && msg.get("method").and_then(Value::as_str).is_none();
        if targets_response_id && !is_acp_response_for_target(&msg, target_id) {
            let redacted_line = record_malformed_stdout_line(stdout_preview, &line).await;
            return Err(KiroCliError::new(
                format!(
                    "kiro-cli ACP malformed JSON-RPC response for id {}: {}{}",
                    target_id,
                    redacted_line,
                    diagnostic_context(stderr_preview, stdout_preview).await
                ),
                AiErrorClass::Fatal,
            )
            .into());
        }

        if !is_acp_json_rpc_message(&msg) {
            let redacted_line = record_malformed_stdout_line(stdout_preview, &line).await;
            debug!("Ignoring non-ACP JSON stdout line: {}", redacted_line);
            continue;
        }

        // Only valid JSON-RPC lines count as ACP activity and feed the observed
        // max idle gap metric. This keeps KIRO_ACP_VERBOSE traces from
        // corrupting the benchmark's ACP silence measurements.
        let now = Instant::now();
        let idle_gap = now.duration_since(last_acp_line_at);
        last_acp_line_at = now;
        if let Some(stats) = options.read_stats.as_deref_mut() {
            stats.record_line(idle_gap);
        }

        if is_acp_response_for_target(&msg, target_id) {
            if let Some(error) = msg.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
                let raw_message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown ACP error");
                let data = error.get("data");
                let side_effects_seen = side_effects_seen.load(Ordering::Relaxed);
                let classification = classify_kiro_acp_error_with_details(
                    code,
                    raw_message,
                    data,
                    side_effects_seen,
                );
                let message = redact_kiro_diagnostic_text(raw_message);
                debug!(
                    provider = "kiro-acp",
                    json_rpc_code = code,
                    message = %message,
                    data_shape = acp_error_data_shape(data),
                    classification = ?classification.class,
                    marker_class = ?classification.marker_class,
                    matched_marker = ?classification.matched_marker,
                    side_effects_seen = side_effects_seen,
                    retry_blocked_by_side_effect_gate = classification.retry_blocked_by_side_effect_gate,
                    "kiro-cli ACP classified error"
                );
                let data_context = acp_error_data_context(error);
                let diagnostic = diagnostic_context(stderr_preview, stdout_preview).await;
                let message = format!(
                    "kiro-cli ACP error {}: {}{}{}",
                    code, message, data_context, diagnostic
                );
                let request_ids = extract_request_ids(&message);
                return Err(KiroCliError::with_request_ids(
                    message,
                    classification.class,
                    request_ids,
                )
                .into());
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }

        if acp_update_has_side_effect(&msg) {
            side_effects_seen.store(true, Ordering::Relaxed);
            debug!(
                provider = "kiro-acp",
                "kiro-cli ACP side-effect update observed"
            );
        }

        if let Some(text) = extract_acp_text_chunk(&msg)
            && let Some(chunks) = options.chunks.as_deref_mut()
        {
            chunks.push(text);
        }
    }
}

async fn record_stderr_line(stderr_preview: &StderrPreview, line: &str) {
    let redacted = redact_kiro_diagnostic_text(line);
    debug!("[kiro-cli acp stderr] {}", redacted);

    if redacted.trim().is_empty() {
        return;
    }

    let mut preview = stderr_preview.lock().await;
    if !preview.is_empty() {
        preview.push('\n');
    }
    preview.push_str(redacted.trim_end());
    trim_stderr_preview(&mut preview);
}

async fn record_malformed_stdout_line(stdout_preview: &StdoutPreview, line: &str) -> String {
    let redacted = redact_kiro_diagnostic_text(line);

    if redacted.trim().is_empty() {
        return redacted;
    }

    let mut preview = stdout_preview.lock().await;
    if !preview.is_empty() {
        preview.push('\n');
    }
    preview.push_str(redacted.trim_end());
    trim_stdout_preview(&mut preview);

    redacted
}

fn redact_kiro_diagnostic_text(text: &str) -> String {
    let redacted = redact_secret(text);
    match serde_json::from_str::<Value>(&redacted) {
        Ok(value) => serialize_redacted_json_value(&value).unwrap_or(redacted),
        Err(_) => redacted,
    }
}

fn serialize_redacted_json_value(value: &Value) -> Option<String> {
    serde_json::to_string(&redact_sensitive_json_value(value)).ok()
}

fn redact_sensitive_json_value(value: &Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(redact_sensitive_json_value).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_diagnostic_key(key) {
                        Value::String("[REDACTED]".to_string())
                    } else {
                        redact_sensitive_json_value(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::String(text) => Value::String(redact_secret(text)),
        _ => value.clone(),
    }
}

fn trim_stderr_preview(preview: &mut String) {
    if preview.len() <= STDERR_PREVIEW_LIMIT {
        return;
    }

    let excess = preview.len() - STDERR_PREVIEW_LIMIT;
    let drain_to = preview
        .char_indices()
        .find_map(|(idx, _)| (idx >= excess).then_some(idx))
        .unwrap_or(preview.len());
    preview.drain(..drain_to);
}

fn trim_stdout_preview(preview: &mut String) {
    if preview.len() <= STDOUT_PREVIEW_LIMIT {
        return;
    }

    let excess = preview.len() - STDOUT_PREVIEW_LIMIT;
    let drain_to = preview
        .char_indices()
        .find_map(|(idx, _)| (idx >= excess).then_some(idx))
        .unwrap_or(preview.len());
    preview.drain(..drain_to);
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

fn request_fingerprint(model: &str, context_tag: Option<&str>, prompt: &str) -> u64 {
    // This is only an in-process key for retry budgets. DefaultHasher is not
    // stable across Rust releases, so do not use these values as persisted or
    // cross-process identifiers.
    let mut hasher = DefaultHasher::new();
    model.hash(&mut hasher);
    context_tag.hash(&mut hasher);
    prompt.hash(&mut hasher);
    hasher.finish()
}

fn kiro_failure_kind_from_error(message: &str) -> KiroFailureKind {
    let text = message.to_ascii_lowercase();
    if find_marker(&text, KIRO_STRONG_STREAM_FAILURE_MARKERS).is_some()
        || find_marker(&text, KIRO_STREAM_CONTEXT_MARKERS).is_some()
    {
        return KiroFailureKind::Stream;
    }

    if find_marker(&text, KIRO_PROVIDER_TRANSIENT_MARKERS).is_some() {
        return KiroFailureKind::ProviderAvailability;
    }

    if text.contains("idle timeout")
        || text.contains("timed out after")
        || find_marker(&text, KIRO_WEAK_TRANSIENT_MARKERS).is_some()
    {
        return KiroFailureKind::Timeout;
    }

    KiroFailureKind::OtherTransient
}

fn kiro_request_ids_from_error(error: &anyhow::Error) -> Vec<String> {
    if let Some(error) = error.downcast_ref::<KiroCliError>()
        && !error.request_ids.is_empty()
    {
        return error.request_ids.clone();
    }

    extract_request_ids(&error.to_string())
}

fn extract_request_ids(text: &str) -> Vec<String> {
    const REQUEST_ID_MARKERS: &[&str] = &[
        "request_id",
        "requestid",
        "request id",
        "x-amzn-requestid",
        "x-amzn-request-id",
        "amzn-requestid",
    ];

    let lower = text.to_ascii_lowercase();
    let mut ids = Vec::new();
    for marker in REQUEST_ID_MARKERS {
        let mut search_from = 0;
        while let Some(relative_start) = lower[search_from..].find(marker) {
            let marker_end = search_from + relative_start + marker.len();
            search_from = marker_end;

            let Some(id) = extract_identifier_after_marker(text, marker_end) else {
                continue;
            };
            if !ids.iter().any(|seen| seen == &id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn extract_identifier_after_marker(text: &str, marker_end: usize) -> Option<String> {
    let tail = text.get(marker_end..)?;
    let mut current = String::new();
    for ch in tail.chars() {
        if is_request_id_char(ch) {
            current.push(ch);
            continue;
        }

        if let Some(id) = valid_request_id_token(&current) {
            return Some(id);
        }
        current.clear();
    }

    valid_request_id_token(&current)
}

fn is_request_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

fn valid_request_id_token(token: &str) -> Option<String> {
    if token.len() < 8 {
        return None;
    }

    let lower = token.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "request_id" | "requestid" | "request" | "some" | "none" | "null"
    ) {
        return None;
    }

    Some(token.to_string())
}

fn acp_error_data_context(error: &Value) -> String {
    match error.get("data") {
        Some(data) if !data.is_null() => {
            let data = redacted_json_preview(data, KIRO_MAX_ACP_ERROR_DATA_CHARS);
            format!("; data: {}", data)
        }
        _ => String::new(),
    }
}

fn redacted_json_preview(value: &Value, max_chars: usize) -> String {
    let mut out = String::new();
    let completed = push_redacted_json_value_preview(&mut out, value, max_chars);
    if !completed {
        out.push_str("...");
    }
    out
}

fn push_redacted_json_value_preview(out: &mut String, value: &Value, max_chars: usize) -> bool {
    if out.chars().count() >= max_chars {
        return false;
    }

    match value {
        Value::Null => push_bounded(out, "null", max_chars),
        Value::Bool(value) => push_bounded(out, &value.to_string(), max_chars),
        Value::Number(value) => push_bounded(out, &value.to_string(), max_chars),
        Value::String(value) => {
            let redacted = redact_secret(value);
            push_json_string_preview(out, &redacted, max_chars)
        }
        Value::Array(values) => {
            if !push_bounded(out, "[", max_chars) {
                return false;
            }
            for (index, value) in values.iter().enumerate() {
                if index > 0 && !push_bounded(out, ",", max_chars) {
                    return false;
                }
                if !push_redacted_json_value_preview(out, value, max_chars) {
                    return false;
                }
            }
            push_bounded(out, "]", max_chars)
        }
        Value::Object(values) => {
            if !push_bounded(out, "{", max_chars) {
                return false;
            }
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 && !push_bounded(out, ",", max_chars) {
                    return false;
                }
                if !push_json_string_preview(out, key, max_chars)
                    || !push_bounded(out, ":", max_chars)
                {
                    return false;
                }
                if is_sensitive_diagnostic_key(key) {
                    if !push_json_string_preview(out, "[REDACTED]", max_chars) {
                        return false;
                    }
                    continue;
                }
                if !push_redacted_json_value_preview(out, value, max_chars) {
                    return false;
                }
            }
            push_bounded(out, "}", max_chars)
        }
    }
}

fn push_json_string_preview(out: &mut String, value: &str, max_chars: usize) -> bool {
    if !push_bounded(out, "\"", max_chars) {
        return false;
    }

    for ch in value.chars() {
        let completed = match ch {
            '"' => push_bounded(out, "\\\"", max_chars),
            '\\' => push_bounded(out, "\\\\", max_chars),
            '\n' => push_bounded(out, "\\n", max_chars),
            '\r' => push_bounded(out, "\\r", max_chars),
            '\t' => push_bounded(out, "\\t", max_chars),
            ch if ch.is_control() => push_bounded(out, "?", max_chars),
            ch => push_char_bounded(out, ch, max_chars),
        };
        if !completed {
            return false;
        }
    }

    push_bounded(out, "\"", max_chars)
}

fn push_bounded(out: &mut String, text: &str, max_chars: usize) -> bool {
    for ch in text.chars() {
        if !push_char_bounded(out, ch, max_chars) {
            return false;
        }
    }
    true
}

fn push_char_bounded(out: &mut String, ch: char, max_chars: usize) -> bool {
    if out.chars().count() >= max_chars {
        return false;
    }
    out.push(ch);
    true
}

fn is_sensitive_diagnostic_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();

    matches!(
        normalized.as_str(),
        "key"
            | "apikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "secret"
            | "password"
            | "credential"
            | "credentials"
            | "authorization"
            | "auth"
            | "bearer"
    ) || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("password")
        || normalized.ends_with("credential")
        || normalized.ends_with("credentials")
        || normalized.ends_with("secretkey")
        || normalized.ends_with("privatekey")
        || normalized.ends_with("accesskey")
}

async fn diagnostic_context(
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
) -> String {
    let stderr = stderr_preview.lock().await.trim().to_string();
    let stdout = stdout_preview.lock().await.trim().to_string();

    match (stderr.is_empty(), stdout.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("; stderr: {}", stderr),
        (true, false) => format!("; malformed stdout: {}", stdout),
        (false, false) => format!("; stderr: {}; malformed stdout: {}", stderr, stdout),
    }
}

fn acp_update_has_side_effect(msg: &Value) -> bool {
    if msg.get("method").and_then(Value::as_str) != Some("session/update") {
        return false;
    }

    let Some(update) = msg.get("params").and_then(|params| params.get("update")) else {
        return false;
    };

    let update_type = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if matches!(
        update_type.as_str(),
        "agentmessagechunk" | "agent_message_chunk"
    ) {
        return false;
    }

    if update_type_has_side_effect_marker(&update_type) {
        return true;
    }

    // Unknown non-message updates are treated conservatively, but avoid broad
    // substring scans over arbitrary prose. Structural keys and identifier-like
    // values still gate retries; text/message fields do not become side effects
    // merely because they mention words like "file" or "cmdline".
    update_payload_has_side_effect_marker(update)
}

fn update_type_has_side_effect_marker(text: &str) -> bool {
    const SIDE_EFFECT_UPDATE_TYPES: &[&str] = &[
        "toolcall",
        "toolcallupdate",
        "tool_call",
        "tool_call_update",
    ];
    const SIDE_EFFECT_MARKERS: &[&str] = &[
        "tool",
        "approval",
        "permission",
        "command",
        "cmd",
        "exec",
        "shell",
        "file",
        "write",
        "edit",
        "patch",
        "mutation",
    ];

    if SIDE_EFFECT_UPDATE_TYPES.contains(&text) {
        return true;
    }

    SIDE_EFFECT_MARKERS
        .iter()
        .any(|marker| identifier_has_side_effect_marker(text, marker))
}

fn update_payload_has_side_effect_marker(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            if update_type_has_side_effect_marker(&key) {
                return true;
            }

            if value.is_string() && is_textual_update_field(&key) {
                return false;
            }

            update_payload_has_side_effect_marker(value)
        }),
        Value::Array(items) => items.iter().any(update_payload_has_side_effect_marker),
        Value::String(text) => side_effect_text_has_marker(&text.to_ascii_lowercase()),
        _ => false,
    }
}

fn is_textual_update_field(key: &str) -> bool {
    matches!(
        key,
        "content" | "description" | "detail" | "details" | "message" | "reason" | "text"
    )
}

fn identifier_has_side_effect_marker(text: &str, marker: &str) -> bool {
    contains_marker_token(text, marker)
}

fn side_effect_text_has_marker(text: &str) -> bool {
    const SIDE_EFFECT_MARKERS: &[&str] = &[
        "tool",
        "approval",
        "permission",
        "command",
        "cmd",
        "exec",
        "shell",
        "file",
        "write",
        "edit",
        "patch",
        "mutation",
    ];

    SIDE_EFFECT_MARKERS
        .iter()
        .any(|marker| contains_marker_token(text, marker))
}

fn contains_marker_token(text: &str, marker: &str) -> bool {
    let mut search_from = 0;
    while let Some(relative_start) = text[search_from..].find(marker) {
        let start = search_from + relative_start;
        let end = start + marker.len();
        if is_marker_boundary(text, start, true) && is_marker_boundary(text, end, false) {
            return true;
        }
        search_from = end;
    }
    false
}

fn is_marker_boundary(text: &str, index: usize, before: bool) -> bool {
    let byte = if before {
        index
            .checked_sub(1)
            .and_then(|idx| text.as_bytes().get(idx))
            .copied()
    } else {
        text.as_bytes().get(index).copied()
    };

    !matches!(byte, Some(ch) if ch.is_ascii_alphanumeric())
}

fn extract_acp_text_chunk(msg: &Value) -> Option<String> {
    if msg.get("method")?.as_str()? != "session/update" {
        return None;
    }

    let update = msg.get("params")?.get("update")?;
    let update_type = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))?
        .as_str()?;
    let update_type = update_type.to_ascii_lowercase();
    if !matches!(
        update_type.as_str(),
        "agentmessagechunk" | "agent_message_chunk"
    ) {
        return None;
    }

    extract_text_content(update.get("content")?)
}

fn extract_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(extract_text_content)
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

impl KiroCliProvider {
    fn default_state() -> Arc<KiroProviderState> {
        #[cfg(test)]
        {
            Arc::new(KiroProviderState::default())
        }
        #[cfg(not(test))]
        {
            KIRO_PROVIDER_STATE
                .get_or_init(|| Arc::new(KiroProviderState::default()))
                .clone()
        }
    }

    pub fn new(
        model: String,
        binary: String,
        agent: Option<String>,
        context_window_size: usize,
    ) -> Self {
        Self {
            model,
            binary,
            agent,
            context_window_size,
            state: Self::default_state(),
            #[cfg(test)]
            turn_timeout_override: None,
            #[cfg(test)]
            idle_timeout_override: None,
        }
    }

    #[cfg(test)]
    fn with_turn_timeout_override(mut self, timeout: Duration) -> Self {
        self.turn_timeout_override = Some(timeout);
        self
    }

    fn turn_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(timeout) = self.turn_timeout_override {
            return timeout;
        }

        kiro_acp_turn_timeout()
    }

    #[cfg(test)]
    fn with_idle_timeout_override(mut self, timeout: Duration) -> Self {
        self.idle_timeout_override = Some(timeout);
        self
    }

    fn idle_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(timeout) = self.idle_timeout_override {
            return timeout;
        }

        kiro_acp_idle_timeout()
    }

    async fn run_acp_prompt(
        &self,
        prompt: &str,
        agent_name: &str,
        isolated_workspace: Option<&TempDir>,
        side_effects_seen: Arc<AtomicBool>,
    ) -> Result<String> {
        let prompt_chars = prompt.len();
        let total_start = Instant::now();
        debug!(
            "kiro-cli ACP starting: model={}, agent={}, prompt_chars={}",
            self.model, agent_name, prompt_chars
        );

        let mut args = build_args(&self.model, agent_name);
        let verbose_args = verbose_args_from_env();
        if !verbose_args.is_empty() {
            debug!(
                "kiro-cli ACP verbose diagnostics enabled: {} flag(s)",
                verbose_args.len()
            );
            args.extend(verbose_args);
        }

        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(tmp) = isolated_workspace {
            cmd.current_dir(tmp.path());
        }

        cmd.kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn kiro-cli ACP: {}. Is it installed?", e)
        })?;
        let mut child = KiroChildGuard::new(child);
        debug!(
            "kiro-cli ACP spawned in {:.2}s",
            total_start.elapsed().as_secs_f64()
        );

        let stderr_preview = Arc::new(Mutex::new(String::new()));
        let stdout_preview = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.child_mut()?.stderr.take() {
            let stderr_preview = stderr_preview.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    record_stderr_line(&stderr_preview, &line).await;
                }
            });
        }

        let mut stdin = child
            .child_mut()?
            .stdin
            .take()
            .context("kiro-cli ACP stdin missing")?;
        let stdout = child
            .child_mut()?
            .stdout
            .take()
            .context("kiro-cli ACP stdout missing")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut next_id = 0u64;
        let idle_timeout = self.idle_timeout();

        let phase_start = Instant::now();
        write_rpc_checked(
            &mut stdin,
            next_id,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "sashiko",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        read_rpc_response(
            &mut lines,
            next_id,
            &stderr_preview,
            &stdout_preview,
            side_effects_seen.as_ref(),
            RpcReadOptions {
                idle_timeout: Some(idle_timeout),
                ..RpcReadOptions::default()
            },
        )
        .await?;
        debug!(
            "kiro-cli ACP initialize completed in {:.2}s",
            phase_start.elapsed().as_secs_f64()
        );
        next_id += 1;

        let phase_start = Instant::now();
        write_rpc_checked(
            &mut stdin,
            next_id,
            "session/new",
            json!({
                "cwd": ".",
                "mcpServers": [],
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        let session = read_rpc_response(
            &mut lines,
            next_id,
            &stderr_preview,
            &stdout_preview,
            side_effects_seen.as_ref(),
            RpcReadOptions {
                idle_timeout: Some(idle_timeout),
                ..RpcReadOptions::default()
            },
        )
        .await?;
        debug!(
            "kiro-cli ACP session/new completed in {:.2}s",
            phase_start.elapsed().as_secs_f64()
        );
        let session_id = match session.get("sessionId").and_then(Value::as_str) {
            Some(session_id) => session_id.to_string(),
            None => {
                anyhow::bail!(
                    "kiro-cli ACP session/new response missing sessionId{}",
                    diagnostic_context(&stderr_preview, &stdout_preview).await
                );
            }
        };
        next_id += 1;

        let phase_start = Instant::now();
        write_rpc_checked(
            &mut stdin,
            next_id,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": prompt,
                    }
                ],
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        let mut chunks = Vec::new();
        let mut read_stats = KiroRpcReadStats::default();
        read_rpc_response(
            &mut lines,
            next_id,
            &stderr_preview,
            &stdout_preview,
            side_effects_seen.as_ref(),
            RpcReadOptions {
                idle_timeout: Some(idle_timeout),
                read_stats: Some(&mut read_stats),
                chunks: Some(&mut chunks),
            },
        )
        .await?;
        let response_chars: usize = chunks.iter().map(String::len).sum();
        debug!(
            "kiro-cli ACP session/prompt completed in {:.2}s (chunks={}, response_chars={}, acp_lines={}, max_idle_gap_secs={:.2})",
            phase_start.elapsed().as_secs_f64(),
            chunks.len(),
            response_chars,
            read_stats.lines_seen,
            read_stats.max_idle_gap.as_secs_f64()
        );

        drop(stdin);
        child.kill_and_wait().await;
        debug!(
            "kiro-cli ACP completed in {:.2}s (prompt_chars={}, response_chars={})",
            total_start.elapsed().as_secs_f64(),
            prompt_chars,
            response_chars
        );

        Ok(chunks.join(""))
    }
}

#[async_trait]
impl AiProvider for KiroCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);
        let prompt_tokens = TokenBudget::estimate_tokens(&prompt);
        debug!(
            "kiro-cli prompt length: {} chars, estimated tokens: {}",
            prompt.len(),
            prompt_tokens
        );
        let request_key = request_fingerprint(&self.model, request.context_tag.as_deref(), &prompt);
        self.state.reject_if_circuit_open().await?;

        let (agent_name, isolated_workspace) = match &self.agent {
            Some(a) => (a.clone(), None),
            None => {
                let tmp = create_isolated_workspace()?;
                ("sashiko-provider".to_string(), Some(tmp))
            }
        };

        let call_start = Instant::now();
        let remaining_turn_wall_clock = self.state.start_turn(request_key, call_start).await;
        let turn_timeout = self.turn_timeout().min(remaining_turn_wall_clock);
        let side_effects_seen = Arc::new(AtomicBool::new(false));
        let acp_result = timeout(
            turn_timeout,
            self.run_acp_prompt(
                &prompt,
                &agent_name,
                isolated_workspace.as_ref(),
                side_effects_seen.clone(),
            ),
        )
        .await;

        let text = match acp_result {
            Ok(Ok(text)) => {
                self.state.report_success(request_key).await;
                text
            }
            Ok(Err(err)) => {
                match crate::ai::classify_ai_error(&err) {
                    AiErrorClass::Transient { .. } => {
                        let error_message = redact_secret(&err.to_string());
                        let request_ids = kiro_request_ids_from_error(&err);
                        self.state
                            .report_transient_failure(KiroTransientFailure {
                                request_key,
                                model: &self.model,
                                prompt_chars: prompt.len(),
                                prompt_tokens,
                                failure_kind: kiro_failure_kind_from_error(&error_message),
                                error_message,
                                request_ids,
                            })
                            .await?;
                    }
                    AiErrorClass::Fatal => {
                        self.state.report_success(request_key).await;
                    }
                    AiErrorClass::RateLimit { .. } => {
                        // Keep first_seen so quota retries share this logical
                        // turn's wall-clock budget.
                    }
                }
                return Err(err);
            }
            Err(_) => {
                let side_effects_seen = side_effects_seen.load(Ordering::Relaxed);
                let class = timeout_error_class(side_effects_seen);
                let err: anyhow::Error = KiroCliError::new(
                    format!(
                        "kiro-cli ACP timed out after {:.2} seconds (model={}, prompt_chars={}, estimated_prompt_tokens={}, side_effects_seen={})",
                        turn_timeout.as_secs_f64(),
                        self.model,
                        prompt.len(),
                        prompt_tokens,
                        side_effects_seen
                    ),
                    class,
                )
                .into();
                if side_effects_seen {
                    self.state.report_success(request_key).await;
                    return Err(err);
                }

                let error_message = redact_secret(&err.to_string());
                self.state
                    .report_transient_failure(KiroTransientFailure {
                        request_key,
                        model: &self.model,
                        prompt_chars: prompt.len(),
                        prompt_tokens,
                        failure_kind: kiro_failure_kind_from_error(&error_message),
                        error_message,
                        request_ids: kiro_request_ids_from_error(&err),
                    })
                    .await?;
                return Err(err);
            }
        };
        debug!(
            "kiro-cli ACP generate_content completed in {:.2}s (response_chars={})",
            call_start.elapsed().as_secs_f64(),
            text.len()
        );

        // Synthesize usage from token estimates since kiro-cli does not
        // expose provider token counts.
        let completion_tokens = TokenBudget::estimate_tokens(&text);
        let usage = Some(AiUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cached_tokens: None,
        });

        parse_inner_response(&text, usage)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        let prompt = build_prompt(request);
        TokenBudget::estimate_tokens(&prompt)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiRole, AiTool, create_provider};
    use crate::settings::Settings;

    fn sample_request() -> AiRequest {
        AiRequest {
            system: Some("You are a kernel reviewer.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Review this patch.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Some(vec![AiTool {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn test_factory_creates_provider() {
        let mut settings = Settings::new().expect("Failed to load settings");
        settings.ai.provider = "kiro-cli".to_string();
        settings.ai.model = "claude-sonnet-4".to_string();

        let provider = create_provider(&settings).unwrap();
        let caps = provider.get_capabilities();
        assert_eq!(caps.model_name, "claude-sonnet-4");
        assert_eq!(caps.context_window_size, 200_000);
    }

    #[test]
    fn test_command_args_default() {
        let args = build_args("claude-sonnet-4", "sashiko-provider");
        assert_eq!(args[0], "acp");
        assert_eq!(args[1], "--agent");
        assert_eq!(args[2], "sashiko-provider");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "claude-sonnet-4");
    }

    #[test]
    fn test_command_args_custom_agent() {
        let args = build_args("claude-sonnet-4", "my-agent");
        assert_eq!(
            args,
            vec![
                "acp".to_string(),
                "--agent".to_string(),
                "my-agent".to_string(),
                "--model".to_string(),
                "claude-sonnet-4".to_string(),
            ]
        );
    }

    #[test]
    fn test_isolated_workspace_agent_json() {
        let tmp = create_isolated_workspace().unwrap();
        let agent_path = tmp.path().join(".kiro/agents/sashiko-provider.json");
        assert!(agent_path.exists());

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&agent_path).unwrap()).unwrap();

        assert_eq!(content["tools"], serde_json::json!([]));
        assert_eq!(content["allowedTools"], serde_json::json!([]));
        assert_eq!(content["mcpServers"], serde_json::json!({}));
        assert_eq!(content["resources"], serde_json::json!([]));
        assert_eq!(content["includeMcpJson"], serde_json::json!(false));
        assert!(
            content["prompt"]
                .as_str()
                .unwrap()
                .contains("Follow the user-provided instructions exactly")
        );

        // Verify deny-all hook is wired
        let hooks = &content["hooks"]["preToolUse"];
        assert!(hooks.is_array());
        let hook_cmd = hooks[0]["command"].as_str().unwrap();
        assert_eq!(hook_cmd, ".kiro/hooks/deny-all-tools.sh");
    }

    #[test]
    fn test_deny_all_hook_exits_nonzero() {
        let tmp = create_isolated_workspace().unwrap();
        let hook_path = tmp.path().join(".kiro/hooks/deny-all-tools.sh");
        assert!(hook_path.exists());

        let output = std::process::Command::new("sh")
            .arg(&hook_path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("disabled"));
    }

    #[test]
    fn test_extract_acp_text_chunk_snake_case() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hello"}
                }
            }
        });
        assert_eq!(extract_acp_text_chunk(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_acp_text_chunk_camel_case() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "AgentMessageChunk",
                    "content": [
                        {"type": "text", "text": "hel"},
                        {"type": "text", "text": "lo"}
                    ]
                }
            }
        });
        assert_eq!(extract_acp_text_chunk(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_acp_text_chunk_lower_camel_case() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agentMessageChunk",
                    "content": {"type": "text", "text": "hello"}
                }
            }
        });
        assert_eq!(extract_acp_text_chunk(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_acp_ignores_tool_updates() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {"sessionUpdate": "tool_call", "content": {"text": "ignored"}}
            }
        });
        assert!(extract_acp_text_chunk(&input).is_none());
    }

    #[test]
    fn test_parse_tool_calls_json() {
        let text = r#"{"tool_calls":[{"id":"c1","function_name":"read_file","arguments":{"path":"README.md"}}]}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert!(resp.tool_calls.is_some());
        let calls = resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function_name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
    }

    #[test]
    fn test_parse_plain_content() {
        let text = r#"{"content":"No issues found in this patch."}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(
            resp.content.as_deref(),
            Some("No issues found in this patch.")
        );
        assert!(resp.tool_calls.is_none());
    }

    #[test]
    fn test_parse_raw_text_fallback() {
        let text = "This is not JSON at all.";
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(resp.content.as_deref(), Some(text));
    }

    #[test]
    fn test_extract_request_ids_from_kiro_errors() {
        let ids = extract_request_ids(
            r#"Internal error: request_id: 123e4567-e89b-12d3-a456-426614174000, x-amzn-requestid="abcde12345", request_id=Some("570a44c8-1111-2222-3333-444455556666")"#,
        );

        assert_eq!(
            ids,
            vec![
                "123e4567-e89b-12d3-a456-426614174000".to_string(),
                "570a44c8-1111-2222-3333-444455556666".to_string(),
                "abcde12345".to_string()
            ]
        );
    }

    #[test]
    fn test_request_fingerprint_separates_review_contexts() {
        let first = request_fingerprint("test", Some("[ps:1 p:1]"), "same prompt");
        let second = request_fingerprint("test", Some("[ps:2 p:1]"), "same prompt");

        assert_ne!(first, second);
        assert_eq!(
            first,
            request_fingerprint("test", Some("[ps:1 p:1]"), "same prompt")
        );
    }

    #[tokio::test]
    async fn test_kiro_provider_state_caps_same_transient_failure_streak() {
        let state = KiroProviderState::default();
        let request_key = 42;
        for attempt in 1..KIRO_MAX_SAME_TRANSIENT_STREAK_PER_TURN {
            state
                .report_transient_failure(KiroTransientFailure {
                    request_key,
                    model: "test",
                    prompt_chars: 100,
                    prompt_tokens: 25,
                    failure_kind: KiroFailureKind::Stream,
                    error_message: format!("stream failure {attempt}"),
                    request_ids: Vec::new(),
                })
                .await
                .unwrap();
        }

        let err = state
            .report_transient_failure(KiroTransientFailure {
                request_key,
                model: "test",
                prompt_chars: 100,
                prompt_tokens: 25,
                failure_kind: KiroFailureKind::Stream,
                error_message: "stream failure capped".to_string(),
                request_ids: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("KiroTransientBudgetExceeded"));
        assert_eq!(crate::ai::classify_ai_error(&err), AiErrorClass::Fatal);
        assert!(err.downcast_ref::<KiroCliError>().is_some());
        assert!(
            err.downcast_ref::<crate::worker::prompts::ReviewError>()
                .is_some()
        );
        state.reject_if_circuit_open().await.unwrap();
    }

    #[test]
    fn test_kiro_acp_turn_timeout_covers_kiro_budgets() {
        let timeout = kiro_acp_turn_timeout();
        assert!(timeout >= Duration::from_secs(300));
        assert!(timeout >= kiro_acp_idle_timeout());
        assert!(timeout >= kiro_max_turn_wall_clock());
    }

    #[tokio::test]
    async fn test_turn_budget_elapsed_includes_first_attempt() {
        let state = KiroProviderState::default();
        let request_key = 42;
        let started_at = Instant::now() - kiro_max_turn_wall_clock() - Duration::from_secs(1);
        let _ = state.start_turn(request_key, started_at).await;

        let err = state
            .report_transient_failure(KiroTransientFailure {
                request_key,
                model: "test",
                prompt_chars: 100,
                prompt_tokens: 25,
                failure_kind: KiroFailureKind::Timeout,
                error_message: "outer timeout".to_string(),
                request_ids: Vec::new(),
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("KiroTransientBudgetExceeded"));
        assert!(err.contains("turn wall-clock budget exceeded"));
    }

    #[tokio::test]
    async fn test_turn_budget_returns_remaining_wall_clock_for_retry() {
        let state = KiroProviderState::default();
        let request_key = 42;
        let first_started = Instant::now();
        let _ = state.start_turn(request_key, first_started).await;

        let almost_exhausted = first_started + kiro_max_turn_wall_clock() - Duration::from_secs(2);
        let remaining = state.start_turn(request_key, almost_exhausted).await;

        assert_eq!(remaining, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn test_provider_state_prunes_stale_budget_entries() {
        let state = KiroProviderState::default();
        let now = Instant::now();
        let old = now - KIRO_BUDGET_ENTRY_TTL - Duration::from_secs(1);
        let old_key = 1;
        let fresh_key = 2;

        let _ = state.start_turn(old_key, old).await;
        let _ = state.start_turn(fresh_key, now).await;

        let turn_budgets = state.turn_budgets.lock().await;
        assert!(!turn_budgets.contains_key(&old_key));
        assert!(turn_budgets.contains_key(&fresh_key));
    }

    #[test]
    fn test_estimate_tokens_uses_token_budget() {
        let provider =
            KiroCliProvider::new("test".to_string(), "kiro-cli".to_string(), None, 200_000);
        let req = sample_request();
        let estimate = provider.estimate_tokens(&req);
        // The prompt is non-empty, so estimate should be > 0
        assert!(estimate > 0);
    }

    #[tokio::test]
    async fn test_generate_content_with_fake_acp_server() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}' ;;
    2)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"{\"content\":\"ok\"}"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}'
      exit 0
      ;;
  esac
  i=$((i + 1))
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        );
        let response = provider.generate_content(sample_request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_stderr_on_startup_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' 'authentication failed token=abc123' >&2
sleep 0.1
exit 2
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        );

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kiro-cli ACP exited before response 0"));
        assert!(err.contains("stderr: authentication failed token=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }

    #[tokio::test]
    async fn test_generate_content_applies_idle_timeout_during_initialize() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
read -r line
sleep 30
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        )
        .with_turn_timeout_override(Duration::from_secs(2))
        .with_idle_timeout_override(Duration::from_millis(50));

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
        assert!(
            err.to_string()
                .contains("ACP idle timeout after 0.05s waiting for response 0")
        );
    }

    #[tokio::test]
    async fn test_generate_content_rejects_malformed_target_response_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
read -r line
printf '%s\n' '{"id":0,"result":{"protocolVersion":1}}'
sleep 30
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        )
        .with_turn_timeout_override(Duration::from_secs(2))
        .with_idle_timeout_override(Duration::from_secs(1));

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert_eq!(crate::ai::classify_ai_error(&err), AiErrorClass::Fatal);
        assert!(
            err.to_string()
                .contains("malformed JSON-RPC response for id 0")
        );
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_acp_error_data() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"Internal error token=message-secret","data":{"kind":"ServiceFailure","token":"abc123","auth":{"password":"secret123"},"reason":"Encountered an error in the response stream: CodewhispererChatResponseStream(DispatchFailure(TimedOut)) token=abc123"}}}'
  exit 0
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        );

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
        let err = err.to_string();
        assert!(err.contains("kiro-cli ACP error -32603: Internal error token=[REDACTED]"));
        assert!(!err.contains("message-secret"));
        assert!(err.contains("data:"));
        assert!(err.contains("ServiceFailure"));
        assert!(err.contains("CodewhispererChatResponseStream"));
        assert!(err.contains("token=[REDACTED]"));
        assert!(err.contains("\"token\":\"[REDACTED]\""));
        assert!(err.contains("\"auth\":\"[REDACTED]\""));
        assert!(!err.contains("abc123"));
        assert!(!err.contains("secret123"));
    }

    #[tokio::test]
    async fn test_rate_limit_preserves_turn_wall_clock_start() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"Too many requests"}}'
  exit 0
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        );
        let mut request = sample_request();
        request.context_tag = Some("[ps:1 p:1]".to_string());
        let prompt = build_prompt(&request);
        let request_key =
            request_fingerprint(&provider.model, request.context_tag.as_deref(), &prompt);

        let err = provider
            .generate_content(request.clone())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::RateLimit { .. }
        ));
        let first_seen = provider
            .state
            .turn_budgets
            .lock()
            .await
            .get(&request_key)
            .map(|budget| budget.first_seen);
        assert!(first_seen.is_some());

        let err = provider.generate_content(request).await.unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::RateLimit { .. }
        ));
        let retry_first_seen = provider
            .state
            .turn_budgets
            .lock()
            .await
            .get(&request_key)
            .map(|budget| budget.first_seen);
        assert_eq!(retry_first_seen, first_seen);
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_malformed_stdout_context() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"token":"abc123","message":"kiro warning"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"Internal error"}}'
  exit 0
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        );

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kiro-cli ACP error -32603: Internal error"));
        assert!(
            err.contains(
                "malformed stdout: {\"message\":\"kiro warning\",\"token\":\"[REDACTED]\"}"
            )
        );
        assert!(!err.contains("abc123"));
    }

    #[tokio::test]
    async fn test_generate_content_timeout_is_transient() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
sleep 2
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        )
        .with_turn_timeout_override(Duration::ZERO);

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
        let err = err.to_string();
        assert!(err.contains("kiro-cli ACP timed out after 0.00 seconds"));
        assert!(err.contains("model=test"));
        assert!(err.contains("prompt_chars="));
        assert!(err.contains("estimated_prompt_tokens="));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_generate_content_timeout_reaps_fake_acp_process() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        let pid_file = tmp.path().join("fake.pid");
        let script = r#"#!/bin/sh
printf '%s\n' "$$" > "__PID_FILE__"
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}' ;;
    2) sleep 30 ;;
  esac
  i=$((i + 1))
done
"#
        .replace("__PID_FILE__", &pid_file.to_string_lossy());
        std::fs::write(&fake, script).unwrap();

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        )
        .with_turn_timeout_override(Duration::from_millis(100));

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("kiro-cli ACP timed out"));

        let mut pid_text = None;
        for _ in 0..20 {
            match std::fs::read_to_string(&pid_file) {
                Ok(text) => {
                    pid_text = Some(text);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        let pid: u32 = pid_text
            .expect("fake kiro process did not write pid")
            .trim()
            .parse()
            .unwrap();
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..40 {
            if !proc_path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("fake kiro process {pid} still exists after timeout cleanup");
    }

    #[tokio::test]
    async fn test_generate_content_timeout_after_side_effect_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}' ;;
    2)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","content":{"name":"write_file"}}}}'
      sleep 2
      ;;
  esac
  i=$((i + 1))
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider::new(
            "test".to_string(),
            fake.to_string_lossy().to_string(),
            None,
            200_000,
        )
        .with_turn_timeout_override(Duration::from_secs(1));

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert_eq!(crate::ai::classify_ai_error(&err), AiErrorClass::Fatal);
        let err = err.to_string();
        assert!(err.contains("kiro-cli ACP timed out after 1.00 seconds"));
        assert!(err.contains("side_effects_seen=true"));
    }

    #[test]
    fn test_non_acp_json_does_not_count_as_protocol_message() {
        let diagnostic = json!({"id": 2, "event": "verbose"});
        let incomplete_response = json!({"jsonrpc": "2.0", "id": 2, "event": "verbose"});
        let valid_response = json!({"jsonrpc": "2.0", "id": 2, "result": {}});
        let valid_update = json!({"jsonrpc": "2.0", "method": "session/update", "params": {}});

        assert!(!is_acp_json_rpc_message(&diagnostic));
        assert!(!is_acp_json_rpc_message(&incomplete_response));
        assert!(is_acp_json_rpc_message(&valid_response));
        assert!(is_acp_json_rpc_message(&valid_update));
        assert!(!is_acp_response_for_target(&incomplete_response, 2));
        assert!(is_acp_response_for_target(&valid_response, 2));
    }

    #[test]
    fn test_acp_error_data_context_redacts_and_bounds_json() {
        let secret = "abc123";
        let long_value = "x".repeat(KIRO_MAX_ACP_ERROR_DATA_CHARS * 2);
        let error = json!({
            "data": {
                "token": secret,
                "z_details": long_value,
            }
        });

        let context = acp_error_data_context(&error);

        assert!(context.contains("\"token\":\"[REDACTED]\""));
        assert!(!context.contains(secret));
        assert!(context.len() <= KIRO_MAX_ACP_ERROR_DATA_CHARS + "; data: …".len() + 8);
    }

    #[test]
    fn test_acp_error_data_context_does_not_emit_large_sensitive_value() {
        let secret = format!("abc123{}", "x".repeat(KIRO_MAX_ACP_ERROR_DATA_CHARS * 2));
        let error = json!({
            "data": {
                "safe": "prefix",
                "session_token": secret,
            }
        });

        let context = acp_error_data_context(&error);

        assert!(context.contains("\"session_token\":\"[REDACTED]\""));
        assert!(context.contains("\"safe\":\"prefix\""));
        assert!(!context.contains("abc123"));
        assert!(context.len() <= KIRO_MAX_ACP_ERROR_DATA_CHARS + "; data: ...".len());
    }

    #[tokio::test]
    async fn test_malformed_stdout_redacts_composite_sensitive_json_keys() {
        let stdout_preview = Arc::new(Mutex::new(String::new()));
        let line = r#"{"session_token":"abc123","secret_key":"def456","safe":"visible"}"#;

        let redacted = record_malformed_stdout_line(&stdout_preview, line).await;
        let preview = stdout_preview.lock().await.clone();

        for output in [&redacted, &preview] {
            assert!(output.contains("\"session_token\":\"[REDACTED]\""));
            assert!(output.contains("\"secret_key\":\"[REDACTED]\""));
            assert!(output.contains("\"safe\":\"visible\""));
            assert!(!output.contains("abc123"));
            assert!(!output.contains("def456"));
        }
    }

    #[test]
    fn test_kiro_acp_stream_context_and_timedout_is_transient() {
        let data = json!("CodewhispererChatResponseStream(DispatchFailure(TimedOut))");
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_strong_stream_marker_is_transient() {
        let data = json!("failed to receive the next event");
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_weak_marker_alone_is_fatal() {
        let data = json!("TimedOut");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_provider_operation_context_does_not_make_stream_timeout() {
        let data = json!("GenerateAssistantResponse TimedOut");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_classification_text_is_bounded() {
        let marker_after_cap = format!(
            "{} unexpected eof",
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1)
        );
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&json!(marker_after_cap))),
            AiErrorClass::Fatal
        );

        let marker_before_cap = format!(
            "failed to receive the next event {}",
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1)
        );
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&json!(marker_before_cap))),
            AiErrorClass::Transient { .. }
        ));
    }

    #[test]
    fn test_kiro_acp_classification_scans_bounded_structured_data() {
        let marker_before_cap = json!(["failed to receive the next event", {
            "padding": "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1),
        }]);
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&marker_before_cap)),
            AiErrorClass::Transient { .. }
        ));

        let marker_after_cap = json!([
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1),
            "failed to receive the next event"
        ]);
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&marker_after_cap)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_permanent_marker_wins_over_stream_marker() {
        let data = json!(
            "invalid conversation history CodewhispererChatResponseStream DispatchFailure TimedOut"
        );
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_provider_availability_is_transient() {
        let data = json!({"kind": "ModelOverloadedError"});
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_additional_strong_stream_markers_are_transient() {
        for marker in [
            "failed to receive the next message",
            "connection closed before message completed",
            "RecvErrorStreamTimeout",
            "ThroughputBelowMinimum",
            "minimum throughput was specified at 1 B/s but throughput of 0 B/s was observed",
            "grace period ended; timing out request",
        ] {
            let data = json!(marker);
            assert!(
                matches!(
                    classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
                    AiErrorClass::Transient { .. }
                ),
                "marker should be transient: {marker}"
            );
        }
    }

    #[test]
    fn test_kiro_acp_context_marker_alone_is_fatal() {
        let data = json!("ChatResponseStreamUnmarshaller");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_object_data_with_only_timedout_is_fatal() {
        let data = json!({"reason": "TimedOut"});
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_throttling_is_rate_limit() {
        let data = json!({"kind": "ThrottlingError"});
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::RateLimit { retry_after } if retry_after == DEFAULT_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_rate_limit_wins_over_stream_marker() {
        let data = json!(
            "Encountered an error in the response stream: ThrottlingError from CodewhispererChatResponseStream"
        );
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::RateLimit { retry_after } if retry_after == DEFAULT_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_classification_reports_marker_details() {
        let data = json!("CodewhispererChatResponseStream(DispatchFailure(TimedOut))");
        let classification =
            classify_kiro_acp_error_with_details(-32603, "Internal error", Some(&data), false);
        assert_eq!(
            classification.marker_class,
            KiroMarkerClass::StreamContextWeakTransient
        );
        assert_eq!(classification.matched_marker, Some("dispatchfailure"));
        assert!(!classification.retry_blocked_by_side_effect_gate);
    }

    #[test]
    fn test_kiro_acp_side_effect_gate_blocks_stream_retry() {
        let data = json!("failed to receive the next event");
        let classification =
            classify_kiro_acp_error_with_details(-32603, "Internal error", Some(&data), true);
        assert_eq!(classification.class, AiErrorClass::Fatal);
        assert_eq!(
            classification.marker_class,
            KiroMarkerClass::StrongStreamFailure
        );
        assert!(classification.retry_blocked_by_side_effect_gate);
    }

    #[test]
    fn test_kiro_acp_side_effect_gate_blocks_rate_limit_retry() {
        let data = json!({"kind": "ThrottlingError"});
        let classification =
            classify_kiro_acp_error_with_details(-32603, "Internal error", Some(&data), true);
        assert_eq!(classification.class, AiErrorClass::Fatal);
        assert_eq!(classification.marker_class, KiroMarkerClass::RateLimit);
        assert!(classification.retry_blocked_by_side_effect_gate);
    }

    #[test]
    fn test_acp_update_has_side_effect_detects_tool_updates() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {"sessionUpdate": "tool_call", "content": {}}
            }
        });
        assert!(acp_update_has_side_effect(&msg));
    }

    #[test]
    fn test_acp_update_has_side_effect_detects_camel_case_tool_updates() {
        for update_type in ["ToolCall", "ToolCallUpdate"] {
            let msg = json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "update": {"sessionUpdate": update_type, "content": {}}
                }
            });
            assert!(acp_update_has_side_effect(&msg), "{update_type}");
        }
    }

    #[test]
    fn test_acp_update_has_side_effect_ignores_agent_message_chunks() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"text": "I might write about tools in text only"}
                }
            }
        });
        assert!(!acp_update_has_side_effect(&msg));
    }

    #[test]
    fn test_acp_update_has_side_effect_ignores_profile_key_substring() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "progress",
                    "content": {"profile": "scheduler"}
                }
            }
        });
        assert!(!acp_update_has_side_effect(&msg));
    }

    #[test]
    fn test_acp_update_has_side_effect_ignores_unknown_text_mentions() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "progress",
                    "content": {
                        "text": "Reading the kernel cmdline and profile data"
                    }
                }
            }
        });
        assert!(!acp_update_has_side_effect(&msg));
    }

    #[test]
    fn test_acp_update_has_side_effect_detects_structural_payload_markers() {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "progress",
                    "content": {
                        "name": "write_file"
                    }
                }
            }
        });
        assert!(acp_update_has_side_effect(&msg));
    }

    #[test]
    fn test_redact_secret_available_for_error_previews() {
        let redacted = redact_secret("kiro failed with token=abc123");
        assert_eq!(redacted, "kiro failed with token=[REDACTED]");
    }
}

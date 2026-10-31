use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use super::{
    AiProvider, AiProviderDecorator, AiRequest, AiResponse, CacheStats, ProviderCapabilities,
    get_log_prefix,
};

/// A date as the workflows' system prompts state it ("%A, %B %d, %Y").
static PROMPT_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:Mon|Tues|Wednes|Thurs|Fri|Satur|Sun)day, [A-Z][a-z]+ \d{2}, \d{4}\b").unwrap()
});

pub struct ResponseCacheStore {
    conn: libsql::Connection,
    session_start: i64,
    hits_this: AtomicU64,
    hits_prev: AtomicU64,
    tokens_saved_this: AtomicU64,
    tokens_saved_prev: AtomicU64,
}

pub struct CachingAiProvider {
    inner: Arc<dyn AiProvider>,
    store: Arc<ResponseCacheStore>,
    replay: bool,
    resume: bool,
}

impl CachingAiProvider {
    pub async fn new(inner: Arc<dyn AiProvider>, cache_path: &str, ttl_days: u64) -> Result<Self> {
        let db = libsql::Builder::new_local(cache_path).build().await?;
        let conn = db.connect()?;

        let _ = conn
            .query("PRAGMA journal_mode=WAL;", ())
            .await?
            .next()
            .await;
        let _ = conn
            .query("PRAGMA busy_timeout = 5000;", ())
            .await?
            .next()
            .await;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS response_cache (
                request_hash TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                request_json TEXT NOT NULL,
                response_json TEXT NOT NULL,
                tokens_saved INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_response_cache_provider_model
                ON response_cache (provider, model);",
        )
        .await?;

        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - ttl_days as i64 * 86400;
        let result = conn
            .execute(
                "DELETE FROM response_cache WHERE created_at < ?",
                libsql::params![cutoff],
            )
            .await;
        if let Ok(reaped) = result
            && reaped > 0
        {
            info!(
                "Response cache: reaped {} expired entries (>{} days old)",
                reaped, ttl_days
            );
        }

        let session_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        info!("Response cache enabled ({})", cache_path);

        let store = Arc::new(ResponseCacheStore {
            conn,
            session_start,
            hits_this: AtomicU64::new(0),
            hits_prev: AtomicU64::new(0),
            tokens_saved_this: AtomicU64::new(0),
            tokens_saved_prev: AtomicU64::new(0),
        });

        Ok(Self {
            inner,
            store,
            replay: true,
            resume: false,
        })
    }

    /// Records every answer and serves none, so a run that asks the model
    /// afresh still leaves its answers for a later run to replay.
    pub fn record_only(mut self) -> Self {
        self.replay = false;
        self
    }

    pub fn with_resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }

    fn canonical_request_json(request: &AiRequest) -> String {
        let mut val = serde_json::to_value(request).unwrap_or_default();
        // Strip nondeterministic fields
        if let serde_json::Value::Object(ref mut map) = val {
            map.remove("context_tag");
            // The system prompt's date would make every day's keys differ. Only
            // the first date is today's: a later one is part of the patch.
            if let Some(system) = map.get_mut("system")
                && let Some(text) = system.as_str()
            {
                *system = PROMPT_DATE.replace(text, "<date>").into();
            }
        }
        super::scrub_thought_signatures(&mut val);
        serde_json::to_string(&val).unwrap_or_default()
    }

    fn compute_cache_key_for_identity(identity: &str, canonical: &str) -> String {
        // The model and the provider's own knobs never appear in the request,
        // so hash them alongside it. Without them a raised reasoning effort
        // replays the answer recorded at the lower one.
        let mut hasher = Sha256::new();
        hasher.update(identity.as_bytes());
        hasher.update(b"\0");
        hasher.update(canonical.as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[cfg(test)]
    fn compute_cache_key(&self, request: &AiRequest) -> String {
        let canonical = Self::canonical_request_json(request);
        Self::compute_cache_key_for_identity(&self.inner.cache_identity(), &canonical)
    }

    async fn candidate_resume_hashes(
        &self,
        canonical: &str,
        primary_hash: &str,
    ) -> Result<Vec<String>> {
        let mut rows = self
            .store
            .conn
            .query("SELECT DISTINCT provider, model FROM response_cache", ())
            .await?;

        let mut pairs = Vec::new();
        while let Some(row) = rows.next().await? {
            let prov: String = row.get(0)?;
            let model: String = row.get(1)?;
            pairs.push((prov, model));
        }

        let current_identity = self.inner.cache_identity();
        let current_model = self.inner.get_capabilities().model_name;

        let mut candidate_identities = Vec::new();
        let mut seen_identities = std::collections::HashSet::new();

        for (prov, other_model) in pairs {
            let candidates = [
                prov,
                current_identity.replacen(&current_model, &other_model, 1),
                other_model,
            ];
            for cand in candidates {
                if seen_identities.insert(cand.clone()) {
                    candidate_identities.push(cand);
                }
            }
        }

        let mut candidate_hashes = Vec::new();
        let mut seen_hashes = std::collections::HashSet::new();
        for cand_id in candidate_identities {
            let cand_hash = Self::compute_cache_key_for_identity(&cand_id, canonical);
            if cand_hash != primary_hash && seen_hashes.insert(cand_hash.clone()) {
                candidate_hashes.push(cand_hash);
            }
        }

        Ok(candidate_hashes)
    }
}

#[async_trait]
impl AiProvider for CachingAiProvider {
    fn map_cached_inner(
        &self,
        decorator: &mut Option<AiProviderDecorator>,
    ) -> Option<Arc<dyn AiProvider>> {
        let f = decorator.take()?;
        Some(Arc::new(Self {
            inner: f(self.inner.clone()),
            store: self.store.clone(),
            replay: self.replay,
            resume: self.resume,
        }))
    }

    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let canonical = Self::canonical_request_json(&request);
        let hash = Self::compute_cache_key_for_identity(&self.inner.cache_identity(), &canonical);
        let hash_prefix = &hash[..12];

        if self.replay {
            let mut cached_entry = None;
            let mut rows = self
                .store
                .conn
                .query(
                    "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                    libsql::params![hash.clone()],
                )
                .await?;

            if let Some(row) = rows.next().await? {
                let response_json: String = row.get(0)?;
                let tokens_saved: i64 = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                cached_entry = Some((
                    response_json,
                    tokens_saved,
                    created_at,
                    hash_prefix.to_string(),
                ));
            }
            drop(rows);

            if cached_entry.is_none() && self.resume {
                let candidate_hashes = self.candidate_resume_hashes(&canonical, &hash).await?;
                for cand_hash in candidate_hashes {
                    let mut rows = self
                        .store
                        .conn
                        .query(
                            "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                            libsql::params![cand_hash.clone()],
                        )
                        .await?;
                    if let Some(row) = rows.next().await? {
                        let response_json: String = row.get(0)?;
                        let tokens_saved: i64 = row.get(1)?;
                        let created_at: i64 = row.get(2)?;
                        let cand_prefix = cand_hash[..12].to_string();
                        cached_entry = Some((response_json, tokens_saved, created_at, cand_prefix));
                        break;
                    }
                }
            }

            if let Some((response_json, tokens_saved, created_at, hit_prefix)) = cached_entry
                && let Ok(mut resp) = serde_json::from_str::<AiResponse>(&response_json)
            {
                let (origin, total) = if created_at >= self.store.session_start {
                    self.store.hits_this.fetch_add(1, Ordering::Relaxed);
                    let t = self
                        .store
                        .tokens_saved_this
                        .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                        + tokens_saved as u64;
                    ("this session", t)
                } else {
                    self.store.hits_prev.fetch_add(1, Ordering::Relaxed);
                    let t = self
                        .store
                        .tokens_saved_prev
                        .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                        + tokens_saved as u64;
                    ("previous session", t)
                };
                info!(
                    "Cache hit [{}] ({}) — {} tokens saved (total {}: {})",
                    hit_prefix, origin, tokens_saved, origin, total
                );
                if let Some(ref mut usage) = resp.usage {
                    // The hit serves the whole prompt from this cache, so all
                    // of it counts as cached.  cached_tokens is a breakdown
                    // of prompt_tokens rather than an addend.  The count
                    // recorded with the response covers this same prompt.
                    usage.cached_tokens = Some(usage.prompt_tokens);
                }
                return Ok(resp);
            }
        }

        debug!("Cache miss [{}]", hash_prefix);

        let resp = self.inner.generate_content(request.clone()).await?;

        let response_json = serde_json::to_string(&resp)?;
        let request_json = serde_json::to_string(&request)?;
        let caps = self.inner.get_capabilities();
        let cache_id = self.inner.cache_identity();
        let tokens_saved = resp
            .usage
            .as_ref()
            .map(|u| u.prompt_tokens + u.completion_tokens)
            .unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let _ = self
            .store
            .conn
            .execute(
                "INSERT OR REPLACE INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    hash,
                    cache_id,
                    caps.model_name,
                    request_json,
                    response_json,
                    tokens_saved as i64,
                    now
                ],
            )
            .await;

        Ok(resp)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_identity(&self) -> String {
        self.inner.cache_identity()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        Some(CacheStats {
            hits_this_session: self.store.hits_this.load(Ordering::Relaxed),
            hits_prev_session: self.store.hits_prev.load(Ordering::Relaxed),
            tokens_saved_this_session: self.store.tokens_saved_this.load(Ordering::Relaxed),
            tokens_saved_prev_session: self.store.tokens_saved_prev.load(Ordering::Relaxed),
        })
    }

    async fn forget(&self, request: &AiRequest) {
        let canonical = Self::canonical_request_json(request);
        let hash = Self::compute_cache_key_for_identity(&self.inner.cache_identity(), &canonical);
        let mut hashes_to_delete = vec![hash];
        let prefix = get_log_prefix();
        if self.resume {
            match self
                .candidate_resume_hashes(&canonical, &hashes_to_delete[0])
                .await
            {
                Ok(cands) => hashes_to_delete.extend(cands),
                Err(e) => {
                    warn!(
                        "{}Failed to resolve candidate resume hashes to forget: {:#}",
                        prefix, e
                    );
                }
            }
        }
        for h in &hashes_to_delete {
            let result = self
                .store
                .conn
                .execute(
                    "DELETE FROM response_cache WHERE request_hash = ?",
                    libsql::params![h.clone()],
                )
                .await;
            match result {
                Ok(0) => debug!("{}Cache forget [{}]: nothing to forget", prefix, &h[..12]),
                Ok(_) => info!("{}Cache forget [{}]", prefix, &h[..12]),
                Err(e) => warn!("{}Cache forget [{}] failed: {:#}", prefix, &h[..12], e),
            }
        }
        self.inner.forget(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
    use serde_json::Value;

    /// Answers every request with text the session below rejects, and counts
    /// how often it was actually asked.
    struct RejectedAnswers {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedAnswers {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some("not json".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    struct JsonSession;

    #[async_trait]
    impl LlmSession for JsonSession {
        type Output = Value;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "answer in JSON".to_string()
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            unreachable!("the session offers no tools")
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            serde_json::from_str(response.content.as_deref().unwrap_or(""))
                .map_err(|e| ValidationError::FormatViolation(e.to_string()))
        }
    }

    /// Calls a tool on its first turn, then answers with text the session
    /// rejects. Counts how often it was actually asked.
    struct ToolThenRejected {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for ToolThenRejected {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let has_tool_result = request
                .messages
                .iter()
                .any(|m| m.role == crate::ai::AiRole::Tool);
            Ok(AiResponse {
                content: has_tool_result.then(|| "not json".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: (!has_tool_result).then(|| {
                    vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "read".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }]
                }),
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    struct ToolThenJsonSession;

    #[async_trait]
    impl LlmSession for ToolThenJsonSession {
        type Output = Value;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "read, then answer in JSON".to_string()
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            Ok(serde_json::json!({ "ok": true }))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            serde_json::from_str(response.content.as_deref().unwrap_or(""))
                .map_err(|e| ValidationError::FormatViolation(e.to_string()))
        }
    }

    #[tokio::test]
    async fn test_a_retry_still_replays_the_turns_that_were_accepted() {
        // Only the rejected answers are forgotten. The tool call turn before
        // them was fine, so the retry is served it from the cache and asks
        // the model again only for the answer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(ToolThenRejected {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider)
                .run(&mut ToolThenJsonSession)
                .await;
            assert!(result.is_err(), "every answer is rejected");
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        // Attempt 1: the tool call turn, then three rejected answers.
        // Attempt 2: the tool call turn from the cache, then three answers.
        assert_eq!(per_attempt, vec![4, 3]);
    }

    /// Rejects the first answer of a conversation, accepts the answer given
    /// after the rejection, and counts how often it was actually asked.
    struct RejectedOnce {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedOnce {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first_answer = request.messages.len() == 1;
            Ok(AiResponse {
                content: Some(if first_answer { "not json" } else { "{}" }.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_a_stage_that_recovers_stays_cached() {
        // The first answer is rejected and the next one accepted, so the
        // stage succeeds. Nothing is forgotten, and a later run is served
        // the whole exchange from the cache.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_run = Vec::new();
        for _run in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedOnce {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(result.is_ok(), "the second answer is accepted");
            per_run.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(per_run, vec![2, 0], "calls per run");
    }

    /// Answers every request with a response the provider marked as cut off
    /// at the output limit, and counts how often it was actually asked.
    struct TruncatedAnswers {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for TruncatedAnswers {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some("{\"conc".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: true,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_a_retry_after_a_truncated_answer_reaches_the_model() {
        // The runner gives up on a truncated answer before validating it.
        // The answer was still a successful API response, so it is cached,
        // and a retry must not be handed it again.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(TruncatedAnswers {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(result.is_err(), "a truncated answer ends the session");
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(per_attempt, vec![1, 1], "calls per attempt");
    }

    /// Rejects the first answer of a conversation, then cuts the next one
    /// off, and counts how often it was actually asked.
    struct RejectedThenTruncated {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedThenTruncated {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first_answer = request.messages.len() == 1;
            Ok(AiResponse {
                content: Some(if first_answer { "not json" } else { "{\"conc" }.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: !first_answer,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_giving_up_on_a_truncated_answer_also_forgets_the_rejected_ones() {
        // A rejected answer, then a truncated one: the stage gives up, and a
        // retry must be asked both again rather than be served the rejection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedThenTruncated {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(
                result.is_err(),
                "the stage gives up on the truncated answer"
            );
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(per_attempt, vec![2, 2], "calls per attempt");
    }

    #[tokio::test]
    async fn test_a_retry_after_rejected_answers_reaches_the_model() {
        // local_review retries a failed patch review by building a fresh
        // cached provider over the same cache file and running again. The
        // first attempt fails because every answer is rejected. A retry is
        // there to ask the model again, so it must reach the model at least
        // once rather than be served the rejected answers from the cache.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedAnswers {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(result.is_err(), "every answer is rejected");
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(
            per_attempt[0], 3,
            "attempt 1 asks until the validation limit"
        );
        assert!(
            per_attempt[1] > 0,
            "the retry never reached the model: calls per attempt {:?}",
            per_attempt
        );
    }

    /// Answers every request with JSON the session accepts, and counts how
    /// often it was actually asked.
    struct AcceptedAnswers {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for AcceptedAnswers {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some("{}".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_a_recording_run_asks_afresh_and_leaves_its_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));
        let provider = |record_only: bool| {
            let calls = calls.clone();
            let path = path.clone();
            async move {
                let cached = CachingAiProvider::new(
                    Arc::new(AcceptedAnswers { calls }),
                    path.to_str().unwrap(),
                    30,
                )
                .await
                .unwrap();
                if record_only {
                    cached.record_only()
                } else {
                    cached
                }
            }
        };

        // The second recording run asks again for the answer the first one
        // recorded.
        for _run in 1..=2 {
            SessionRunner::new(&provider(true).await)
                .run(&mut JsonSession)
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        SessionRunner::new(&provider(false).await)
            .run(&mut JsonSession)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a replaying run is served the recorded answer"
        );
    }

    #[tokio::test]
    async fn test_the_prompt_date_does_not_reach_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let provider = CachingAiProvider::new(
            Arc::new(RejectedAnswers {
                calls: Arc::new(AtomicU64::new(0)),
            }),
            path.to_str().unwrap(),
            30,
        )
        .await
        .unwrap();
        let key = |system: String| {
            provider.compute_cache_key(&AiRequest {
                system: Some(system),
                ..Default::default()
            })
        };

        // A week of dates, as the workflows format them.
        let keys: std::collections::HashSet<String> = (0..7)
            .map(|day| {
                let date =
                    chrono::NaiveDate::from_ymd_opt(2026, 10, 25).unwrap() + chrono::Days::new(day);
                key(format!(
                    "the current date is {}. Review this.",
                    date.format("%A, %B %d, %Y")
                ))
            })
            .collect();
        assert_eq!(keys.len(), 1);
        assert!(!keys.contains(&key(
            "the current date is Sunday, October 25, 2026. Review that.".to_string()
        )));

        // A date in the patch below it still counts.
        let patch = |date: &str| {
            key(format!(
                "the current date is Sunday, October 25, 2026.\n+ * {date}: fixed"
            ))
        };
        assert_ne!(
            patch("Monday, October 26, 2026"),
            patch("Tuesday, October 27, 2026")
        );
    }

    #[tokio::test]
    async fn test_cross_model_replay_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let path_str = path.to_str().unwrap();

        struct MockModelProvider {
            model: String,
            identity: String,
            calls: Arc<AtomicU64>,
        }

        #[async_trait]
        impl AiProvider for MockModelProvider {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(AiResponse {
                    content: Some(format!("answer from {}", self.model)),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: Some(crate::ai::AiUsage {
                        prompt_tokens: 100,
                        completion_tokens: 20,
                        total_tokens: 120,
                        cached_tokens: None,
                    }),
                    truncated: false,
                })
            }

            fn get_capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    model_name: self.model.clone(),
                    context_window_size: 128_000,
                }
            }

            fn cache_identity(&self) -> String {
                self.identity.clone()
            }
        }

        let calls1 = Arc::new(AtomicU64::new(0));
        let model1 = Arc::new(MockModelProvider {
            model: "model-a".to_string(),
            identity: crate::ai::cache_identity_with("model-a", &[("thinking", Some("high"))]),
            calls: calls1.clone(),
        });

        let cached1 = CachingAiProvider::new(model1, path_str, 30).await.unwrap();
        let request = AiRequest {
            messages: vec![crate::ai::AiMessage {
                role: crate::ai::AiRole::User,
                content: Some("test prompt".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            ..Default::default()
        };

        // First model generates and caches the answer.
        let resp1 = cached1.generate_content(request.clone()).await.unwrap();
        assert_eq!(resp1.content.as_deref(), Some("answer from model-a"));
        assert_eq!(calls1.load(Ordering::SeqCst), 1);

        // Second model with different model_name and knobs, without resume:
        // misses the cache because cache identities differ.
        let calls2 = Arc::new(AtomicU64::new(0));
        let model2 = Arc::new(MockModelProvider {
            model: "model-b".to_string(),
            identity: crate::ai::cache_identity_with("model-b", &[("thinking", Some("medium"))]),
            calls: calls2.clone(),
        });
        let cached2_no_resume = CachingAiProvider::new(model2.clone(), path_str, 30)
            .await
            .unwrap();
        let resp2 = cached2_no_resume
            .generate_content(request.clone())
            .await
            .unwrap();
        assert_eq!(resp2.content.as_deref(), Some("answer from model-b"));
        assert_eq!(calls2.load(Ordering::SeqCst), 1);

        // Third model with with_resume(true):
        // successfully replayed from model-a (or model-b) across models.
        let calls3 = Arc::new(AtomicU64::new(0));
        let model3 = Arc::new(MockModelProvider {
            model: "model-c".to_string(),
            identity: crate::ai::cache_identity_with("model-c", &[("thinking", Some("high"))]),
            calls: calls3.clone(),
        });
        let cached3_resume = CachingAiProvider::new(model3.clone(), path_str, 30)
            .await
            .unwrap()
            .with_resume(true);
        let resp3 = cached3_resume
            .generate_content(request.clone())
            .await
            .unwrap();
        // Model was never called because cached turn was replayed
        assert_eq!(calls3.load(Ordering::SeqCst), 0);
        assert!(resp3.content.is_some());

        // Calling forget() with resume: true clears both primary and candidate hashes.
        cached3_resume.forget(&request).await;

        // Next generate_content() with model-c must now miss and call model-c.
        let resp4 = cached3_resume
            .generate_content(request.clone())
            .await
            .unwrap();
        assert_eq!(resp4.content.as_deref(), Some("answer from model-c"));
        assert_eq!(calls3.load(Ordering::SeqCst), 1);
    }
}

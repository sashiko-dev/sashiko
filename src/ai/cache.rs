use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use crate::sqlite_transaction::SqliteTransaction;

use super::{
    AiProvider, AiRequest, AiResponse, CacheStats, MAX_PROVIDER_METADATA_BYTES,
    ProviderCapabilities, get_log_prefix,
};

const MAX_CACHE_ENTRY_BYTES: usize = MAX_PROVIDER_METADATA_BYTES;
const MAX_CACHE_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
const PAYLOAD_BYTES_SQL: &str = "SELECT total_bytes FROM response_cache_payload WHERE id = 1";
const PRUNE_CANDIDATES_SQL: &str =
    "SELECT rowid, entry_bytes FROM response_cache ORDER BY created_at, rowid";

struct CacheEntry {
    hash: String,
    provider: String,
    model: String,
    request_json: String,
    response_json: String,
    tokens_saved: i64,
    created_at: i64,
}

pub struct CachingAiProvider {
    inner: Arc<dyn AiProvider>,
    conn: libsql::Connection,
    session_start: i64,
    hits_this: AtomicU64,
    hits_prev: AtomicU64,
    tokens_saved_this: AtomicU64,
    tokens_saved_prev: AtomicU64,
    connection_lock: Arc<tokio::sync::Mutex<()>>,
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
                created_at INTEGER NOT NULL,
                entry_bytes INTEGER NOT NULL DEFAULT 0
            );",
        )
        .await?;

        Self::migrate_payload_accounting(&conn).await?;

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

        if let Err(error) = async {
            let transaction = SqliteTransaction::begin(&conn, Arc::default()).await?;
            Self::prune_payload(&transaction, MAX_CACHE_PAYLOAD_BYTES).await?;
            transaction.commit().await
        }
        .await
        {
            warn!("Response cache: startup pruning failed: {error:#}");
        }

        let cache = Self {
            inner,
            conn,
            session_start,
            hits_this: AtomicU64::new(0),
            hits_prev: AtomicU64::new(0),
            tokens_saved_this: AtomicU64::new(0),
            tokens_saved_prev: AtomicU64::new(0),
            connection_lock: Arc::default(),
        };

        info!("Response cache enabled ({})", cache_path);
        Ok(cache)
    }

    async fn migrate_payload_accounting(connection: &libsql::Connection) -> Result<()> {
        // Serialize schema inspection and migration across cache instances,
        // and roll back the new column if its backfill fails.
        let transaction = SqliteTransaction::begin(connection, Arc::default())
            .await
            .context("Response cache: failed to start payload accounting migration")?;
        let has_entry_bytes = {
            let mut rows = transaction
                .query(
                    "SELECT 1 FROM pragma_table_info('response_cache') \
                     WHERE name = 'entry_bytes' COLLATE NOCASE",
                    (),
                )
                .await
                .context("Response cache: failed to inspect payload accounting schema")?;
            rows.next()
                .await
                .context("Response cache: failed to read payload accounting schema")?
                .is_some()
        };
        if !has_entry_bytes {
            transaction
                .execute(
                    "ALTER TABLE response_cache ADD COLUMN entry_bytes INTEGER NOT NULL DEFAULT 0",
                    (),
                )
                .await
                .context("Response cache: failed to add entry_bytes column")?;
        }
        transaction
            .execute(
                "UPDATE response_cache SET entry_bytes = \
                    length(CAST(request_json AS BLOB)) + \
                    length(CAST(response_json AS BLOB)) \
                 WHERE entry_bytes = 0",
                (),
            )
            .await
            .context("Response cache: failed to backfill entry_bytes")?;
        // The implicit row ID breaks timestamp ties without a sort. Keep
        // payload accounting in SQLite so all connections share one total.
        transaction
            .execute_batch(
                "DROP INDEX IF EXISTS response_cache_created_at_idx;
                 CREATE INDEX IF NOT EXISTS response_cache_payload_order_idx
                     ON response_cache(created_at);
                 CREATE TABLE IF NOT EXISTS response_cache_payload (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0)
                 );
                 INSERT OR REPLACE INTO response_cache_payload
                     SELECT 1, COALESCE(SUM(entry_bytes), 0) FROM response_cache;
                 CREATE TRIGGER IF NOT EXISTS response_cache_payload_insert
                     AFTER INSERT ON response_cache BEGIN
                     UPDATE response_cache_payload
                         SET total_bytes = total_bytes + NEW.entry_bytes WHERE id = 1;
                 END;
                 CREATE TRIGGER IF NOT EXISTS response_cache_payload_delete
                     AFTER DELETE ON response_cache BEGIN
                     UPDATE response_cache_payload
                         SET total_bytes = total_bytes - OLD.entry_bytes WHERE id = 1;
                 END;
                 CREATE TRIGGER IF NOT EXISTS response_cache_payload_update
                     AFTER UPDATE OF entry_bytes ON response_cache BEGIN
                     UPDATE response_cache_payload
                         SET total_bytes = total_bytes - OLD.entry_bytes + NEW.entry_bytes
                         WHERE id = 1;
                 END;",
            )
            .await
            .context("Response cache: failed to initialize payload pruning index")?;
        transaction
            .commit()
            .await
            .context("Response cache: failed to commit payload accounting migration")
    }

    // Synchronous JSON round-trip on the async thread.  The request is
    // already in memory and typical payloads are single-digit KB; the
    // serialize-hash takes microseconds, well under Tokio's cooperative
    // budget.  spawn_blocking would add more overhead than it saves.
    fn compute_cache_key(&self, request: &AiRequest) -> String {
        let mut val = serde_json::to_value(request).unwrap_or_default();
        // Strip nondeterministic fields
        if let serde_json::Value::Object(ref mut map) = val {
            map.remove("context_tag");
        }
        super::scrub_thought_signatures(&mut val);
        let canonical = serde_json::to_string(&val).unwrap_or_default();
        // The model and the provider's own knobs never appear in the request,
        // so hash them alongside it. Without them a raised reasoning effort
        // replays the answer recorded at the lower one.
        let mut hasher = Sha256::new();
        hasher.update(self.inner.cache_identity().as_bytes());
        hasher.update(b"\0");
        hasher.update(canonical.as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }

    fn diagnostic_request_json(request: &AiRequest) -> Result<String> {
        let mut value = serde_json::to_value(request)?;
        if let Some(messages) = value
            .get_mut("messages")
            .and_then(|value| value.as_array_mut())
        {
            for message in messages {
                if let Some(message) = message.as_object_mut() {
                    message.remove("provider_metadata");
                }
            }
        }
        Ok(serde_json::to_string(&value)?)
    }

    #[cfg(test)]
    async fn payload_bytes(&self) -> Result<usize> {
        let _guard = self.connection_lock.lock().await;
        Ok(Self::read_payload_bytes(&self.conn).await? as usize)
    }

    async fn read_payload_bytes(connection: &libsql::Connection) -> Result<i64> {
        let mut rows = connection.query(PAYLOAD_BYTES_SQL, ()).await?;
        Ok(rows
            .next()
            .await?
            .context("Response cache: missing payload accounting total")?
            .get(0)?)
    }

    async fn prune_payload(connection: &libsql::Connection, limit: usize) -> Result<()> {
        let mut remaining = Self::read_payload_bytes(connection).await?;
        if remaining <= limit as i64 {
            return Ok(());
        }
        let mut rows = connection.query(PRUNE_CANDIDATES_SQL, ()).await?;
        let mut evicted = Vec::new();
        while remaining > limit as i64 {
            let row = rows
                .next()
                .await?
                .context("Response cache: payload total exceeds stored entries")?;
            evicted.push(row.get::<i64>(0)?);
            remaining -= row.get::<i64>(1)?;
        }
        // Finish the read before changing rows on the same SQLite handle.
        drop(rows);
        let statement = connection
            .prepare("DELETE FROM response_cache WHERE rowid = ?")
            .await?;
        for rowid in evicted {
            statement.reset();
            statement.execute(libsql::params![rowid]).await?;
        }
        Ok(())
    }

    async fn store_entry(&self, entry: CacheEntry) -> Result<()> {
        let entry_bytes = entry
            .request_json
            .len()
            .saturating_add(entry.response_json.len());
        if entry_bytes > MAX_CACHE_ENTRY_BYTES {
            warn!(
                "Response cache: skipped {}-byte entry exceeding the {}-byte limit",
                entry_bytes, MAX_CACHE_ENTRY_BYTES
            );
            return Ok(());
        }

        let transaction =
            SqliteTransaction::begin(&self.conn, self.connection_lock.clone()).await?;
        // REPLACE skips delete triggers unless recursive_triggers is enabled.
        // Explicit deletion keeps the total correct and renews insertion order.
        transaction
            .execute(
                "DELETE FROM response_cache WHERE request_hash = ?",
                libsql::params![entry.hash.as_str()],
            )
            .await?;
        transaction
            .execute(
                "INSERT INTO response_cache \
                 (request_hash, provider, model, request_json, response_json, \
                  tokens_saved, created_at, entry_bytes) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    entry.hash,
                    entry.provider,
                    entry.model,
                    entry.request_json,
                    entry.response_json,
                    entry.tokens_saved,
                    entry.created_at,
                    entry_bytes as i64
                ],
            )
            .await?;
        Self::prune_payload(&transaction, MAX_CACHE_PAYLOAD_BYTES).await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[async_trait]
impl AiProvider for CachingAiProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let hash = self.compute_cache_key(&request);
        let hash_prefix = &hash[..12];

        let entry = {
            let _guard = self.connection_lock.lock().await;
            let mut rows = self
                .conn
                .query(
                    "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                    libsql::params![hash.clone()],
                )
                .await?;
            rows.next()
                .await?
                .map(|row| {
                    Ok::<_, libsql::Error>((
                        row.get::<String>(0)?,
                        row.get::<i64>(1)?,
                        row.get::<i64>(2)?,
                    ))
                })
                .transpose()?
        };

        if let Some((response_json, tokens_saved, created_at)) = entry {
            if response_json.len() > MAX_CACHE_ENTRY_BYTES {
                debug!(
                    "Cache hit [{hash_prefix}]: oversized legacy entry ({} bytes), skipping",
                    response_json.len()
                );
            } else {
                if let Ok(mut resp) = serde_json::from_str::<AiResponse>(&response_json) {
                    let (origin, total) = if created_at >= self.session_start {
                        self.hits_this.fetch_add(1, Ordering::Relaxed);
                        let t = self
                            .tokens_saved_this
                            .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                            + tokens_saved as u64;
                        ("this session", t)
                    } else {
                        self.hits_prev.fetch_add(1, Ordering::Relaxed);
                        let t = self
                            .tokens_saved_prev
                            .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                            + tokens_saved as u64;
                        ("previous session", t)
                    };
                    info!(
                        "Cache hit [{}] ({}) — {} tokens saved (total {}: {})",
                        hash_prefix, origin, tokens_saved, origin, total
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
        }
        debug!("Cache miss [{}]", hash_prefix);

        let resp = self.inner.generate_content(request.clone()).await?;

        let response_json = serde_json::to_string(&resp)?;
        let request_json = Self::diagnostic_request_json(&request)?;
        let caps = self.inner.get_capabilities();
        let tokens_saved = resp
            .usage
            .as_ref()
            .map(|u| u.prompt_tokens + u.completion_tokens)
            .unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if let Err(error) = self
            .store_entry(CacheEntry {
                hash,
                provider: caps.model_name.clone(),
                model: caps.model_name,
                request_json,
                response_json,
                tokens_saved: tokens_saved as i64,
                created_at: now,
            })
            .await
        {
            warn!("Response cache: failed to store entry: {error:#}");
        }

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
            hits_this_session: self.hits_this.load(Ordering::Relaxed),
            hits_prev_session: self.hits_prev.load(Ordering::Relaxed),
            tokens_saved_this_session: self.tokens_saved_this.load(Ordering::Relaxed),
            tokens_saved_prev_session: self.tokens_saved_prev.load(Ordering::Relaxed),
        })
    }

    async fn forget(&self, request: &AiRequest) {
        let hash = self.compute_cache_key(request);
        // Like the insert above, a failure here is not fatal: the worst case
        // is that a retry is served this answer again, as it was before.
        let result = {
            let _guard = self.connection_lock.lock().await;
            self.conn
                .execute(
                    "DELETE FROM response_cache WHERE request_hash = ?",
                    libsql::params![hash.clone()],
                )
                .await
        };
        let prefix = get_log_prefix();
        match result {
            Ok(0) => debug!(
                "{}Cache forget [{}]: nothing to forget",
                prefix,
                &hash[..12]
            ),
            Ok(_) => info!("{}Cache forget [{}]", prefix, &hash[..12]),
            Err(e) => warn!("{}Cache forget [{}] failed: {:#}", prefix, &hash[..12], e),
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
                provider_metadata: None,
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
                provider_metadata: None,
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
                provider_metadata: None,
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
                provider_metadata: None,
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
                provider_metadata: None,
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
}

#[cfg(test)]
mod payload_tests {
    use super::*;
    use crate::ai::{AiMessage, AiProviderMetadata, AiRole};
    use serde_json::{Value, json};

    struct FixedProvider;

    #[async_trait]
    impl AiProvider for FixedProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: Some("response".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
                provider_metadata: None,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "test".to_string(),
                context_window_size: 4096,
            }
        }
    }

    fn request_with_metadata(data: serde_json::Value) -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
                provider_metadata: Some(AiProviderMetadata {
                    provider: "test.provider".to_string(),
                    version: 1,
                    data,
                }),
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    async fn test_cache() -> Result<(tempfile::TempDir, CachingAiProvider)> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("response-cache.db");
        let cache = CachingAiProvider::new(
            Arc::new(FixedProvider),
            path.to_str().expect("temporary path must be UTF-8"),
            7,
        )
        .await?;
        Ok((temp, cache))
    }

    const LEGACY_REQUEST: &str = "{\"prompt\":\"ž\"}";
    const LEGACY_RESPONSE: &str = "{\"content\":\"réponse\"}";

    async fn legacy_cache() -> Result<(tempfile::TempDir, libsql::Connection)> {
        let temp = tempfile::tempdir()?;
        let db = libsql::Builder::new_local(temp.path().join("response-cache.db"))
            .build()
            .await?;
        let connection = db.connect()?;
        connection
            .execute_batch(
                "CREATE TABLE response_cache (
                    request_hash TEXT PRIMARY KEY,
                    provider TEXT NOT NULL,
                    model TEXT NOT NULL,
                    request_json TEXT NOT NULL,
                    response_json TEXT NOT NULL,
                    tokens_saved INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                );",
            )
            .await?;
        connection
            .execute(
                "INSERT INTO response_cache VALUES ('legacy', 'test', 'test', ?, ?, 0, ?)",
                libsql::params![
                    LEGACY_REQUEST,
                    LEGACY_RESPONSE,
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64
                ],
            )
            .await?;
        Ok((temp, connection))
    }

    #[tokio::test]
    async fn legacy_migration_accounts_for_bytes_and_is_idempotent() -> Result<()> {
        let (temp, _connection) = legacy_cache().await?;
        let path = temp.path().join("response-cache.db");
        let legacy_bytes = LEGACY_REQUEST.len() + LEGACY_RESPONSE.len();
        for _ in 0..2 {
            let cache = CachingAiProvider::new(
                Arc::new(FixedProvider),
                path.to_str().expect("temporary path must be UTF-8"),
                7,
            )
            .await?;
            assert_eq!(cache.payload_bytes().await?, legacy_bytes);
        }

        let cache = CachingAiProvider::new(
            Arc::new(FixedProvider),
            path.to_str().expect("temporary path must be UTF-8"),
            7,
        )
        .await?;
        cache
            .store_entry(CacheEntry {
                hash: "new".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                request_json: "{}".to_string(),
                response_json: "{}".to_string(),
                tokens_saved: 0,
                created_at: cache.session_start + 1,
            })
            .await?;
        assert_eq!(cache.payload_bytes().await?, legacy_bytes + 4);
        // Legacy payloads must count toward the limit used by pruning.
        CachingAiProvider::prune_payload(&cache.conn, legacy_bytes).await?;
        assert_eq!(cache.payload_bytes().await?, 4);
        Ok(())
    }

    #[tokio::test]
    async fn payload_counter_tracks_replacement_other_connections_and_rollback() -> Result<()> {
        let (temp, cache) = test_cache().await?;
        for bytes in [10, 20] {
            cache
                .store_entry(CacheEntry {
                    hash: "replaced".to_string(),
                    provider: "test".to_string(),
                    model: "test".to_string(),
                    request_json: "r".repeat(bytes),
                    response_json: "{}".to_string(),
                    tokens_saved: 0,
                    created_at: cache.session_start,
                })
                .await?;
            assert_eq!(cache.payload_bytes().await?, bytes + 2);
        }
        let db = libsql::Builder::new_local(temp.path().join("response-cache.db"))
            .build()
            .await?;
        let other = db.connect()?;
        let transaction = SqliteTransaction::begin(&other, Arc::default()).await?;
        transaction
            .execute("UPDATE response_cache SET entry_bytes = 100", ())
            .await?;
        assert_eq!(
            CachingAiProvider::read_payload_bytes(&transaction).await?,
            100
        );
        assert_eq!(cache.payload_bytes().await?, 22);
        transaction.rollback().await?;
        assert_eq!(cache.payload_bytes().await?, 22);
        other
            .execute("UPDATE response_cache SET entry_bytes = 30", ())
            .await?;
        assert_eq!(cache.payload_bytes().await?, 30);
        other.execute("DELETE FROM response_cache", ()).await?;
        assert_eq!(cache.payload_bytes().await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn failed_eviction_rolls_back_insert_and_payload_counter() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        cache
            .conn
            .execute(
                "INSERT INTO response_cache VALUES \
                 ('old', 'test', 'test', '{}', '{}', 0, 1, ?)",
                libsql::params![MAX_CACHE_PAYLOAD_BYTES as i64],
            )
            .await?;
        cache
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_eviction BEFORE DELETE ON response_cache
                 BEGIN SELECT RAISE(ABORT, 'injected eviction failure'); END;",
            )
            .await?;
        let error = cache
            .store_entry(CacheEntry {
                hash: "new".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                request_json: "{}".to_string(),
                response_json: "{}".to_string(),
                tokens_saved: 0,
                created_at: 2,
            })
            .await
            .expect_err("failed eviction must reject the cache write");
        assert!(error.to_string().contains("injected eviction failure"));
        assert_eq!(cache.payload_bytes().await?, MAX_CACHE_PAYLOAD_BYTES);
        let mut rows = cache
            .conn
            .query("SELECT request_hash FROM response_cache", ())
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<String>(0)?, "old");
        assert!(rows.next().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn pruning_below_limit_does_not_access_cache_entries() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        // Renaming the table makes any attempt to scan response_cache fail.
        cache
            .conn
            .execute("ALTER TABLE response_cache RENAME TO hidden_cache", ())
            .await?;
        CachingAiProvider::prune_payload(&cache.conn, MAX_CACHE_PAYLOAD_BYTES).await?;
        Ok(())
    }

    #[tokio::test]
    async fn backfill_failure_aborts_initialization_and_allows_retry() -> Result<()> {
        for already_migrated in [false, true] {
            let (temp, connection) = legacy_cache().await?;
            if already_migrated {
                connection
                    .execute(
                        "ALTER TABLE response_cache ADD COLUMN entry_bytes INTEGER NOT NULL DEFAULT 0",
                        (),
                    )
                    .await?;
            }
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_backfill BEFORE UPDATE ON response_cache
                     BEGIN SELECT RAISE(ABORT, 'injected backfill failure'); END;",
                )
                .await?;
            let path = temp.path().join("response-cache.db");
            let error = CachingAiProvider::new(
                Arc::new(FixedProvider),
                path.to_str().expect("temporary path must be UTF-8"),
                7,
            )
            .await
            .err()
            .context("backfill failure must reject initialization")?;
            assert!(error.to_string().contains("failed to backfill entry_bytes"));
            assert!(format!("{error:#}").contains("injected backfill failure"));

            let mut rows = connection
                .query(
                    "SELECT 1 FROM pragma_table_info('response_cache') WHERE name = 'entry_bytes'",
                    (),
                )
                .await?;
            assert_eq!(rows.next().await?.is_some(), already_migrated);
            drop(rows);
            connection
                .execute("DROP TRIGGER reject_backfill", ())
                .await?;
            let cache = CachingAiProvider::new(
                Arc::new(FixedProvider),
                path.to_str().expect("temporary path must be UTF-8"),
                7,
            )
            .await?;
            assert_eq!(
                cache.payload_bytes().await?,
                LEGACY_REQUEST.len() + LEGACY_RESPONSE.len()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn schema_change_failure_is_propagated() -> Result<()> {
        let db = libsql::Builder::new_local(":memory:").build().await?;
        let connection = db.connect()?;
        // A view exposes columns for inspection but rejects ALTER TABLE.
        connection
            .execute(
                "CREATE VIEW response_cache AS SELECT '{}' AS request_json",
                (),
            )
            .await?;
        let error = CachingAiProvider::migrate_payload_accounting(&connection)
            .await
            .expect_err("a failed schema change must fail migration");
        assert!(
            error
                .to_string()
                .contains("failed to add entry_bytes column")
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_lock_failure_is_propagated() -> Result<()> {
        let (temp, connection) = legacy_cache().await?;
        let db = libsql::Builder::new_local(temp.path().join("response-cache.db"))
            .build()
            .await?;
        let competing_connection = db.connect()?;
        let transaction = SqliteTransaction::begin(&competing_connection, Arc::default()).await?;
        let error = CachingAiProvider::migrate_payload_accounting(&connection)
            .await
            .expect_err("a locked database must fail migration");
        assert!(
            error
                .to_string()
                .contains("failed to start payload accounting migration")
        );
        transaction.rollback().await?;
        CachingAiProvider::migrate_payload_accounting(&connection).await?;
        Ok(())
    }

    #[tokio::test]
    async fn reads_wait_for_transaction_rollback() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        let request = request_with_metadata(Value::Null);
        let transaction =
            SqliteTransaction::begin(&cache.conn, cache.connection_lock.clone()).await?;
        transaction
            .execute(
                "INSERT INTO response_cache VALUES (?, 'test', 'test', '{}', ?, 0, 1, 1)",
                libsql::params![
                    cache.compute_cache_key(&request),
                    serde_json::to_string(&AiResponse {
                        content: Some("uncommitted".to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                        provider_metadata: None,
                    })?
                ],
            )
            .await?;
        let mut read = Box::pin(cache.generate_content(request));
        assert!(futures::poll!(read.as_mut()).is_pending());
        transaction.rollback().await?;
        assert_eq!(read.await?.content.as_deref(), Some("response"));
        Ok(())
    }

    #[tokio::test]
    async fn invalidation_waits_for_active_transaction() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        let request = request_with_metadata(Value::Null);
        cache.generate_content(request.clone()).await?;
        let transaction =
            SqliteTransaction::begin(&cache.conn, cache.connection_lock.clone()).await?;
        let mut forget = Box::pin(cache.forget(&request));
        assert!(futures::poll!(forget.as_mut()).is_pending());
        transaction.rollback().await?;
        forget.await;
        let mut rows = cache
            .conn
            .query("SELECT COUNT(*) FROM response_cache", ())
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<i64>(0)?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn pruning_uses_index_and_keeps_newest_entries_with_tied_timestamps() -> Result<()> {
        let (temp, connection) = legacy_cache().await?;
        connection
            .execute(
                "CREATE INDEX response_cache_created_at_idx ON response_cache(created_at DESC)",
                (),
            )
            .await?;
        let path = temp.path().join("response-cache.db");
        let cache =
            CachingAiProvider::new(Arc::new(FixedProvider), path.to_str().unwrap(), 7).await?;
        let mut rows = cache
            .conn
            .query(&format!("EXPLAIN QUERY PLAN {PRUNE_CANDIDATES_SQL}"), ())
            .await?;
        let mut details = Vec::new();
        while let Some(row) = rows.next().await? {
            details.push(row.get::<String>(3)?);
        }
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("response_cache_payload_order_idx"))
        );
        assert!(
            details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
            "{details:?}"
        );
        drop(rows);
        cache.conn.execute("DELETE FROM response_cache", ()).await?;
        for index in 1..=3 {
            cache
                .store_entry(CacheEntry {
                    hash: format!("entry-{index}"),
                    provider: "test".to_string(),
                    model: "test".to_string(),
                    request_json: "r".repeat(300),
                    response_json: "s".repeat(300),
                    tokens_saved: 0,
                    created_at: 1,
                })
                .await?;
        }
        CachingAiProvider::prune_payload(&cache.conn, 600).await?;
        let mut rows = cache
            .conn
            .query("SELECT request_hash FROM response_cache", ())
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<String>(0)?, "entry-3");
        assert!(rows.next().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn diagnostic_request_omits_metadata_but_cache_key_keeps_it() -> Result<()> {
        let first = request_with_metadata(json!({"opaque": "first"}));
        let second = request_with_metadata(json!({"opaque": "second"}));
        let diagnostic: serde_json::Value =
            serde_json::from_str(&CachingAiProvider::diagnostic_request_json(&first)?)?;
        assert!(diagnostic["messages"][0].get("provider_metadata").is_none());

        let (_temp, cache) = test_cache().await?;
        assert_ne!(
            cache.compute_cache_key(&first),
            cache.compute_cache_key(&second)
        );
        Ok(())
    }

    #[tokio::test]
    async fn oversized_entry_is_not_stored() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        cache
            .store_entry(CacheEntry {
                hash: "large".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                request_json: "{}".to_string(),
                response_json: "x".repeat(MAX_CACHE_ENTRY_BYTES),
                tokens_saved: 0,
                created_at: 1,
            })
            .await?;

        let mut rows = cache
            .conn
            .query("SELECT COUNT(*) FROM response_cache", ())
            .await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn pruning_reserves_room_for_the_next_entry() -> Result<()> {
        let (_temp, cache) = test_cache().await?;
        for created_at in 1..=3 {
            cache
                .store_entry(CacheEntry {
                    hash: format!("entry-{created_at}"),
                    provider: "test".to_string(),
                    model: "test".to_string(),
                    request_json: "r".repeat(300),
                    response_json: "s".repeat(300),
                    tokens_saved: 0,
                    created_at,
                })
                .await?;
        }

        CachingAiProvider::prune_payload(&cache.conn, 600).await?;
        assert!(cache.payload_bytes().await? <= 600);

        let mut rows = cache
            .conn
            .query(
                "SELECT request_hash FROM response_cache ORDER BY created_at",
                (),
            )
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<String>(0)?, "entry-3");
        assert!(rows.next().await?.is_none());
        Ok(())
    }
}

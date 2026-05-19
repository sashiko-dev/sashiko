use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

use super::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};

pub fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push('.');
        }
        result.push(c);
    }
    result
}

/// Format a token count with abbreviated suffix: 1.2M, 42.1k, or raw number.
pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub struct CachingAiProvider {
    inner: Arc<dyn AiProvider>,
    conn: libsql::Connection,
    session_start: i64,
    hits_this: AtomicU64,
    hits_prev: AtomicU64,
    tokens_saved_this: AtomicU64,
    tokens_saved_prev: AtomicU64,
    misses: AtomicU64,
    tokens_stored: AtomicU64,
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
            );",
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

        Ok(Self {
            inner,
            conn,
            session_start,
            hits_this: AtomicU64::new(0),
            hits_prev: AtomicU64::new(0),
            tokens_saved_this: AtomicU64::new(0),
            tokens_saved_prev: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            tokens_stored: AtomicU64::new(0),
        })
    }

    fn compute_cache_key(request: &AiRequest) -> String {
        let mut val = serde_json::to_value(request).unwrap_or_default();
        // Strip nondeterministic fields
        if let serde_json::Value::Object(ref mut map) = val {
            map.remove("context_tag");
        }
        super::scrub_thought_signatures(&mut val);
        let canonical = serde_json::to_string(&val).unwrap_or_default();
        let hash = Sha256::digest(canonical.as_bytes());
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[async_trait]
impl AiProvider for CachingAiProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let hash = Self::compute_cache_key(&request);
        let hash_prefix = &hash[..12];

        let mut rows = self
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
            if let Ok(mut resp) = serde_json::from_str::<AiResponse>(&response_json) {
                // Evict poisoned entries: empty responses that were cached before
                // this guard existed.  Without eviction they replay on every retry,
                // turning a transient AI failure into a permanent one.
                let has_content = resp.content.as_ref().is_some_and(|c| !c.trim().is_empty());
                let has_tool_calls = resp.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
                if !has_content && !has_tool_calls {
                    debug!(
                        "Evicting poisoned cache entry [{}]: no content or tool calls",
                        hash_prefix
                    );
                    let _ = self
                        .conn
                        .execute(
                            "DELETE FROM response_cache WHERE request_hash = ?",
                            libsql::params![hash.clone()],
                        )
                        .await;
                    // Fall through to cache miss path below
                } else {
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
                        hash_prefix,
                        origin,
                        fmt_thousands(tokens_saved as u64),
                        origin,
                        fmt_thousands(total)
                    );
                    if let Some(ref mut usage) = resp.usage {
                        usage.cached_tokens =
                            Some(usage.cached_tokens.unwrap_or(0) + usage.prompt_tokens);
                    }
                    resp.cache_key = Some(hash.clone());
                    return Ok(resp);
                }
            }
        }

        debug!("Cache miss [{}]", hash_prefix);
        self.misses.fetch_add(1, Ordering::Relaxed);

        let mut resp = self.inner.generate_content(request.clone()).await?;

        // Never cache empty responses — they poison retries (same hash →
        // same empty result on every attempt, turning a transient failure
        // into a permanent one).
        let has_content = resp.content.as_ref().is_some_and(|c| !c.trim().is_empty());
        let has_tool_calls = resp.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        if !has_content && !has_tool_calls {
            debug!(
                "Skipping cache store [{}]: response has no content or tool calls",
                hash_prefix
            );
            return Ok(resp);
        }

        let response_json = serde_json::to_string(&resp)?;
        let request_json = serde_json::to_string(&request)?;
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

        let _ = self
            .conn
            .execute(
                "INSERT OR REPLACE INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    hash.clone(),
                    caps.model_name.clone(),
                    caps.model_name,
                    request_json,
                    response_json,
                    tokens_saved as i64,
                    now
                ],
            )
            .await;

        self.tokens_stored
            .fetch_add(tokens_saved as u64, Ordering::Relaxed);

        resp.cache_key = Some(hash);
        Ok(resp)
    }

    async fn invalidate_cache_entry(&self, key: &str) {
        let prefix = &key[..key.len().min(12)];
        info!("Invalidating poisoned cache entry [{}]", prefix);
        let _ = self
            .conn
            .execute(
                "DELETE FROM response_cache WHERE request_hash = ?",
                libsql::params![key],
            )
            .await;
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        Some(CacheStats {
            hits_this_session: self.hits_this.load(Ordering::Relaxed),
            hits_prev_session: self.hits_prev.load(Ordering::Relaxed),
            tokens_saved_this_session: self.tokens_saved_this.load(Ordering::Relaxed),
            tokens_saved_prev_session: self.tokens_saved_prev.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            tokens_stored: self.tokens_stored.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_db() -> libsql::Connection {
        let db = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(
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
        .await
        .unwrap();
        conn
    }

    #[tokio::test]
    async fn test_poisoned_empty_response_evicted_on_read() {
        let conn = setup_test_db().await;
        let empty_resp = serde_json::json!({
            "content": null,
            "tool_calls": null,
            "usage": {"prompt_tokens": 100, "completion_tokens": 0, "total_tokens": 100},
            "truncated": false
        });
        conn.execute(
            "INSERT INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES ('poisoned', 'test', 'test', '{}', ?, 100, 1000)",
            libsql::params![empty_resp.to_string()],
        ).await.unwrap();

        let resp: AiResponse = serde_json::from_str(&empty_resp.to_string()).unwrap();
        let has_content = resp.content.as_ref().is_some_and(|c| !c.trim().is_empty());
        let has_tool_calls = resp.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        assert!(
            !has_content && !has_tool_calls,
            "empty response should trigger eviction"
        );

        let good_resp = serde_json::json!({
            "content": "{\"concerns\": [], \"dismissed_concerns\": []}",
            "tool_calls": null,
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
            "truncated": false
        });
        let good: AiResponse = serde_json::from_str(&good_resp.to_string()).unwrap();
        let has_content = good.content.as_ref().is_some_and(|c| !c.trim().is_empty());
        assert!(has_content, "non-empty response should pass the guard");
    }

    #[tokio::test]
    async fn test_invalidate_cache_entry() {
        let conn = setup_test_db().await;
        conn.execute(
            "INSERT INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES ('bad_entry', 'test', 'test', '{}', '{}', 100, 1000)",
            libsql::params![],
        ).await.unwrap();

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM response_cache WHERE request_hash = 'bad_entry'",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );

        let _ = conn
            .execute(
                "DELETE FROM response_cache WHERE request_hash = ?",
                libsql::params!["bad_entry"],
            )
            .await;

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM response_cache WHERE request_hash = 'bad_entry'",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            0
        );
    }

    #[test]
    fn test_cache_key_not_serialized() {
        let resp = AiResponse {
            content: Some("test".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
            cache_key: Some("abc123".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("cache_key"),
            "cache_key must not appear in serialized JSON"
        );
        assert!(
            !json.contains("abc123"),
            "cache_key value must not appear in serialized JSON"
        );

        let parsed: AiResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.cache_key.is_none());
    }

    #[test]
    fn test_fmt_tokens() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(500), "500");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(42_100), "42.1k");
        assert_eq!(fmt_tokens(999_999), "1000.0k");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_fmt_thousands() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(1_000), "1.000");
        assert_eq!(fmt_thousands(1_234_567), "1.234.567");
    }
}

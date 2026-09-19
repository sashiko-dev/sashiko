use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use super::{
    AiProvider, AiRequest, AiResponse, CacheStats, MAX_PROVIDER_METADATA_BYTES,
    ProviderCapabilities,
};

const MAX_CACHE_ENTRY_BYTES: usize = MAX_PROVIDER_METADATA_BYTES;
const MAX_CACHE_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

struct CacheEntry {
    hash: String,
    provider: String,
    model: String,
    request_json: String,
    response_json: String,
    tokens_saved: i64,
    created_at: i64,
}

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

pub struct CachingAiProvider {
    inner: Arc<dyn AiProvider>,
    conn: libsql::Connection,
    session_start: i64,
    hits_this: AtomicU64,
    hits_prev: AtomicU64,
    tokens_saved_this: AtomicU64,
    tokens_saved_prev: AtomicU64,
    write_lock: tokio::sync::Mutex<()>,
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
            CREATE INDEX IF NOT EXISTS response_cache_created_at_idx
                ON response_cache(created_at DESC);",
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

        if let Err(error) = async {
            let transaction = conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await?;
            Self::prune_payload(&transaction, MAX_CACHE_PAYLOAD_BYTES).await?;
            transaction.commit().await.map_err(anyhow::Error::from)
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
            write_lock: tokio::sync::Mutex::new(()),
        };

        info!("Response cache enabled ({})", cache_path);
        Ok(cache)
    }

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
        let mut rows = self
            .conn
            .query(
                "SELECT COALESCE(SUM(\
                    length(CAST(request_json AS BLOB)) + \
                    length(CAST(response_json AS BLOB))\
                 ), 0) FROM response_cache",
                (),
            )
            .await?;
        let bytes: i64 = rows
            .next()
            .await?
            .map(|row| row.get(0))
            .transpose()?
            .unwrap_or_default();
        Ok(bytes.max(0) as usize)
    }

    async fn prune_payload(connection: &libsql::Connection, limit: usize) -> Result<()> {
        connection
            .execute(
                "WITH newest_first AS (\
                    SELECT rowid, SUM(\
                        length(CAST(request_json AS BLOB)) + \
                        length(CAST(response_json AS BLOB))\
                    ) OVER (\
                        ORDER BY created_at DESC, rowid DESC \
                        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW\
                    ) AS retained_bytes \
                    FROM response_cache\
                 ) \
                 DELETE FROM response_cache WHERE rowid IN (\
                    SELECT rowid FROM newest_first WHERE retained_bytes > ?\
                 )",
                libsql::params![limit as i64],
            )
            .await?;
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

        let _guard = self.write_lock.lock().await;
        let transaction = self
            .conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    entry.hash,
                    entry.provider,
                    entry.model,
                    entry.request_json,
                    entry.response_json,
                    entry.tokens_saved,
                    entry.created_at
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

        let mut rows = self
            .conn
            .query(
                "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                libsql::params![hash.clone()],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let response_json: String = row.get(0)?;
            if response_json.len() > MAX_CACHE_ENTRY_BYTES {
                debug!(
                    "Cache hit [{hash_prefix}]: oversized legacy entry ({} bytes), skipping",
                    response_json.len()
                );
            } else {
                let tokens_saved: i64 = row.get(1)?;
                let created_at: i64 = row.get(2)?;
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
                        hash_prefix,
                        origin,
                        fmt_thousands(tokens_saved as u64),
                        origin,
                        fmt_thousands(total)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiProviderMetadata, AiRole};
    use serde_json::json;

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

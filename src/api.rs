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

use crate::access::{BugAccess, OptionalPrincipal, Principal, SectionTitle};
use crate::db::Database;
use crate::events::{Event, MessageSource};
use crate::fetcher::FetchRequest;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Redirect},
    routing::{get, get_service, post},
};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info, warn};

use std::time::{Duration, Instant};
use tokio::sync::RwLock;

struct CachedValue<T> {
    value: T,
    timestamp: Instant,
}

struct AsyncCache<T> {
    inner: RwLock<Option<CachedValue<T>>>,
    ttl: Duration,
}

impl<T: Clone> AsyncCache<T> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(None),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, fetch: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        if let Some(cached) = self.inner.read().await.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        *write_guard = Some(CachedValue {
            value: value.clone(),
            timestamp: Instant::now(),
        });
        Ok(value)
    }
}

struct AsyncMapCache<K, V> {
    inner: RwLock<std::collections::HashMap<K, CachedValue<V>>>,
    ttl: Duration,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> AsyncMapCache<K, V> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(std::collections::HashMap::new()),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, key: K, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        if let Some(cached) = self.inner.read().await.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        write_guard.insert(
            key,
            CachedValue {
                value: value.clone(),
                timestamp: Instant::now(),
            },
        );
        Ok(value)
    }
}

/// In-process rate limiter for sign-in link requests.
///
/// Limits:
/// - Per address: 3 requests per 15 minutes, 10 requests per 24 hours (caps mail bombs)
/// - Per source IP: 10 requests per 15 minutes (caps enumeration sweeps)
/// - Global: 100 requests per 1 hour (caps total outbound login mail)
#[derive(Default)]
pub struct SignInLinkRateLimiter {
    state: std::sync::Mutex<SignInLinkRateLimiterState>,
}

#[derive(Default)]
struct SignInLinkRateLimiterState {
    per_address: std::collections::HashMap<String, Vec<Instant>>,
    per_ip: std::collections::HashMap<String, Vec<Instant>>,
    global: Vec<Instant>,
}

impl SignInLinkRateLimiter {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SignInLinkRateLimiterState::default()),
        }
    }

    /// Checks if a request is allowed by all rate-limit buckets.
    /// If allowed, records the attempt in all buckets and returns true.
    /// If any limit is exceeded, returns false without recording an outbound grant.
    pub fn check_and_record(&self, email: &str, client_ip: &str) -> bool {
        let now = Instant::now();
        let fifteen_mins = Duration::from_secs(15 * 60);
        let one_hour = Duration::from_secs(60 * 60);
        let one_day = Duration::from_secs(24 * 60 * 60);

        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // 1. Per client IP: 10 per 15 minutes
        let ip_attempts = guard.per_ip.entry(client_ip.to_string()).or_default();
        ip_attempts.retain(|&t| now.saturating_duration_since(t) < fifteen_mins);
        if ip_attempts.len() >= 10 {
            return false;
        }

        // 2. Per address: 3 per 15 minutes, 10 per day
        let normalized_email = email.trim().to_lowercase();
        if !normalized_email.is_empty() {
            let addr_attempts = guard
                .per_address
                .entry(normalized_email.clone())
                .or_default();
            addr_attempts.retain(|&t| now.saturating_duration_since(t) < one_day);
            let in_15m = addr_attempts
                .iter()
                .filter(|&&t| now.saturating_duration_since(t) < fifteen_mins)
                .count();
            if in_15m >= 3 || addr_attempts.len() >= 10 {
                return false;
            }
        }

        // 3. Global: 100 per hour
        guard
            .global
            .retain(|&t| now.saturating_duration_since(t) < one_hour);
        if guard.global.len() >= 100 {
            return false;
        }

        // Passed all limits; record the attempt
        guard
            .per_ip
            .entry(client_ip.to_string())
            .or_default()
            .push(now);
        if !normalized_email.is_empty() {
            guard
                .per_address
                .entry(normalized_email)
                .or_default()
                .push(now);
        }
        guard.global.push(now);

        true
    }
}

/// What the router needs to know about the process it runs in.
///
/// These are facts about this invocation rather than configuration, which is
/// why they arrive separately from Settings. They are grouped because passing
/// them positionally made three consecutive bools that no call site could be
/// read against.
#[derive(Default, Clone)]
pub struct ServerOptions {
    /// Accepts mutations from any address without authentication. Unsafe, and
    /// only set by an explicit command line flag.
    pub allow_all_submit: bool,
    pub smtp_enabled: bool,
    pub dry_run: bool,
    /// The credential this process published for local tooling to present.
    ///
    /// Absent when the token could not be written, in which case local tools
    /// authenticate the same way anyone else does.
    pub local_token: Option<crate::auth::LocalToken>,
}

pub struct AppState {
    pub settings: Arc<crate::settings::Settings>,
    pub db: Arc<Database>,
    pub sender: mpsc::Sender<Event>,
    pub fetch_sender: mpsc::Sender<FetchRequest>,
    pub forge_registry: Arc<crate::forge::ForgeRegistry>,
    pub read_only: bool,
    pub allow_all_submit: bool,
    pub smtp_enabled: bool,
    pub dry_run: bool,
    pub local_token: Option<crate::auth::LocalToken>,
    pub sign_in_link_rate_limiter: SignInLinkRateLimiter,
    stats_timeline_cache: AsyncMapCache<Option<i64>, serde_json::Value>,
    stats_reviews_cache: AsyncCache<serde_json::Value>,
    stats_tools_cache: AsyncCache<serde_json::Value>,
    messages_count_cache: AsyncCache<usize>,
    patchsets_count_cache: AsyncCache<usize>,
    patchsets_homepage_cache: AsyncCache<Vec<crate::db::PatchsetRow>>,
    messages_homepage_cache: AsyncCache<Vec<crate::db::MessageRow>>,
    bug_subsystems_cache: AsyncMapCache<Option<String>, Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub mailing_list: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PatchsetsResponse {
    pub items: Vec<crate::db::PatchsetRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Serialize, Deserialize)]
pub struct MessagesResponse {
    pub items: Vec<crate::db::MessageRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Deserialize)]
pub struct PatchQuery {
    pub id: String,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub id: Option<i64>,
    pub patchset_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct BugQuery {
    pub id: Option<i64>,
    pub bugid: Option<String>,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct BugListQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub subsystem: Option<String>,
    pub subsystems: Option<String>,
    pub min_severity: Option<String>,
    pub severity: Option<String>,
    /// Triage state. Also accepts the historical `status` spelling.
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
    /// Analysis execution state.
    pub pipeline_state: Option<String>,
    /// Filters by assignee. The literal `none` selects unassigned bugs.
    pub assignee: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
}

#[derive(Deserialize)]
pub struct BugSubsystemsQuery {
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
}

#[derive(Deserialize)]
pub struct RerunPatchQuery {
    pub patchset_id: i64,
    pub patch_id: i64,
}

#[derive(Deserialize)]
pub struct SubsystemQuery {
    pub subsystem_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct CancelQuery {
    pub id: i64,
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
pub struct InjectRequest {
    pub raw: String,
    pub group: Option<String>,
    pub baseline: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubmitRequest {
    Inject {
        raw: String,
        base_commit: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Remote {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    #[serde(rename = "remote-range")]
    RemoteRange {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Thread {
        msgid: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct SubmitResponse {
    pub status: String,
    pub id: String,
}

async fn redirect_www(req: Request, next: Next) -> impl IntoResponse {
    if let Some(host) = req.headers().get("host").and_then(|h| h.to_str().ok()) {
        let host_without_port = host.split(':').next().unwrap_or("");
        if host_without_port == "www.sashiko.dev" {
            let uri = req.uri();
            let new_uri = format!(
                "https://sashiko.dev{}{}",
                uri.path(),
                uri.query().map(|q| format!("?{}", q)).unwrap_or_default()
            );
            return Redirect::permanent(&new_uri).into_response();
        }
    }
    next.run(req).await
}

/// Build the API router with all routes and shared state.
///
/// Extracted from [`run_server`] so that integration tests can construct the
/// router independently (e.g. bind to port 0 for random-port testing).
pub fn build_router(
    settings: Arc<crate::settings::Settings>,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
    options: ServerOptions,
) -> Router {
    let forge_registry = Arc::new(crate::forge::ForgeRegistry::new());
    let read_only = settings.server.read_only;

    let state = Arc::new(AppState {
        settings: settings.clone(),
        db,
        sender,
        fetch_sender,
        read_only,
        forge_registry,
        allow_all_submit: options.allow_all_submit,
        smtp_enabled: options.smtp_enabled,
        dry_run: options.dry_run,
        local_token: options.local_token,
        sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
        stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
        stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
        stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
        messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
        messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
        bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
    });

    Router::new()
        .route("/health", get(health_check))
        .route("/api/config", get(get_config))
        .route("/api/lists", get(list_mailing_lists))
        .route("/api/patchsets", get(list_patchsets))
        .route("/api/messages", get(list_messages))
        .route("/api/patch", get(get_patchset))
        .route("/api/patchset", get(get_patchset_summary))
        .route("/api/message", get(get_message))
        .route("/api/review", get(get_review))
        .route("/api/review_log", get(get_review_log))
        .route("/api/stats", get(get_stats))
        .route("/api/stats/timeline", get(stats_timeline))
        .route("/api/stats/reviews", get(stats_reviews))
        .route("/api/stats/tools", get(stats_tools))
        .route("/api/submit", post(submit_patch))
        .route("/api/auth/request-link", post(request_link))
        .route("/api/auth/verify", get(verify_link))
        .route("/api/auth/refresh", post(refresh_token))
        .route("/api/patchset/rerun", post(rerun_patchset))
        .route("/api/patchset/cancel", post(cancel_patchset))
        .route("/api/patch/rerun", post(rerun_patch))
        .route("/api/bug", get(get_bug))
        .route("/api/bugs", get(list_bugs))
        .route("/api/bugs/subsystems", get(list_bug_subsystems))
        .route("/api/subsystems", get(list_bug_subsystems))
        .route("/api/bug/logs", get(get_bug_logs))
        .route("/api/bug/raw", get(get_bug_raw))
        .route("/api/bug/input", get(get_bug_input))
        .route("/api/bug/enrichments", get(get_bug_enrichments))
        .route("/api/bug/analyze", post(analyze_bug))
        .route("/api/bug/action", post(bug_action))
        .route("/bug/{bugid}", get(redirect_bug))
        .route("/api/webhook/{provider}", post(forge_webhook))
        .route("/", get_service(ServeFile::new("static/index.html")))
        .route(
            "/auth/verify",
            get_service(ServeFile::new("static/index.html")),
        )
        .nest_service("/static", ServeDir::new("static"))
        .layer(middleware::from_fn(redirect_www))
        .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024))
        .with_state(state)
}

pub async fn run_server(
    settings: Arc<crate::settings::Settings>,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
    options: ServerOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router(settings.clone(), db, sender, fetch_sender, options);

    let bind_addr = format!("{}:{}", settings.server.host, settings.server.port);
    let addrs: Vec<SocketAddr> = bind_addr
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid bind address '{}': {}", bind_addr, e))?
        .collect();
    info!("Web API listening on {:?}", addrs);

    let listener = TcpListener::bind(addrs.as_slice()).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn generate_synthetic_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    // e.g. sashiko-local-1715890000-12345
    format!(
        "sashiko-{}-{}-{}@sashiko.local",
        prefix,
        since_the_epoch.as_secs(),
        fastrand::u32(..)
    )
}

async fn submit_patch(
    auth: crate::auth::OptionalAuthUser,
    // Logged on a refusal to give an operator something to grep for. It is
    // deliberately not passed to is_authorized: where a request comes from
    // decides nothing.
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Ingest,
    ) {
        info!("Refused unauthorized patch submission from {}", addr);
        return Err(StatusCode::FORBIDDEN);
    }

    match payload {
        SubmitRequest::Inject {
            raw,
            base_commit,
            skip_subjects,
            only_subjects,
        } => {
            if raw.trim().is_empty() {
                return Err(StatusCode::BAD_REQUEST);
            }
            // Basic guardrail
            if !raw.contains("From ") && !raw.contains("Subject:") {
                return Err(StatusCode::BAD_REQUEST);
            }

            let id = generate_synthetic_id("inject");
            info!("Received raw mbox injection: {} (len: {})", id, raw.len());

            let submitted_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .ok();

            let event = Event::RawMboxSubmitted {
                raw,
                submission_id: id.clone(),
                source: MessageSource::ApiInject,
                group: "api-submit".to_string(),
                baseline: base_commit,
                skip_subjects,
                only_subjects,
                submitted_at,
            };

            if let Err(e) = state.sender.send(event).await {
                error!("Failed to send raw mbox to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Remote {
            sha,
            repo,
            skip_subjects,
            only_subjects,
        }
        | SubmitRequest::RemoteRange {
            sha,
            repo,
            skip_subjects,
            only_subjects,
        } => {
            let id = sha.clone();
            let repo_display = repo.as_deref().unwrap_or("local");
            info!(
                "Received remote fetch request: {} from {}",
                sha, repo_display
            );

            // Optimistic check: If we already have this patchset in the DB,
            // skip creating placeholder and skip fetch queue entirely.
            match state.db.has_patchset_by_msgid(&id).await {
                Ok(true) => {
                    info!(
                        "Remote fetch request for already ingested SHA {}, skipping placeholder and fetch",
                        id
                    );
                    return Ok(Json(SubmitResponse {
                        status: "accepted".to_string(),
                        id,
                    }));
                }
                Err(e) => {
                    error!("Failed to check if patchset exists: {}", e);
                }
                _ => {}
            }

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &format!("{}@sashiko.local", id),
                    &format!("Fetching {} from {}...", sha, repo_display),
                    skip_subjects.as_ref(),
                    only_subjects.as_ref(),
                    None,
                    None,
                    None,
                    None,
                )
                .await
            {
                error!("Failed to create placeholder patchset: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let req = FetchRequest {
                repo_url: repo,
                commit_hash: sha,
                mr_url: None,
                mr_title: None,
                mr_number: None,
            };

            if let Err(e) = state.fetch_sender.send(req).await {
                error!("Failed to send fetch request to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Thread { msgid } => {
            let id = generate_synthetic_id("thread");
            // Percent-encode path-significant characters in the message-ID
            // for safe inclusion in the lore.kernel.org fetch URL. This
            // handles RFC 5322 message-IDs that contain `/` or other
            // path-sensitive characters without rejecting them.
            const PATH_SEGMENT_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
                .add(b'/')
                .add(b'\\')
                .add(b'?')
                .add(b'#')
                .add(b' ')
                .add(b'%');
            let clean_msgid = percent_encoding::utf8_percent_encode(
                msgid.trim_matches(|c| c == '<' || c == '>'),
                PATH_SEGMENT_ENCODE,
            )
            .to_string();
            info!(
                "Received thread fetch request: {} (msgid: {})",
                id, clean_msgid
            );

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &clean_msgid,
                    &format!("Fetching thread {}...", clean_msgid),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
            {
                tracing::error!("Failed to create placeholder patchset: {}", e);
                // Non-fatal, just continue
            }

            let msgid_clone = clean_msgid.clone();
            let sender = state.sender.clone();

            tokio::spawn(async move {
                if let Err(e) = fetch_and_inject_thread(&msgid_clone, sender.clone()).await {
                    tracing::error!("Failed to fetch thread {}: {}", msgid_clone, e);
                    let _ = sender
                        .send(Event::IngestionFailed {
                            article_id: msgid_clone.clone(),
                            error: format!("Failed to fetch thread: {}", e),
                            source: MessageSource::ApiFetchThread,
                        })
                        .await;
                }
            });

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id, // The client might expect this ID
            }))
        }
    }
}

async fn fetch_and_inject_thread(
    msgid: &str,
    sender: tokio::sync::mpsc::Sender<Event>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("https://lore.kernel.org/all/{}/t.mbox.gz", msgid);
    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to fetch thread {}: HTTP {}",
            msgid,
            response.status()
        )
        .into());
    }

    const MAX_MBOX_DOWNLOAD: usize = 10 * 1024 * 1024;
    const MAX_MBOX_DECOMPRESSED: u64 = 50 * 1024 * 1024;

    let bytes = response.bytes().await?;
    if bytes.len() > MAX_MBOX_DOWNLOAD {
        return Err(format!(
            "Mbox download {} bytes exceeds {} byte limit",
            bytes.len(),
            MAX_MBOX_DOWNLOAD
        )
        .into());
    }

    // Decompress into bytes first, then convert to UTF-8. Reading directly
    // into a String via read_to_string would produce an InvalidData error
    // if the byte limit splits a multi-byte UTF-8 character.
    let raw = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
        use std::io::Read;
        let decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut limited = decoder.take(MAX_MBOX_DECOMPRESSED);
        let mut raw_bytes = Vec::new();
        limited.read_to_end(&mut raw_bytes)?;

        // If we read exactly the limit, check whether the stream had more
        // data. This distinguishes a file that is exactly 50 MiB (accept)
        // from one that was truncated at 50 MiB (reject).
        if raw_bytes.len() as u64 == MAX_MBOX_DECOMPRESSED {
            let mut probe = [0u8; 1];
            if limited.into_inner().read(&mut probe).unwrap_or(0) > 0 {
                return Err(std::io::Error::other(format!(
                    "Decompressed mbox exceeds {} byte limit",
                    MAX_MBOX_DECOMPRESSED
                )));
            }
        }
        String::from_utf8(raw_bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })
    .await??;

    let submitted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .ok();

    let event = Event::RawMboxSubmitted {
        raw,
        submission_id: msgid.to_string(),
        source: MessageSource::ApiFetchThread,
        group: "api-submit".to_string(),
        baseline: None,
        skip_subjects: None,
        only_subjects: None,
        submitted_at,
    };

    sender.send(event).await?;
    Ok(())
}

async fn list_mailing_lists(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let lists = state.db.get_mailing_lists().await.map_err(|e| {
        error!("Failed to get mailing lists: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let result = lists
        .into_iter()
        .map(|(name, group)| {
            serde_json::json!({
                "name": name,
                "group": group
            })
        })
        .collect();

    Ok(Json(result))
}

async fn list_patchsets(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PatchsetsResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .patchsets_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_patchsets(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_patchsets(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .patchsets_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_patchsets(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_patchsets(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(PatchsetsResponse {
        items,
        total,
        page,
        per_page,
    }))
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<MessagesResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .messages_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_messages(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_messages(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .messages_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_messages(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_messages(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(MessagesResponse {
        items,
        total,
        page,
        per_page,
    }))
}

async fn get_patchset(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for patchset id: {}", id_val);
        state
            .db
            .get_patchset_details(id_val, query.page, query.per_page)
            .await
    } else if query.id.contains('-') && !query.id.contains('@') {
        info!("Fetching details for patchset slug: {}", query.id);
        state
            .db
            .get_patchset_details_by_slug(&query.id, query.page, query.per_page)
            .await
    } else {
        info!("Fetching details for patchset msgid: {}", query.id);
        state
            .db
            .get_patchset_details_by_msgid(&query.id, query.page, query.per_page)
            .await
    };

    match result {
        Ok(Some(mut details)) => {
            if let Some(obj) = details.as_object_mut() {
                obj.insert(
                    "smtp_enabled".to_string(),
                    serde_json::Value::Bool(state.smtp_enabled),
                );
                obj.insert(
                    "dry_run".to_string(),
                    serde_json::Value::Bool(state.dry_run),
                );
            }
            redact_embedded_bugs(&state, &principal, &mut details).await?;
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Patchset not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_review(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Some(ps_id) = query.patchset_id {
        info!("Fetching latest review for patchset id: {}", ps_id);
        state.db.get_latest_review_for_patchset(ps_id).await
    } else if let Some(id) = query.id {
        info!("Fetching details for review id: {}", id);
        state.db.get_review_details(id).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    match result {
        Ok(Some(mut details)) => {
            redact_embedded_bugs(&state, &principal, &mut details).await?;
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Review not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_patchset_summary(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching summary for patchset id: {}", id_val);
        state
            .db
            .get_patchset_summary(id_val, query.page, query.per_page)
            .await
    } else if query.id.contains('-') && !query.id.contains('@') {
        info!("Fetching summary for patchset slug: {}", query.id);
        state
            .db
            .get_patchset_summary_by_slug(&query.id, query.page, query.per_page)
            .await
    } else {
        info!("Fetching summary for patchset msgid: {}", query.id);
        state
            .db
            .get_patchset_summary_by_msgid(&query.id, query.page, query.per_page)
            .await
    };

    match result {
        Ok(Some(mut details)) => {
            if let Some(obj) = details.as_object_mut() {
                obj.insert(
                    "smtp_enabled".to_string(),
                    serde_json::Value::Bool(state.smtp_enabled),
                );
                obj.insert(
                    "dry_run".to_string(),
                    serde_json::Value::Bool(state.dry_run),
                );
            }
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Patchset not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_review_log(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Some(ps_id) = query.patchset_id {
        info!("Fetching latest review log for patchset id: {}", ps_id);
        state.db.get_latest_review_for_patchset(ps_id).await
    } else if let Some(id) = query.id {
        info!("Fetching details for review id: {}", id);
        state.db.get_review_details(id).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    match result {
        Ok(Some(mut details)) => {
            redact_embedded_bugs(&state, &principal, &mut details).await?;
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Review not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Resolves what the caller may do to one bug, from the MAINTAINERS sections
/// attributed to it. Subsystem names derived from a path prefix or supplied by
/// the reporter name nobody and are excluded by the accessor.
async fn bug_access(
    state: &AppState,
    principal: &Principal,
    bug_id: i64,
) -> Result<BugAccess, StatusCode> {
    let sections = state
        .db
        .authorizing_sections_for_bug(bug_id)
        .await
        .map_err(|e| {
            tracing::error!("Database error resolving bug authority: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let titles: Vec<SectionTitle> = sections.iter().map(|s| SectionTitle::new(s)).collect();
    Ok(principal.access_to(&titles))
}

/// The subset of the given bugs the caller may read, resolved in one query.
async fn readable_bug_ids(
    state: &AppState,
    principal: &Principal,
    bug_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, StatusCode> {
    let sections = state
        .db
        .authorizing_sections_for_bugs(bug_ids)
        .await
        .map_err(|e| {
            tracing::error!("Database error resolving bug authority: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(bug_ids
        .iter()
        .copied()
        .filter(|id| {
            let titles: Vec<SectionTitle> = sections
                .get(id)
                .map(|names| names.iter().map(|s| SectionTitle::new(s)).collect())
                .unwrap_or_default();
            principal.access_to(&titles).can_read()
        })
        .collect())
}

/// Loads the bug the query names and confirms the caller may read it.
///
/// A bug outside the caller's authority answers 404, byte for byte the same as
/// a bug that does not exist, so the response does not disclose which of the
/// two it was.
async fn readable_bug(
    state: &AppState,
    principal: &Principal,
    query: &BugQuery,
) -> Result<crate::db::Bug, StatusCode> {
    let bug = if let Some(id) = query.id {
        state.db.get_bug(id).await
    } else if let Some(bugid) = query.bugid.as_ref().or(query.slug.as_ref()) {
        state.db.get_bug_by_bugid(bugid).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    }
    .map_err(|e| {
        tracing::error!("Database error fetching bug: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    if !bug_access(state, principal, bug.id).await?.can_read() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(bug)
}

/// Loads the bug for one of the raw transcript endpoints.
///
/// Transcripts are narrower than the bug itself. The deduplication stage
/// compares a bug against every other bug in the database and the prompt
/// embeds each candidate's problem statement, so a transcript discloses
/// unrelated bugs by construction. Only a principal who can already read every
/// bug learns nothing new from one; for a subsystem-scoped maintainer it would
/// cross exactly the boundary the rest of this model draws.
///
/// The refusal is 403 rather than 404 because the caller reached this point by
/// passing the read check, so the bug's existence is already known to them.
async fn transcript_bug(
    state: &AppState,
    principal: &Principal,
    query: &BugQuery,
) -> Result<crate::db::Bug, StatusCode> {
    let bug = readable_bug(state, principal, query).await?;
    if !principal.has_global_bug_visibility() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(bug)
}

/// Removes embedded bug summaries the caller may not read.
///
/// The patchset and review payloads carry each bug's problem statement,
/// severity and inline review. That is bug data and answers to the same
/// authority as `/api/bug`, so it is filtered here rather than being left to
/// the single-bug endpoints to protect.
async fn redact_embedded_bugs(
    state: &AppState,
    principal: &Principal,
    payload: &mut serde_json::Value,
) -> Result<(), StatusCode> {
    let Some(bugs) = payload.get("bugs").and_then(|b| b.as_array()) else {
        return Ok(());
    };
    if bugs.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = bugs.iter().filter_map(|b| b["id"].as_i64()).collect();
    let readable = readable_bug_ids(state, principal, &ids).await?;
    if let Some(array) = payload.get_mut("bugs").and_then(|b| b.as_array_mut()) {
        array.retain(|b| b["id"].as_i64().is_some_and(|id| readable.contains(&id)));
    }
    Ok(())
}

async fn get_bug(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = readable_bug(&state, &principal, &query).await?;
    let mut val = serde_json::to_value(&bug).unwrap_or(serde_json::json!({}));
    val["slug"] = serde_json::Value::String(bug.bugid.clone());
    val["problem"] = serde_json::Value::String(bug.problem().to_string());
    val["severity"] = serde_json::to_value(bug.severity()).unwrap();
    val["severity_explanation"] = serde_json::to_value(bug.severity_explanation()).unwrap();
    val["description"] = serde_json::to_value(bug.description()).unwrap();
    val["inline_review"] = serde_json::Value::String(bug.inline_review());
    val["locations"] = serde_json::to_value(bug.locations()).unwrap();
    val["source_files"] = serde_json::to_value(bug.source_files()).unwrap();
    val["introduced_in_commit"] = serde_json::to_value(bug.introduced_in_commit()).unwrap();
    val["verified_on_sha"] = serde_json::to_value(bug.verified_on_sha()).unwrap();
    val["is_fixed"] = serde_json::Value::Bool(bug.is_fixed());
    val["fixed_in_commit"] = serde_json::to_value(bug.fixed_in_commit()).unwrap();
    val["raw_input"] = serde_json::to_value(bug.raw_input()).unwrap();
    val["tokens_in"] = serde_json::Value::Number(bug.tokens_in().into());
    val["tokens_out"] = serde_json::Value::Number(bug.tokens_out().into());
    val["tokens_cached"] = serde_json::Value::Number(bug.tokens_cached().into());

    attach_duplicate_relations(&state, &principal, &bug, &mut val).await?;
    let access = bug_access(&state, &principal, bug.id).await?;
    val["can_comment"] = serde_json::Value::Bool(!state.read_only && access.can_comment());
    val["can_manage"] = serde_json::Value::Bool(!state.read_only && access.can_manage());
    let mut family = state
        .db
        .bug_family(bug.id, false)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let family_ids: Vec<i64> = family.iter().map(|member| member.id).collect();
    let readable = readable_bug_ids(&state, &principal, &family_ids).await?;
    family.retain(|member| readable.contains(&member.id));
    val["evidence"] = state
        .db
        .bug_evidence(&family)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(obj) = val.as_object_mut() {
        for key in [
            "raw_input",
            "vector_json",
            "enrichments",
            "tokens_in",
            "tokens_out",
            "tokens_cached",
        ] {
            obj.remove(key);
        }
    }
    Ok(Json(val))
}

/// Attaches the bug's duplicate relations, omitting any counterpart the caller
/// may not read. A pointer to an invisible bug would disclose both that it
/// exists and what it is about.
async fn attach_duplicate_relations(
    state: &AppState,
    principal: &Principal,
    bug: &crate::db::Bug,
    val: &mut serde_json::Value,
) -> Result<(), StatusCode> {
    if bug.lifecycle_status == crate::db::BugLifecycleStatus::Duplicate {
        let Some(canonical_id) = bug.duplicate_of_id else {
            return Ok(());
        };
        if !bug_access(state, principal, canonical_id).await?.can_read() {
            if let Some(obj) = val.as_object_mut() {
                obj.remove("duplicate_of_id");
            }
            return Ok(());
        }
        if let Some(canonical) = state.db.get_bug(canonical_id).await.ok().flatten() {
            val["duplicate_of"] = serde_json::json!({
                "id": canonical.id,
                "bugid": canonical.bugid,
                "problem": canonical.problem(),
            });
        }
        return Ok(());
    }

    let Ok(duplicates) = state.db.list_duplicates_for_bug(bug.id).await else {
        return Ok(());
    };
    let ids: Vec<i64> = duplicates.iter().map(|d| d.id).collect();
    let readable = readable_bug_ids(state, principal, &ids).await?;
    let summaries: Vec<serde_json::Value> = duplicates
        .iter()
        .filter(|d| readable.contains(&d.id))
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "bugid": d.bugid,
                "problem": d.problem(),
                "created_at": d.created_at,
            })
        })
        .collect();
    val["duplicates"] = serde_json::Value::Array(summaries);
    Ok(())
}

async fn get_bug_raw(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;
    let records = state
        .db
        .bug_family(bug.id, true)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({ "bugid": bug.bugid, "records": records }),
    ))
}

/// Serves the payload the bug workflow was started with for a single bug.
/// Duplicates keep their own candidate records, so each one resolves to the
/// input that produced it rather than the canonical bug's input.
async fn get_bug_input(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;

    let inputs: Vec<serde_json::Value> = bug
        .enrichments
        .iter()
        .filter(|e| e.kind == "candidate" || e.kind == "raw_candidate")
        .map(|e| {
            serde_json::json!({
                "enrichment_id": e.id,
                "created_at": e.created_at,
                "author": e.author,
                "tool": e.tool,
                "model": e.model,
                "input": e.data_json,
                "content": e.content,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "bugid": bug.bugid,
        "title": bug.title,
        "inputs": inputs,
    })))
}

async fn get_bug_enrichments(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = readable_bug(&state, &principal, &query).await?;
    Ok(Json(
        serde_json::to_value(&bug.enrichments).unwrap_or_default(),
    ))
}

async fn get_bug_logs(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;
    let result = state.db.get_bug_logs(bug.id).await;

    match result {
        Ok(Some(logs_str)) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&logs_str).unwrap_or(serde_json::Value::String(logs_str));
            Ok(Json(parsed))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("Database error fetching bug logs: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(serde::Deserialize)]
struct AnalyzeBugPayload {
    #[serde(flatten)]
    input: crate::workflows::linux_bug::BugInput,
    tool: Option<String>,
    model: Option<String>,
}

async fn analyze_bug(
    principal: Principal,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AnalyzeBugPayload>,
) -> Result<Json<crate::workflows::linux_bug::BugOutcome>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".to_string(),
        ));
    }

    // Filing a bug runs an LLM analysis, so it costs money and is granted
    // explicitly rather than falling out of any read or review capability.
    if !principal.may_create() {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to file bugs.".to_string(),
        ));
    }

    // Uncached, as this asked for before the cached constructor grew to take the
    // AI settings and a database path: filing a bug is a one-off analysis.
    let provider = match crate::ai::create_provider(&state.settings) {
        Ok(p) => p,
        Err(e) => {
            // The reason names the provider and its configuration, so it stays
            // in the log rather than going back over the wire.
            tracing::error!("Failed to create AI provider: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Analysis is unavailable.".to_string(),
            ));
        }
    };

    let repo_path = std::path::PathBuf::from(&state.settings.git.repository_path);
    let tools = if repo_path.exists() {
        let mainline_sha = match crate::git_ops::get_commit_hash(&repo_path, "origin/master").await
        {
            Ok(sha) => Some(sha),
            Err(_) => match crate::git_ops::get_commit_hash(&repo_path, "master").await {
                Ok(sha) => Some(sha),
                Err(_) => crate::git_ops::get_commit_hash(&repo_path, "HEAD")
                    .await
                    .ok(),
            },
        };
        let mut tb = crate::toolbox::ToolBox::new(repo_path, None);
        if let Some(m_sha) = mainline_sha {
            tb.set_virtual_head(m_sha);
        }
        Some(std::sync::Arc::new(tb))
    } else {
        None
    };

    let source_tool = payload
        .tool
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "api".into());
    let source_model = payload
        .model
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(provider.get_capabilities().model_name));
    let mut payload = payload.input;
    if payload.subsystems.is_empty() && !payload.source_files.is_empty() {
        let matched = if let Some(mindex) = crate::maintainers::get_global_maintainers() {
            mindex.match_files(&payload.source_files)
        } else if let Ok(mindex) = crate::maintainers::MaintainersIndex::from_top_of_trunk(
            &state.settings.git.repository_path,
        ) {
            mindex.match_files(&payload.source_files)
        } else {
            Vec::new()
        };
        payload.subsystems = matched
            .into_iter()
            .map(crate::db::AttributedSubsystem::from_maintainers)
            .collect();
    }

    let actor = if principal.email().is_empty() {
        format!("authorized client ({})", addr.ip())
    } else {
        principal.email().to_string()
    };
    let attributed_db = state.db.with_bug_actor(&actor, &source_tool, source_model);
    match crate::workflows::linux_bug::process_issue(
        provider.as_ref(),
        tools,
        &attributed_db,
        payload,
        Some("api_analyze"),
    )
    .await
    {
        Ok(outcome) => Ok(Json(outcome)),
        Err(e) => {
            // The failure quotes paths, prompts and provider responses, none of
            // which belong in a response body.
            tracing::error!("Pre-existing bug analysis failed: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Analysis failed.".to_string(),
            ))
        }
    }
}

async fn redirect_bug(Path(bugid): Path<String>) -> impl IntoResponse {
    Redirect::temporary(&format!("/#/bug/{}", bugid))
}

async fn get_message(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<crate::db::MessageRow>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for message id: {}", id_val);
        state.db.get_message_details(id_val).await
    } else {
        info!("Fetching details for message msgid: {}", query.id);
        state.db.get_message_details_by_msgid(&query.id).await
    };

    match result {
        Ok(Some(mut details)) => {
            if (details.body.is_none() || details.body.as_deref() == Some(""))
                && let (Some(hash), Some(group)) = (&details.git_blob_hash, &details.mailing_list)
            {
                let repo_root = std::path::PathBuf::from("archives").join(group);

                // 1. Find all potential repo paths (root + epochs)
                let mut candidate_paths = Vec::new();

                // Check epochs first (most likely for recent messages)
                if let Ok(mut entries) = tokio::fs::read_dir(&repo_root).await {
                    let mut epochs = Vec::new();
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if let Ok(ft) = entry.file_type().await
                            && ft.is_dir()
                            && let Ok(name) = entry.file_name().into_string()
                            && let Ok(num) = name.parse::<i32>()
                        {
                            epochs.push(num);
                        }
                    }
                    epochs.sort_by(|a, b| b.cmp(a)); // Descending

                    for epoch in epochs {
                        candidate_paths.push(repo_root.join(epoch.to_string()));
                    }
                }

                // Add root as fallback
                candidate_paths.push(repo_root.clone());

                // 2. Search for blob
                for path in candidate_paths {
                    if let Ok(raw) = crate::git_ops::read_blob(&path, hash).await
                        && let Ok((metadata, _)) = crate::patch::parse_email(&raw)
                    {
                        details.body = Some(metadata.body);
                        break;
                    }
                }
            }
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Message not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_stats(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pending = crate::metrics::get_pending_patches();
    let reviewing = crate::metrics::get_reviewing_patches();
    let messages = crate::metrics::get_messages();
    let patchsets = crate::metrics::get_patchsets();
    let repo_packs = crate::metrics::get_repo_packs();
    let repo_pack_bytes = crate::metrics::get_repo_pack_bytes();

    Ok(Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "pending": pending,
        "reviewing": reviewing,
        "messages": messages,
        "patchsets": patchsets,
        "repo_packs": repo_packs,
        "repo_pack_bytes": repo_pack_bytes
    })))
}

async fn stats_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SubsystemQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_timeline_cache
        .get_or_fetch(params.subsystem_id, || async {
            state
                .db
                .get_timeline_stats(params.subsystem_id)
                .await
                .map_err(|e| {
                    info!("Error getting timeline stats: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_reviews(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_reviews_cache
        .get_or_fetch(|| async {
            state.db.get_review_stats().await.map_err(|e| {
                info!("Error getting review stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_tools(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_tools_cache
        .get_or_fetch(|| async {
            state.db.get_tool_usage_stats().await.map_err(|e| {
                info!("Error getting tool stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn rerun_patchset(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Review,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to rerun patchsets.".into(),
        ));
    }

    let id = query
        .id
        .parse::<i64>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid id parameter".into()))?;

    state.db.rerun_patchset(id).await.map_err(|e| {
        error!("Failed to rerun patchset {}: {}", id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to rerun patchset".into(),
        )
    })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

async fn cancel_patchset(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Cancel,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to cancel patchsets.".into(),
        ));
    }

    let cancelled = state
        .db
        .cancel_patchset(query.id, query.force)
        .await
        .map_err(|e| {
            error!("Failed to cancel patchset {}: {}", query.id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to cancel patchset".into(),
            )
        })?;

    if cancelled {
        info!("Patchset {} cancelled (force={})", query.id, query.force);
        Ok(Json(serde_json::json!({ "status": "cancelled" })))
    } else {
        let reason = if query.force {
            "Patchset is not in a cancellable state (must be Pending, Incomplete, or In Review)"
        } else {
            "Patchset is not in a cancellable state (must be Pending or Incomplete; use force=true for In Review)"
        };
        Ok(Json(serde_json::json!({
            "status": "not_modified",
            "reason": reason
        })))
    }
}

async fn rerun_patch(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<RerunPatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Review,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to rerun patches.".into(),
        ));
    }

    state
        .db
        .rerun_patch(query.patchset_id, query.patch_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to rerun patch {} in patchset {}: {}",
                query.patch_id, query.patchset_id, e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to rerun patch".into(),
            )
        })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

async fn health_check() -> StatusCode {
    StatusCode::OK
}

async fn get_config(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let is_auth = |perm| is_authorized(&state, &headers, auth.0.as_ref(), perm);
    let can_review = !state.read_only && is_auth(crate::settings::Permission::Review);
    let can_cancel = !state.read_only && is_auth(crate::settings::Permission::Cancel);
    let can_ingest = !state.read_only && is_auth(crate::settings::Permission::Ingest);

    Ok(Json(serde_json::json!({
        "project_name": state.settings.project.name,
        "project_description": state.settings.project.description,
        "project_domain": state.settings.project.domain,
        "attribution": state.settings.project.attribution(),
        "forge_enabled": state.settings.forge.enabled,
        "read_only": state.read_only,
        "permissions": {
            "review": can_review,
            "cancel": can_cancel,
            "ingest": can_ingest,
        },
        "user": {
            "email": auth.0.as_ref().map(|u| &u.email),
            "is_authenticated": auth.0.is_some(),
        },
        "version": env!("CARGO_PKG_VERSION"),
        "git_hash": env!("GIT_HASH"),
    })))
}

async fn forge_webhook(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    let webhook_secret = state.settings.forge.webhook_secret.as_deref();
    let has_secret = webhook_secret.is_some();

    // Access control: when webhook_secret is configured, signature
    // verification in validate_event is the sole access control for ALL
    // requests. This is critical for reverse proxy deployments where all
    // traffic arrives from loopback — the signature check cannot be
    // bypassed by source IP.
    //
    // Without a secret there is nothing to verify, so the only callers left
    // are ones that can prove they share this machine by presenting the local
    // token, plus the explicit --enable-unsafe-all-submit flag. A forge cannot
    // do either, which is the point: an endpoint a forge can reach
    // unauthenticated is an endpoint anyone can reach, and arriving on
    // loopback never distinguished the two behind a reverse proxy.
    if !has_secret && !presents_local_token(&headers, &state) && !state.allow_all_submit {
        info!(
            "Refused {} webhook from {}: configure webhook_secret or use --enable-unsafe-all-submit",
            crate::forge::loggable(&provider),
            addr
        );
        return Err(StatusCode::FORBIDDEN);
    }

    let forge = state.forge_registry.get(&provider).ok_or_else(|| {
        warn!(
            "Unknown forge provider: {}",
            crate::forge::loggable(&provider)
        );
        StatusCode::NOT_FOUND
    })?;

    // A rejected delivery is reported by the forge as a bare status code, so
    // the log line is the only place an administrator can learn which event
    // was refused and why.
    let event = forge
        .validate_event(&headers, &body, webhook_secret)
        .inspect_err(|status| {
            warn!(
                "Rejected {} webhook event {} from {}: {}",
                forge.name(),
                crate::forge::event_label(&headers),
                addr,
                status
            );
        })?;

    if event == crate::forge::ForgeEvent::Handshake {
        info!(
            "{} webhook handshake from {} acknowledged",
            forge.name(),
            addr
        );
        return Ok(Json(serde_json::json!({
            "status": "ok",
            "message": format!("{} webhook endpoint is configured", forge.name())
        })));
    }

    let (action, metadata) = forge.parse_payload(&body).inspect_err(|status| {
        warn!(
            "Rejected {} webhook payload from {}: {} ({} bytes)",
            forge.name(),
            addr,
            status,
            body.len()
        );
    })?;

    info!(
        "{} {}: {} - {}",
        forge.name(),
        crate::forge::loggable(&action),
        metadata.pr_title.as_deref().unwrap_or("(no title)"),
        metadata.pr_url.as_deref().unwrap_or("(no url)")
    );

    // A forge reports far more than new commits, and the base sha it sends
    // moves with the target branch. Reviewing an action that changed only a
    // label would spend a full review re-reading code that did not change.
    if forge.review_intent(&action) == crate::forge::ReviewIntent::Skip {
        info!(
            "{} {} carries no new commits, skipping review",
            forge.name(),
            crate::forge::loggable(&action)
        );
        return Ok(Json(serde_json::json!({
            "status": "ignored",
            "message": format!("{} action carries no new commits", forge.name())
        })));
    }

    if let Some(ref author) = metadata.author
        && crate::forge::is_dependabot_author(author)
    {
        info!(
            "{} PR #{} by {} is from Dependabot, skipping review",
            forge.name(),
            metadata.pr_number,
            crate::forge::loggable(author)
        );
        return Ok(Json(serde_json::json!({
            "status": "ignored",
            "message": "Dependabot pull requests are not reviewed"
        })));
    }

    let default_subject = format!("{} #{}", forge.name(), metadata.pr_number);
    let subject = metadata.pr_title.as_deref().unwrap_or(&default_subject);

    let commit_range = format!("{}..{}", metadata.base_sha, metadata.head_sha);
    let placeholder_id = format!("mr-{}-{}@sashiko.local", metadata.pr_number, commit_range);

    let slug = metadata.repo_url.as_ref().map(|url| {
        let repo = crate::forge::extract_repo_name_from_url(url);
        format!("{}-{}", repo, metadata.pr_number)
    });

    state
        .db
        .create_fetching_patchset(
            &placeholder_id,
            &format!("Fetching {} PR/MR: {}", forge.name(), subject),
            None,
            None,
            metadata.pr_url.as_deref(),
            Some(subject),
            Some(metadata.pr_number),
            slug.as_deref(),
        )
        .await
        .map_err(|e| {
            error!("Failed to create placeholder patchset: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let req = FetchRequest {
        repo_url: metadata.repo_url,
        commit_hash: commit_range,
        mr_url: metadata.pr_url,
        mr_title: metadata.pr_title,
        mr_number: Some(metadata.pr_number),
    };

    state.fetch_sender.send(req).await.map_err(|e| {
        error!("Failed to send fetch request to queue: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({
        "status": "accepted",
        "message": format!("{} {} queued for review", forge.name(), action)
    })))
}

async fn list_bugs(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugListQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let scope = principal.maintained_section_names();
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 100);

    let min_sev = query
        .min_severity
        .as_deref()
        .or(query.severity.as_deref())
        .map(crate::db::Severity::from_str);

    let parsed_subsystems: Option<Vec<String>> = if let Some(ref subs_str) = query.subsystems {
        let subs: Vec<String> = if subs_str.trim().starts_with('[') {
            serde_json::from_str(subs_str).unwrap_or_else(|_| {
                subs_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        } else {
            subs_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Some(subs)
    } else if let Some(ref sub_str) = query.subsystem
        && sub_str.contains(',')
    {
        let subs: Vec<String> = sub_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Some(subs)
    } else {
        None
    };

    // Unknown filter values are rejected outright: silently returning an empty
    // list would look identical to "no bugs match" and hide the typo.
    let lifecycle_status = match query.lifecycle_status.as_deref().map(str::parse) {
        Some(Ok(status)) => Some(status),
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => None,
    };
    let pipeline_state = match query.pipeline_state.as_deref().map(str::parse) {
        Some(Ok(state)) => Some(state),
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => None,
    };
    let assignee = query
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|who| {
            if who.eq_ignore_ascii_case("none") {
                crate::db::AssigneeFilter::Unassigned
            } else {
                crate::db::AssigneeFilter::Is(who)
            }
        });

    match state
        .db
        .list_bugs(crate::db::ListBugsParams {
            page: Some(page as u32),
            limit: Some(per_page as u32),
            min_severity: min_sev,
            subsystem: if parsed_subsystems.is_none() {
                query.subsystem.as_deref()
            } else {
                None
            },
            subsystems: parsed_subsystems.as_deref(),
            lifecycle_status,
            pipeline_state,
            assignee,
            search: query.q.as_deref(),
            sort_by: query.sort_by.as_deref(),
            sort_order: query.sort_order.as_deref(),
            visibility: principal.visibility(&scope),
        })
        .await
    {
        Ok((items, total)) => {
            let ids: Vec<_> = items.iter().map(|b| b.id).collect();
            let mut summaries = state
                .db
                .bug_discovery_summaries(&ids)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let mut serialized_items = Vec::new();
            for bug in &items {
                let mut val = serde_json::to_value(bug).unwrap_or(serde_json::json!({}));
                val["slug"] = serde_json::Value::String(bug.bugid.clone());
                val["problem"] = serde_json::Value::String(bug.problem().to_string());
                val["severity"] = serde_json::to_value(bug.severity()).unwrap();
                val["severity_explanation"] =
                    serde_json::to_value(bug.severity_explanation()).unwrap();
                val["description"] = serde_json::to_value(bug.description()).unwrap();
                val["inline_review"] = serde_json::Value::String(bug.inline_review());
                val["locations"] = serde_json::to_value(bug.locations()).unwrap();
                val["source_files"] = serde_json::to_value(bug.source_files()).unwrap();
                val["introduced_in_commit"] =
                    serde_json::to_value(bug.introduced_in_commit()).unwrap();
                val["verified_on_sha"] = serde_json::to_value(bug.verified_on_sha()).unwrap();
                val["is_fixed"] = serde_json::Value::Bool(bug.is_fixed());
                val["fixed_in_commit"] = serde_json::to_value(bug.fixed_in_commit()).unwrap();
                val["raw_input"] = serde_json::to_value(bug.raw_input()).unwrap();
                val["tokens_in"] = serde_json::Value::Number(bug.tokens_in().into());
                val["tokens_out"] = serde_json::Value::Number(bug.tokens_out().into());
                val["tokens_cached"] = serde_json::Value::Number(bug.tokens_cached().into());
                val.as_object_mut().map(|obj| obj.remove("enrichments"));
                val["evidence"] = summaries.remove(&bug.id).unwrap_or_else(|| serde_json::json!({"count": 0, "models": [], "tools": [], "unknown_models": 0}));
                val.as_object_mut().unwrap().remove("raw_input");
                val.as_object_mut().unwrap().remove("vector_json");
                serialized_items.push(val);
            }
            Ok(Json(serde_json::json!({
                "items": serialized_items,
                "total": total,
                "page": page,
                "per_page": per_page
            })))
        }
        Err(e) => {
            tracing::error!("Failed to fetch bugs list: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn list_bug_subsystems(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugSubsystemsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let lifecycle_status = match query.lifecycle_status.as_deref().map(str::parse) {
        Some(Ok(status)) => status,
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => crate::db::BugLifecycleStatus::Open,
    };
    let scope = principal.maintained_section_names();
    let visibility = principal.visibility(&scope);
    // Key the cache on the parsed status so the alias and the canonical
    // spelling do not each get their own entry, and on the principal's scope so
    // that one caller's counts are never served to a caller with a different
    // reach. Every principal who reads everything shares one entry.
    let scope_key = match visibility {
        crate::db::BugVisibility::Unrestricted => "*".to_string(),
        crate::db::BugVisibility::Sections(sections) => sections.join("\n"),
    };
    let cache_key = Some(format!("{}|{}", lifecycle_status.as_str(), scope_key));

    let res = state
        .bug_subsystems_cache
        .get_or_fetch(cache_key, || async {
            let db = state.db.clone();
            let counts = db
                .get_subsystems_bug_counts(Some(lifecycle_status), visibility)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to fetch bug subsystem counts: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            let list: Vec<serde_json::Value> = counts
                .into_iter()
                .map(|(name, count)| {
                    serde_json::json!({
                        "name": name,
                        "count": count,
                        "open_bugs": count,
                    })
                })
                .collect();
            Ok::<Vec<serde_json::Value>, StatusCode>(list)
        })
        .await?;

    Ok(Json(serde_json::Value::Array(res)))
}

#[derive(Debug, serde::Deserialize)]
pub struct BugActionPayload {
    /// Client-declared provenance. Author always comes from authentication.
    pub tool: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub action: BugAction,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BugAction {
    Comment {
        content: String,
    },
    Close {
        reason: Option<String>,
    },
    Dismiss {
        reason: Option<String>,
    },
    MarkDuplicate {
        duplicate_of_id: Option<i64>,
        duplicate_of_bugid: Option<String>,
        reasoning: Option<String>,
    },
    /// Hands a bug to someone, or drops the assignment when the assignee is
    /// absent, empty, or null.
    Assign {
        assignee: Option<String>,
        reason: Option<String>,
    },
}

/// Checks that a string looks like an email address.
///
/// This deliberately stops well short of RFC validation. Its only job is to
/// catch a value that clearly is not an address, since assignees are matched
/// by exact string and a malformed one can never be filtered for.
fn is_plausible_email(value: &str) -> bool {
    match value.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !value.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// The authority each action demands.
///
/// Commenting is a conversation and is open to anyone who may read the bug,
/// which includes the kernel security list. Everything else rewrites the bug's
/// state and belongs to whoever maintains the affected code.
fn required_access(action: &BugAction) -> BugAccess {
    match action {
        BugAction::Comment { .. } => BugAccess::Comment,
        BugAction::Close { .. }
        | BugAction::Dismiss { .. }
        | BugAction::MarkDuplicate { .. }
        | BugAction::Assign { .. } => BugAccess::Manage,
    }
}

/// Renders a failed bug lookup for an endpoint that answers with a message.
///
/// A bug the caller has no authority over is reported as absent, exactly as
/// the read endpoints report it, so that the two cases stay indistinguishable.
fn bug_lookup_denial(status: StatusCode) -> (StatusCode, String) {
    let message = match status {
        StatusCode::BAD_REQUEST => "Missing id or bugid param",
        StatusCode::NOT_FOUND => "Bug not found",
        StatusCode::UNAUTHORIZED => "Please log in to access bugs repository.",
        _ => "Database error",
    };
    (status, message.to_string())
}

async fn bug_action(
    principal: Principal,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<BugQuery>,
    axum::extract::Json(payload): axum::extract::Json<BugActionPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    // The check can only run once the bug is loaded, because what the caller
    // may do depends on which subsystems the bug is attributed to.
    let bug = readable_bug(&state, &principal, &query)
        .await
        .map_err(bug_lookup_denial)?;
    let access = bug_access(&state, &principal, bug.id)
        .await
        .map_err(bug_lookup_denial)?;
    let required = required_access(&payload.action);
    if access < required {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "This action requires {} authority over the bug.",
                required.describe()
            ),
        ));
    }

    // Testing mode resolves an operator that proved no address, so fall back to
    // naming the peer rather than attributing the change to nobody.
    let actor = if principal.email().is_empty() {
        format!("authorized client ({})", addr.ip())
    } else {
        principal.email().to_string()
    };
    let tool = payload
        .tool
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("api");
    let model = payload.model.filter(|s| !s.trim().is_empty());
    let db = state.db.with_bug_actor(&actor, tool, model);
    match payload.action {
        BugAction::Comment { content } => {
            if content.trim().is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Comment content is required".into(),
                ));
            }
            db.add_bug_enrichment(
                bug.id,
                &crate::db::NewBugEnrichment {
                    kind: "comment".into(),
                    content: Some(content),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Close { reason } => {
            db.change_bug_status_with_reason(
                bug.id,
                crate::db::BugLifecycleStatus::Closed,
                reason.as_deref(),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Dismiss { reason } => {
            db.change_bug_status_with_reason(
                bug.id,
                crate::db::BugLifecycleStatus::Dismissed,
                reason.as_deref(),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::MarkDuplicate {
            duplicate_of_id,
            duplicate_of_bugid,
            reasoning,
        } => {
            let target = match (duplicate_of_id, duplicate_of_bugid) {
                (Some(id), None) => db.get_bug(id).await,
                (None, Some(bugid)) => db.get_bug_by_bugid(bugid.trim()).await,
                _ => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Provide one existing bug ID".into(),
                    ));
                }
            }
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let target = target
                .filter(|b| b.id != bug.id && b.duplicate_of_id.is_none())
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        "Choose an existing canonical bug, distinct from this bug".into(),
                    )
                })?;
            // Marking a bug duplicate of one the caller cannot manage would
            // both disclose that the target exists and rewrite its history, so
            // that case answers exactly like a target that is not there.
            let target_access = bug_access(&state, &principal, target.id)
                .await
                .map_err(bug_lookup_denial)?;
            if !target_access.can_manage() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Choose an existing canonical bug, distinct from this bug".into(),
                ));
            }
            let duplicate_of_id = target.id;
            db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                ephemeral_id: bug.id,
                canonical_id: duplicate_of_id,
                reasoning: reasoning.as_deref().unwrap_or("Marked as duplicate"),
                ..Default::default()
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Assign { assignee, reason } => {
            let assignee = assignee.as_deref().map(str::trim).filter(|s| !s.is_empty());
            // A bare sanity check, not full validation: assignees are matched
            // by exact string, so a value that is obviously not an address
            // would create an assignment nobody can ever filter for.
            if let Some(who) = assignee
                && !is_plausible_email(who)
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Assignee must be an email address".into(),
                ));
            }
            db.assign_bug(bug.id, assignee, reason.as_deref())
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    Ok(Json(serde_json::json!({ "status": "success" })))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_synthetic_id_format() {
        let id = generate_synthetic_id("test");
        assert!(id.starts_with("sashiko-test-"));
        assert!(id.ends_with("@sashiko.local"));
    }

    #[tokio::test]
    async fn test_local_token_authorizes_ingest_but_grants_no_identity() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        // Everything that could authorize the request for another reason is
        // switched off, so only the token can be what admits it.
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.acl = crate::settings::AclSettings::default();
        let settings = Arc::new(settings);

        let local_token = crate::auth::LocalToken::generate().unwrap();
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                local_token: Some(local_token.clone()),
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let submit = format!("http://{}/api/submit", addr);
        let payload = serde_json::json!({
            "type": "remote",
            "sha": "1234567890abcdef1234567890abcdef12345678",
            "repo": "https://example.org/linux.git",
        });

        let anonymous = client
            .post(&submit)
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(anonymous, 403, "loopback alone must not ingest");

        let wrong = client
            .post(&submit)
            .bearer_auth("0".repeat(64))
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(
            wrong, 403,
            "a token this server did not write must not ingest"
        );

        let accepted = client
            .post(&submit)
            .bearer_auth(local_token.secret())
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert!(accepted.is_success(), "token was refused: {}", accepted);

        // The token is an authority, not an identity, so it cannot act on a
        // bug however local its holder is.
        let bug_action = client
            .post(format!("http://{}/api/bug/action?bugid=linux-1", addr))
            .bearer_auth(local_token.secret())
            .json(&serde_json::json!({"action": {"kind": "close"}}))
            .send()
            .await
            .unwrap()
            .status();
        assert!(
            !bug_action.is_success(),
            "token reached a bug route: {}",
            bug_action
        );
    }

    #[tokio::test]
    async fn test_bug_input_endpoint_serves_per_bug_payload() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let make_bug = |bugid: &str, title: &str| crate::db::NewBug {
            bugid: bugid.to_string(),
            title: title.to_string(),
            lifecycle_status: crate::db::BugLifecycleStatus::New,
            pipeline_state: crate::db::BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 100,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![],
        };

        let canonical = db
            .create_bug_with_enrichment(
                &make_bug("linux-canonical", "UAF in canonical path"),
                Some(&crate::db::NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("test-model".to_string()),
                    created_at: 100,
                    content: Some("canonical reasoning".to_string()),
                    data_json: Some(serde_json::json!({"problem": "canonical problem"})),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let duplicate = db
            .create_bug_with_enrichment(
                &make_bug("linux-duplicate", "UAF spotted again"),
                Some(&crate::db::NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("test-model".to_string()),
                    created_at: 200,
                    content: Some("duplicate reasoning".to_string()),
                    data_json: Some(serde_json::json!({"problem": "duplicate problem"})),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
            preserve_triage: false,
            ephemeral_id: duplicate,
            canonical_id: canonical,
            reasoning: "same defect",
            logs: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        })
        .await
        .unwrap();

        // Bug routes require a resolved principal. Testing mode supplies an
        // operator one, so that this test exercises the handler rather than
        // the authorization layer, which is covered separately.
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = true;
        let settings = Arc::new(settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let canonical_input: serde_json::Value = reqwest::get(format!(
            "http://{}/api/bug/input?bugid=linux-canonical",
            addr
        ))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(canonical_input["bugid"], "linux-canonical");
        let inputs = canonical_input["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["input"]["problem"], "canonical problem");

        // A duplicate resolves to the payload that produced it, not to the
        // canonical bug's payload.
        let duplicate_input: serde_json::Value =
            reqwest::get(format!("http://{}/api/bug/input?id={}", addr, duplicate))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let inputs = duplicate_input["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["input"]["problem"], "duplicate problem");
        assert_eq!(inputs[0]["model"], "test-model");

        let missing = reqwest::get(format!("http://{}/api/bug/input?id=999999", addr))
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test]
    async fn test_bug_endpoints() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-12345678".to_string(),
                title: "UAF in test_device".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123456,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "drivers/net",
                )],
            })
            .await
            .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &crate::db::NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456,
                content: Some("Trace".to_string()),
                data_json: Some(serde_json::json!({
                    "severity": "Critical",
                    "severity_int": 4,
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &crate::db::NewBugEnrichment {
                kind: "report".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123457,
                content: Some("Inline review".to_string()),
                data_json: None,
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
                logs: Some("[{\"role\":\"user\",\"content\":\"test\"}]".to_string()),
            },
        )
        .await
        .unwrap();

        // Bug reads need a principal with authority over the bug. The security
        // list reads every bug and holds no other capability, which keeps the
        // capability assertions further down honest.
        const SECRET: &str = "bug-test-secret-12345678901234567890";
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.jwt_secret = Some(SECRET.to_string());
        settings.server.acl.security = vec!["security@example.org".to_string()];
        settings.server.acl.admins = vec!["operator@example.org".to_string()];
        settings.server.acl.blocklist = Vec::new();

        let settings = Arc::new(settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);

        // The capability assertions further down turn on this, since an
        // identity in the security list holds no capability of its own.
        let local_token = crate::auth::LocalToken::generate().unwrap();

        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                dry_run: true,
                local_token: Some(local_token.clone()),
                ..Default::default()
            },
        );

        let token = crate::auth::create_token(
            "security@example.org",
            SECRET,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let mut auth_headers = reqwest::header::HeaderMap::new();
        auth_headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(auth_headers)
            .build()
            .unwrap();
        let get = |url: String| client.get(url).send();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Test 1: get_bug by bugid (logs omitted)
        let res = get(format!("http://{}/api/bug?bugid=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json: serde_json::Value = res.json().await.unwrap();
        assert_eq!(json["bugid"], "linux-12345678");
        assert_eq!(json["problem"], "UAF in test_device");
        assert!(json["logs"].is_null());
        assert!(json.get("enrichments").is_none());
        assert!(json.get("raw_input").is_none());
        assert_eq!(json["evidence"]["count"], 1);
        assert_eq!(json["evidence"]["activity"].as_array().unwrap().len(), 4);

        // Test 1c: get_bug_enrichments
        let res_enrich = get(format!(
            "http://{}/api/bug/enrichments?bugid=linux-12345678",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_enrich.status(), 200);
        let enrich_json: serde_json::Value = res_enrich.json().await.unwrap();
        assert_eq!(enrich_json.as_array().unwrap().len(), 4);
        assert!(
            enrich_json
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "report")
        );

        // Test 1b: get_bug by legacy slug param
        let res_slug = get(format!("http://{}/api/bug?slug=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_slug.status(), 200);

        // Test 2: get_bug_logs by bugid
        let res_logs = get(format!("http://{}/api/bug/logs?bugid=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_logs.status(), 200);
        let logs_json: serde_json::Value = res_logs.json().await.unwrap();
        assert!(logs_json.is_array());
        assert_eq!(logs_json[0]["role"], "user");

        // Test 2b: get_bug_logs by legacy slug param
        let res_logs_slug = get(format!("http://{}/api/bug/logs?slug=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_logs_slug.status(), 200);

        // Test 2b: get_bug_logs by id
        let res_logs_id = get(format!("http://{}/api/bug/logs?id={}", addr, bug_id))
            .await
            .unwrap();
        assert_eq!(res_logs_id.status(), 200);

        // Test 3: list_bugs with subsystem filter
        let res_sub = get(format!("http://{}/api/bugs?subsystem=drivers/net", addr))
            .await
            .unwrap();
        assert_eq!(res_sub.status(), 200);
        let list_json: serde_json::Value = res_sub.json().await.unwrap();
        assert_eq!(list_json["total"], 1);
        assert_eq!(list_json["items"].as_array().unwrap().len(), 1);
        assert!(list_json["items"][0]["logs"].is_null());

        // Test 3a: list_bugs with hierarchical parent subsystem filter ("drivers" matches "drivers/net")
        let res_hier = get(format!("http://{}/api/bugs?subsystem=drivers", addr))
            .await
            .unwrap();
        assert_eq!(res_hier.status(), 200);
        let hier_json: serde_json::Value = res_hier.json().await.unwrap();
        assert_eq!(hier_json["total"], 1);

        // Test 3b: list_bugs with non-matching subsystem filter
        let res_other_sub = get(format!("http://{}/api/bugs?subsystem=btrfs", addr))
            .await
            .unwrap();
        assert_eq!(res_other_sub.status(), 200);
        let empty_list: serde_json::Value = res_other_sub.json().await.unwrap();
        assert_eq!(empty_list["total"], 0);

        // Test 3c: list_bugs with lifecycle filter
        let res_status = get(format!("http://{}/api/bugs?lifecycle_status=new", addr))
            .await
            .unwrap();
        assert_eq!(res_status.status(), 200);
        let status_json: serde_json::Value = res_status.json().await.unwrap();
        assert_eq!(status_json["total"], 1);

        // Test 3d: list_bugs with lifecycle filter mismatch
        let res_open = get(format!("http://{}/api/bugs?lifecycle_status=open", addr))
            .await
            .unwrap();
        assert_eq!(res_open.status(), 200);
        let open_json: serde_json::Value = res_open.json().await.unwrap();
        assert_eq!(open_json["total"], 0);

        // Test 3d1: the historical status spelling still selects the lifecycle
        // axis, so bookmarked URLs keep working.
        let res_alias = get(format!("http://{}/api/bugs?status=new", addr))
            .await
            .unwrap();
        assert_eq!(res_alias.status(), 200);
        let alias_json: serde_json::Value = res_alias.json().await.unwrap();
        assert_eq!(alias_json["total"], 1);

        // Test 3d2: the pipeline axis is filterable independently.
        let res_pipeline = get(format!("http://{}/api/bugs?pipeline_state=pending", addr))
            .await
            .unwrap();
        assert_eq!(res_pipeline.status(), 200);
        let pipeline_json: serde_json::Value = res_pipeline.json().await.unwrap();
        assert_eq!(pipeline_json["total"], 1);

        // Test 3d3: an unknown state is rejected rather than silently matching
        // nothing, which would look identical to an empty result set.
        let res_bogus = get(format!("http://{}/api/bugs?lifecycle_status=raw", addr))
            .await
            .unwrap();
        assert_eq!(res_bogus.status(), 400);

        // Test 3e: list_bugs with sorting
        let res_sort = get(format!(
            "http://{}/api/bugs?sort_by=severity&sort_order=desc",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_sort.status(), 200);

        // Test 3f: list_bugs with multi-subsystem filter (subsystems parameter)
        let res_multi = get(format!(
            "http://{}/api/bugs?subsystems=drivers/net,btrfs",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_multi.status(), 200);
        let multi_json: serde_json::Value = res_multi.json().await.unwrap();
        assert_eq!(multi_json["total"], 1);

        // Test 3g: list_bugs with comma-separated single subsystem parameter
        let res_comma = get(format!("http://{}/api/bugs?subsystem=drivers/net,fs", addr))
            .await
            .unwrap();
        assert_eq!(res_comma.status(), 200);
        let comma_json: serde_json::Value = res_comma.json().await.unwrap();
        assert_eq!(comma_json["total"], 1);

        // Test 3h: list_bugs with non-matching multi-subsystem
        let res_multi_none = get(format!("http://{}/api/bugs?subsystems=btrfs,ext4", addr))
            .await
            .unwrap();
        assert_eq!(res_multi_none.status(), 200);
        let multi_none_json: serde_json::Value = res_multi_none.json().await.unwrap();
        assert_eq!(multi_none_json["total"], 0);

        // Test 3i: GET /api/bugs/subsystems with lifecycle_status=new
        let res_subs_api = get(format!(
            "http://{}/api/bugs/subsystems?lifecycle_status=new",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_api.status(), 200);
        let subs_json: serde_json::Value = res_subs_api.json().await.unwrap();
        let subs_arr = subs_json.as_array().unwrap();
        assert_eq!(subs_arr.len(), 1);
        assert_eq!(subs_arr[0]["name"], "drivers/net");
        assert_eq!(subs_arr[0]["count"], 1);
        assert_eq!(subs_arr[0]["open_bugs"], 1);

        // Test 3j: GET /api/subsystems alias
        let res_subs_alias = get(format!(
            "http://{}/api/subsystems?lifecycle_status=new",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_alias.status(), 200);

        // Test 3k: GET /api/bugs/subsystems with lifecycle_status=open (should
        // skip 0-bug entries)
        let res_subs_open = get(format!(
            "http://{}/api/bugs/subsystems?lifecycle_status=open",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_open.status(), 200);
        let open_subs_json: serde_json::Value = res_subs_open.json().await.unwrap();
        assert_eq!(open_subs_json.as_array().unwrap().len(), 0);

        // Test 3l: duplicate linking on get_bug
        let dup_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-dup-endpoint".to_string(),
                title: "Duplicate issue".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123457,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "drivers/net",
                )],
            })
            .await
            .unwrap();
        db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
            preserve_triage: false,
            ephemeral_id: dup_id,
            canonical_id: bug_id,
            reasoning: "Dup of test_device",
            logs: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        })
        .await
        .unwrap();

        // Check duplicate_of on the duplicate bug
        let res_dup = get(format!("http://{}/api/bug?id={}", addr, dup_id))
            .await
            .unwrap();
        assert_eq!(res_dup.status(), 200);
        let dup_resp: serde_json::Value = res_dup.json().await.unwrap();
        assert_eq!(dup_resp["duplicate_of"]["id"], bug_id);
        assert_eq!(dup_resp["duplicate_of"]["bugid"], "linux-12345678");

        // Check duplicates array on canonical bug
        let res_canon = get(format!("http://{}/api/bug?id={}", addr, bug_id))
            .await
            .unwrap();
        assert_eq!(res_canon.status(), 200);
        let canon_resp: serde_json::Value = res_canon.json().await.unwrap();
        assert_eq!(canon_resp["duplicates"].as_array().unwrap().len(), 1);
        assert_eq!(canon_resp["duplicates"][0]["id"], dup_id);

        // Test 4: redirect_bug
        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let res = no_redirect
            .get(format!("http://{}/bug/pb-12345678", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 307);
        assert_eq!(
            res.headers().get("location").unwrap().to_str().unwrap(),
            "/#/bug/pb-12345678"
        );

        // Test 5: bug_action. The security list may comment on any bug.
        let action_res = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({
                "action": "comment",
                "content": "A new test comment via API"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(action_res.status(), 200);

        // Closing rewrites the bug's state, which the security list may not do
        // outside its own subsystems.
        let refused = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({"action":"close", "reason":"Fixed upstream"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 403);
        assert_eq!(
            refused.text().await.unwrap(),
            "This action requires maintainer authority over the bug."
        );

        let operator_token = crate::auth::create_token(
            "operator@example.org",
            SECRET,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let close = reqwest::Client::new()
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .bearer_auth(operator_token)
            .json(&serde_json::json!({"action":"close", "reason":"Fixed upstream", "tool":"web"}))
            .send()
            .await
            .unwrap();
        assert_eq!(close.status(), 200);
        let stored = db.get_bug(bug_id).await.unwrap().unwrap();
        let comment = stored
            .enrichments
            .iter()
            .find(|e| e.content.as_deref() == Some("Fixed upstream"))
            .unwrap();
        assert_eq!(comment.author.as_deref(), Some("operator@example.org"));
        assert_eq!(comment.tool, "web");
        assert!(comment.model.is_none());
        let raw: serde_json::Value = get(format!("http://{}/api/bug/raw?id={}", addr, bug_id))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(raw["records"].as_array().unwrap().len(), 2);
        assert!(
            raw["records"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|b| b["enrichments"].as_array().unwrap())
                .any(|e| e["logs"].as_str().is_some_and(|s| s.contains("test")))
        );
        let missing = get(format!("http://{}/api/bug/raw?id=999999", addr))
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        // Test 6: get_config reports capabilities, and a remote unauthenticated
        // action is denied with a clear message. Bug authority is deliberately
        // absent from that report, since it is per bug rather than global.
        //
        // The security list holds no capability of its own, so an identity
        // alone reports none, however local the caller is.
        let cfg_identity: serde_json::Value = get(format!("http://{}/api/config", addr))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg_identity["permissions"]["review"], false);
        assert!(cfg_identity["permissions"]["action"].is_null());

        // Presenting the token the server published is what grants them.
        let cfg_local: serde_json::Value = reqwest::Client::new()
            .get(format!("http://{}/api/config", addr))
            .bearer_auth(local_token.secret())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg_local["permissions"]["review"], true);
        assert!(cfg_local["permissions"]["action"].is_null());

        let remote_denied = reqwest::Client::new()
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .header("x-forwarded-for", "203.0.113.1")
            .json(&serde_json::json!({
                "action": "comment",
                "content": "Attempt by remote guest"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(remote_denied.status(), 401);
        let err_msg = remote_denied.text().await.unwrap();
        assert_eq!(err_msg, "Please log in to access bugs repository.");
    }

    #[tokio::test]
    async fn test_acl_blocklist_authorization_and_auth_endpoints() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let mut base_settings = crate::settings::Settings::new().unwrap();
        base_settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        base_settings.server.testing_mode = false;
        base_settings.server.acl.admins = vec!["admin@example.com".to_string()];
        base_settings.server.acl.review = vec!["reviewer@example.com".to_string()];
        base_settings.server.acl.blocklist = vec![
            "blocked@example.com".to_string(),
            "admin@example.com".to_string(),
        ];

        let settings = Arc::new(base_settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);

        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx.clone(),
            fetch_tx.clone(),
            ServerOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 1. request_link for blocklisted email (even though it's in admins) -> 200 OK (unconditional)
        let res_blocked_admin = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "admin@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocked_admin.status(), 200);

        // 2. request_link for blocklisted email with case variations -> 200 OK (unconditional)
        let res_blocked_case = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "BLOCKED@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocked_case.status(), 200);

        // 3. request_link for allowed email -> 200 OK
        let res_allowed = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "reviewer@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_allowed.status(), 200);

        // 4. verify_link for blocklisted email -> 403 Forbidden
        let secret = "test_jwt_secret_12345678901234567890";
        let blocked_token = crate::auth::create_token(
            "blocked@example.com",
            secret,
            Some("sign_in_link".to_string()),
            1800,
        )
        .unwrap();

        let res_verify_blocked = client
            .get(format!(
                "http://{}/api/auth/verify?token={}",
                addr, blocked_token
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res_verify_blocked.status(), 403);

        // 5. verify_link for allowed email -> 200 OK and returns session token
        let allowed_token = crate::auth::create_token(
            "reviewer@example.com",
            secret,
            Some("sign_in_link".to_string()),
            1800,
        )
        .unwrap();

        let res_verify_allowed = client
            .get(format!(
                "http://{}/api/auth/verify?token={}",
                addr, allowed_token
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res_verify_allowed.status(), 200);
        let session_json: serde_json::Value = res_verify_allowed.json().await.unwrap();
        let session_token = session_json["token"].as_str().unwrap();

        // 6. refresh_token for blocklisted email session -> 403 Forbidden
        let blocked_session_token = crate::auth::create_token(
            "blocked@example.com",
            secret,
            Some("session".to_string()),
            86400,
        )
        .unwrap();

        unsafe {
            std::env::set_var("JWT_SECRET", secret);
        }

        let res_refresh_blocked = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", blocked_session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_refresh_blocked.status(), 403);

        // 7. refresh_token for allowed email session -> 200 OK
        let res_refresh_allowed = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_refresh_allowed.status(), 200);

        // Sign-in link token cannot be used directly as a session API token
        let res_bearer_sign_in = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", allowed_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_bearer_sign_in.status(), 401);

        // Session token whose initial issue date is older than 30 days cannot be refreshed
        let old_iat = (chrono::Utc::now().timestamp() as usize).saturating_sub(31 * 86400);
        let expired_session_token = crate::auth::create_token_with_session(
            "reviewer@example.com",
            secret,
            Some("session".to_string()),
            86400,
            Some(old_iat),
            Some("old-session-id".to_string()),
        )
        .unwrap();
        let res_expired_refresh = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", expired_session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_expired_refresh.status(), 401);

        // 8. Test is_authorized directly:
        let mut proxy_headers = axum::http::HeaderMap::new();
        proxy_headers.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        let state_arc = Arc::new(AppState {
            settings: settings.clone(),
            db: db.clone(),
            sender: event_tx,
            fetch_sender: fetch_tx,
            read_only: false,
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            local_token: None,
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
        });

        let blocked_user = crate::auth::AuthUser::new("blocked@example.com");
        assert!(!is_authorized(
            &state_arc,
            &proxy_headers,
            Some(&blocked_user),
            crate::settings::Permission::Review,
        ));

        let empty_headers = axum::http::HeaderMap::new();
        assert!(!is_authorized(
            &state_arc,
            &empty_headers,
            Some(&blocked_user),
            crate::settings::Permission::Review,
        ));

        let allowed_user = crate::auth::AuthUser::new("reviewer@example.com");
        assert!(is_authorized(
            &state_arc,
            &proxy_headers,
            Some(&allowed_user),
            crate::settings::Permission::Review
        ));
    }

    #[tokio::test]
    async fn test_sign_in_link_email_delivery_and_logging() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.jwt_secret = Some("test-jwt-secret-for-email-delivery-123".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings.server.acl.admins = vec!["maintainer@example.org".to_string()];
        settings.smtp = Some(crate::settings::SmtpSettings {
            server: "smtp.example.org".to_string(),
            port: 587,
            username: None,
            password: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .header("x-forwarded-for", "198.51.100.24, 10.0.0.1")
            .json(&serde_json::json!({ "email": "maintainer@example.org" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        let email = db
            .lock_pending_email()
            .await
            .unwrap()
            .expect("email was queued in outbox");
        assert_eq!(email.kind, crate::db::EmailKind::SignInLink);
        assert_eq!(email.status, "Sending");
        assert_eq!(email.to_addresses, r#"["maintainer@example.org"]"#);
        assert_eq!(email.subject, "[sashiko] Your sign-in link");
        assert!(
            email
                .body
                .contains("https://sashiko.dev/auth/verify?token=")
        );
        assert!(email.body.contains("Requested from 198.51.100.24."));
        assert!(email.body.contains("Open this link within 30 minutes"));
        assert!(
            email
                .body
                .contains("-- \nSashiko AI review · https://sashiko.dev")
        );
    }

    #[tokio::test]
    async fn test_request_link_unconditional_response_equivalence() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.smtp = Some(crate::settings::SmtpSettings {
            server: "smtp.example.org".to_string(),
            port: 587,
            username: None,
            password: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });
        settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings
            .server
            .acl
            .bug_reporters
            .push("eligible@example.com".to_string());
        settings
            .server
            .acl
            .blocklist
            .push("blocklisted@example.com".to_string());

        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = tokio::sync::mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 1. Eligible address
        let res_eligible = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "eligible@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_eligible.status(), 200);
        let bytes_eligible = res_eligible.bytes().await.unwrap();

        // 2. Ineligible (unknown) address
        let res_ineligible = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "unregistered@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_ineligible.status(), 200);
        let bytes_ineligible = res_ineligible.bytes().await.unwrap();

        // 3. Blocklisted address
        let res_blocklisted = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "blocklisted@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocklisted.status(), 200);
        let bytes_blocklisted = res_blocklisted.bytes().await.unwrap();

        // 4. Malformed address
        let res_malformed_email = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "not-an-email" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_malformed_email.status(), 200);
        let bytes_malformed_email = res_malformed_email.bytes().await.unwrap();

        // 5. Malformed payload
        let res_malformed_json = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .header("Content-Type", "application/json")
            .body("{not valid json")
            .send()
            .await
            .unwrap();
        assert_eq!(res_malformed_json.status(), 200);
        let bytes_malformed_json = res_malformed_json.bytes().await.unwrap();

        // All responses must be byte-identical
        assert_eq!(bytes_eligible, bytes_ineligible);
        assert_eq!(bytes_eligible, bytes_blocklisted);
        assert_eq!(bytes_eligible, bytes_malformed_email);
        assert_eq!(bytes_eligible, bytes_malformed_json);

        // Only the eligible address must have queued an email
        let queued = db
            .lock_pending_email()
            .await
            .unwrap()
            .expect("eligible email queued");
        assert_eq!(queued.to_addresses, r#"["eligible@example.com"]"#);
        assert!(db.lock_pending_email().await.unwrap().is_none());
    }

    #[test]
    fn test_sign_in_link_rate_limiter_buckets() {
        let limiter = SignInLinkRateLimiter::new();

        // 1. Per-address limit (3 per 15 mins)
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.1"));
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.2"));
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.3"));
        // 4th attempt for the same address fails even from another IP
        assert!(!limiter.check_and_record("dev@example.org", "192.0.2.4"));

        // Another address from 192.0.2.1 still succeeds
        assert!(limiter.check_and_record("dev2@example.org", "192.0.2.1"));

        // 2. Per-IP limit (10 per 15 mins)
        // 192.0.2.1 already made 2 requests above (dev@example.org and dev2@example.org)
        for i in 3..=10 {
            assert!(limiter.check_and_record(&format!("user{}@example.org", i), "192.0.2.1"));
        }
        // 11th request from 192.0.2.1 fails regardless of address
        assert!(!limiter.check_and_record("user11@example.org", "192.0.2.1"));
        // But another IP can still request
        assert!(limiter.check_and_record("user11@example.org", "192.0.2.99"));

        // 3. Global limit (100 per hour)
        let global_limiter = SignInLinkRateLimiter::new();
        for i in 0..100 {
            assert!(global_limiter.check_and_record(
                &format!("batch{}@example.org", i),
                &format!("198.51.100.{}", i % 250),
            ));
        }
        // 101st request fails globally
        assert!(!global_limiter.check_and_record("overflow@example.org", "198.51.100.254"));
    }

    #[tokio::test]
    async fn test_request_link_rate_limiting_suppresses_mail_but_returns_200() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.smtp = Some(crate::settings::SmtpSettings {
            server: "smtp.example.org".to_string(),
            port: 587,
            username: None,
            password: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });
        settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings
            .server
            .acl
            .bug_reporters
            .push("maintainer@example.org".to_string());

        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = tokio::sync::mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 3 allowed requests within 15 minutes
        for _ in 0..3 {
            let res = client
                .post(format!("http://{}/api/auth/request-link", addr))
                .json(&serde_json::json!({ "email": "maintainer@example.org" }))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
        }

        // 4th request exceeds rate limit: still returns 200 OK
        let res_4th = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "maintainer@example.org" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_4th.status(), 200);

        // Verify exactly 3 emails were queued, not 4
        let mut queued_count = 0;
        while let Ok(Some(_email)) = db.lock_pending_email().await {
            queued_count += 1;
        }
        assert_eq!(queued_count, 3);
    }

    #[tokio::test]
    async fn test_blocklist_outranks_every_bypass() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let state_with = |testing_mode: bool, allow_all_submit: bool| {
            let mut settings = crate::settings::Settings::new().unwrap();
            settings.server.testing_mode = testing_mode;
            settings.server.acl.admins = vec!["operator@example.org".to_string()];
            // Spelled with stray padding and mixed case to pin the tolerant
            // comparison, since a revocation that a capital letter defeats is
            // not a revocation.
            settings.server.acl.blocklist = vec![" Blocked@Example.ORG ".to_string()];
            let (event_tx, _event_rx) = mpsc::channel(10);
            let (fetch_tx, _fetch_rx) = mpsc::channel(10);
            Arc::new(AppState {
                settings: Arc::new(settings),
                db: db.clone(),
                sender: event_tx,
                fetch_sender: fetch_tx,
                read_only: false,
                forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
                allow_all_submit,
                smtp_enabled: false,
                dry_run: true,
                local_token: None,
                sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
                stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
                stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
                stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
                messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            })
        };

        let blocked = crate::auth::AuthUser::new("blocked@example.org");
        let operator = crate::auth::AuthUser::new("operator@example.org");
        let headers = axum::http::HeaderMap::new();

        for (testing_mode, allow_all_submit) in [(true, false), (false, true), (true, true)] {
            let state = state_with(testing_mode, allow_all_submit);
            assert!(
                !is_authorized(
                    &state,
                    &headers,
                    Some(&blocked),
                    crate::settings::Permission::Review
                ),
                "blocklisted caller admitted with testing_mode={} allow_all_submit={}",
                testing_mode,
                allow_all_submit
            );
            assert!(
                is_authorized(
                    &state,
                    &headers,
                    Some(&operator),
                    crate::settings::Permission::Review
                ),
                "bypass stopped working for everyone else"
            );
        }
    }

    #[tokio::test]
    async fn test_loopback_grants_nothing_without_the_token() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let state_with = |local_token: Option<crate::auth::LocalToken>| {
            let mut settings = crate::settings::Settings::new().unwrap();
            settings.server.testing_mode = false;
            let (event_tx, _event_rx) = mpsc::channel(10);
            let (fetch_tx, _fetch_rx) = mpsc::channel(10);
            Arc::new(AppState {
                settings: Arc::new(settings),
                db: db.clone(),
                sender: event_tx,
                fetch_sender: fetch_tx,
                read_only: false,
                forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
                allow_all_submit: false,
                smtp_enabled: false,
                dry_run: true,
                local_token,
                sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
                stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
                stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
                stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
                messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            })
        };

        let token = crate::auth::LocalToken::generate().unwrap();
        let bare = axum::http::HeaderMap::new();

        let mut presented = axum::http::HeaderMap::new();
        presented.insert(
            "authorization",
            format!("Bearer {}", token.secret()).parse().unwrap(),
        );

        // The dangerous shape: a reverse proxy that forwards no markers makes
        // every remote request look exactly like a local one, so arriving on
        // loopback buys nothing at all.
        assert!(
            !is_authorized(
                &state_with(None),
                &bare,
                None,
                crate::settings::Permission::Review
            ),
            "a request was authorized for its source address"
        );

        // Holding the token is the whole of the claim, so failing to present
        // one the server did publish is no different.
        assert!(
            !is_authorized(
                &state_with(Some(token.clone())),
                &bare,
                None,
                crate::settings::Permission::Review
            ),
            "a caller that presented nothing was admitted"
        );

        assert!(
            is_authorized(
                &state_with(Some(token.clone())),
                &presented,
                None,
                crate::settings::Permission::Review
            ),
            "the token was refused"
        );

        // Proxy markers used to veto the old bypass. They are irrelevant now:
        // what the caller can present does not change with the route it took.
        let mut forwarded = presented.clone();
        forwarded.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        assert!(
            is_authorized(
                &state_with(Some(token)),
                &forwarded,
                None,
                crate::settings::Permission::Review
            ),
            "a header a caller controls changed the answer"
        );
    }

    #[tokio::test]
    async fn bug_evidence_excludes_inaccessible_family_members() {
        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();
        let mut ids = Vec::new();
        for (name, section) in [
            ("canonical", "SECTION A"),
            ("hidden", "SECTION B"),
            ("sibling", "SECTION A"),
        ] {
            let bug: crate::db::NewBug = serde_json::from_value(serde_json::json!({
                "bugid": name, "title": format!("{name} title"), "reporter": format!("{name}@example.org"),
                "subsystems": [{"name": section, "source": "maintainers_section"}]
            })).unwrap();
            let id = db.create_bug(&bug).await.unwrap();
            for kind in ["report", "comment"] {
                db.add_bug_enrichment(
                    id,
                    &crate::db::NewBugEnrichment {
                        kind: kind.into(),
                        content: Some(format!("{name} {kind} confidential content")),
                        model: Some(format!("{name}-model")),
                        tool: format!("{name}-tool"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            ids.push(id);
        }
        for &id in &ids[1..] {
            db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                ephemeral_id: id,
                canonical_id: ids[0],
                reasoning: "same defect",
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let settings = Arc::new(crate::settings::Settings::new().unwrap());
        let (sender, _) = mpsc::channel(1);
        let (fetch_sender, _) = mpsc::channel(1);
        let state = Arc::new(AppState {
            settings,
            db,
            sender,
            fetch_sender,
            read_only: false,
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            local_token: None,
        });
        let index = crate::maintainers::MaintainersIndex::from_reader(
            b"Maintainers List\n================\n\nSECTION A\nM:\tAlice <a@example.org>\nF:\ta/\n\nSECTION B\nM:\tBob <b@example.org>\nF:\tb/\n".as_slice()
        ).unwrap();
        let acl = crate::settings::AclSettings {
            admins: vec!["operator@example.org".into()],
            ..Default::default()
        };
        // Exercise the canonical, a sibling duplicate, and a duplicate whose
        // canonical bug is invisible. Resolve locally to avoid global test state.
        for (email, id, count, excluded) in [
            ("a@example.org", ids[0], 2, vec!["hidden"]),
            ("a@example.org", ids[2], 2, vec!["hidden"]),
            ("b@example.org", ids[1], 1, vec!["canonical", "sibling"]),
            ("operator@example.org", ids[0], 3, vec![]),
        ] {
            let principal = Principal::resolve(email, &acl, Some(&index));
            let Json(body) = get_bug(
                principal,
                State(state.clone()),
                Query(BugQuery {
                    id: Some(id),
                    bugid: None,
                    slug: None,
                }),
            )
            .await
            .unwrap();
            assert_eq!(body["evidence"]["count"], count);
            let evidence = body["evidence"].to_string();
            for name in excluded {
                assert!(
                    !evidence.contains(name),
                    "{email} learned about {name}: {evidence}"
                );
            }
            if email == "b@example.org" {
                assert!(body.get("duplicate_of_id").is_none());
                assert!(body.get("duplicate_of").is_none());
            }
            if email == "operator@example.org" {
                assert!(evidence.contains("hidden comment confidential content"));
                assert!(evidence.contains("hidden-model"));
            }
        }
    }

    #[tokio::test]

    async fn test_bug_reads_require_an_authorized_principal() {
        const SECRET: &str = "bug-authz-secret-12345678901234567890";
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-authz01".to_string(),
                title: "UAF in btrfs".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123456,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "BTRFS FILE SYSTEM",
                )],
            })
            .await
            .unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.jwt_secret = Some(SECRET.to_string());
        settings.server.acl.admins = vec!["operator@example.org".to_string()];
        settings.server.acl.security = Vec::new();
        settings.server.acl.blocklist = Vec::new();
        settings.server.acl.bug_reporters = Vec::new();
        settings.server.read_only = false;
        let settings = Arc::new(settings);

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let token = |email: &str| {
            crate::auth::create_token(email, SECRET, Some("session".to_string()), 3600).unwrap()
        };
        let get = |path: String, bearer: Option<String>| {
            let mut req = client.get(format!("http://{}{}", addr, path));
            if let Some(bearer) = bearer {
                req = req.header("Authorization", format!("Bearer {}", bearer));
            }
            req.send()
        };

        // No credentials at all. Bug routes answer 401 rather than falling back
        // to anything the transport might suggest.
        let anonymous = get(format!("/api/bug?id={}", bug_id), None).await.unwrap();
        assert_eq!(anonymous.status(), 401);

        let garbage = get(format!("/api/bug?id={}", bug_id), Some("nonsense".into()))
            .await
            .unwrap();
        assert_eq!(garbage.status(), 401);

        // Authenticated, but maintains nothing and holds no capability. The
        // answer has to be indistinguishable from a bug that is not there.
        let stranger = token("stranger@example.org");
        let denied = get(format!("/api/bug?id={}", bug_id), Some(stranger.clone()))
            .await
            .unwrap();
        assert_eq!(denied.status(), 404);
        let absent = get("/api/bug?id=999999".to_string(), Some(stranger.clone()))
            .await
            .unwrap();
        assert_eq!(absent.status(), 404);

        for path in ["/api/bug/logs", "/api/bug/raw", "/api/bug/input"] {
            let res = get(format!("{}?id={}", path, bug_id), Some(stranger.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{} leaked to a stranger", path);
            let res = get(format!("{}?id={}", path, bug_id), None).await.unwrap();
            assert_eq!(res.status(), 401, "{} served without a session", path);
        }

        let listing: serde_json::Value = get("/api/bugs".to_string(), Some(stranger.clone()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["total"], 0);
        assert!(listing["items"].as_array().unwrap().is_empty());

        let facets: serde_json::Value =
            get("/api/bugs/subsystems".to_string(), Some(stranger.clone()))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert!(facets.as_array().unwrap().is_empty());

        // The operator sees everything.
        let operator = token("operator@example.org");
        let allowed = get(format!("/api/bug?id={}", bug_id), Some(operator.clone()))
            .await
            .unwrap();
        assert_eq!(allowed.status(), 200);
        let body: serde_json::Value = allowed.json().await.unwrap();
        assert_eq!(body["bugid"], "linux-authz01");
        assert_eq!(body["can_comment"], true);
        assert_eq!(body["can_manage"], true);

        let listing: serde_json::Value = get("/api/bugs".to_string(), Some(operator))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["total"], 1);

        // Mutations are not bypassable from loopback either, even though every
        // capability in the Permission enum is.
        let unauthenticated_action = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({"action": "comment", "content": "hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated_action.status(), 401);

        // Filing a bug spends money, so it needs the create capability. The
        // principal is resolved before the body is, so an anonymous caller is
        // turned away without the payload being looked at.
        let analyze = |bearer: Option<String>| {
            let mut req = client
                .post(format!("http://{}/api/bug/analyze", addr))
                .json(&serde_json::json!({
                    "problem": "a use after free",
                    "reasoning": "the pointer outlives the allocation",
                    "source_files": [],
                }));
            if let Some(bearer) = bearer {
                req = req.header("Authorization", format!("Bearer {}", bearer));
            }
            req.send()
        };

        let unauthenticated_analyze = analyze(None).await.unwrap();
        assert_eq!(unauthenticated_analyze.status(), 401);

        // A session alone is not enough, and the refusal happens before any
        // provider is built.
        let denied_analyze = analyze(Some(stranger)).await.unwrap();
        assert_eq!(denied_analyze.status(), 403);
        assert_eq!(
            denied_analyze.text().await.unwrap(),
            "You don't have permissions to file bugs."
        );
    }

    #[test]
    fn test_is_sign_in_eligible_checks_maintainers_and_blocklist() {
        let maintainers_text = r#"
Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
S:	Maintained
F:	fs/btrfs/

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
S:	Maintained
F:	net/
"#;
        let index =
            crate::maintainers::MaintainersIndex::from_reader(maintainers_text.as_bytes()).unwrap();
        let acl = crate::settings::AclSettings {
            admins: vec!["operator@example.org".to_string()],
            blocklist: vec!["clm@fb.com".to_string()],
            ..Default::default()
        };

        // Subsystem maintainer in MAINTAINERS (not in Settings.toml ACL) can sign in.
        assert!(is_sign_in_eligible(
            "davem@davemloft.net",
            &acl,
            Some(&index)
        ));
        assert!(is_sign_in_eligible(
            "  DAVEM@davemloft.net ",
            &acl,
            Some(&index)
        ));

        // Blocklist revokes sign-in even for a maintainer listed in MAINTAINERS.
        assert!(!is_sign_in_eligible("clm@fb.com", &acl, Some(&index)));

        // Configured operator can sign in even without a MAINTAINERS index.
        assert!(is_sign_in_eligible("operator@example.org", &acl, None));

        // Unknown address cannot sign in.
        assert!(!is_sign_in_eligible(
            "stranger@example.org",
            &acl,
            Some(&index)
        ));
    }
}

/// Whether the caller may exercise a capability.
///
/// The request's source address is deliberately not an input. A reverse proxy
/// terminates in front of the service and forwards from loopback, so the
/// address distinguishes nothing: a caller is trusted for what it can present,
/// not for where it appears to come from.
pub fn is_authorized(
    state: &std::sync::Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    perm: crate::settings::Permission,
) -> bool {
    // The blocklist is a revocation, so it outranks every bypass below it. It
    // can only match an address the caller actually presented; a blocklisted
    // person who omits their credential and presents the local token instead
    // has proved only that they share the server's machine, which grants no
    // authority over a bug.
    if auth.is_some_and(|user| state.settings.server.acl.is_blocklisted(&user.email)) {
        return false;
    }
    if state.settings.server.testing_mode {
        return true;
    }
    if state.allow_all_submit {
        return true;
    }

    // A caller holding this process's token has read a file only the server's
    // own user can read. Nothing else about where the request came from is
    // consulted: the source address cannot distinguish a local caller from an
    // internet one a reverse proxy forwarded, so it is not evidence of
    // anything. Bug routes resolve their own principal and never call here.
    if presents_local_token(headers, state) && perm.granted_by_local_token() {
        return true;
    }

    if let Some(user) = auth {
        return state.settings.server.acl.has_permission(&user.email, perm);
    }

    false
}

/// Whether the request carries the local operator token this process wrote.
///
/// The token shares the Authorization header with session JWTs, which is safe
/// because the two cannot be confused: a JWT never has the shape of a token,
/// and a token never carries the signature a JWT is accepted on.
pub(crate) fn presents_local_token(
    headers: &axum::http::HeaderMap,
    state: &std::sync::Arc<AppState>,
) -> bool {
    let Some(token) = state.local_token.as_ref() else {
        return false;
    };

    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| token.matches(presented.trim()))
}

fn resolve_jwt_secret(state: &AppState) -> Option<String> {
    state
        .settings
        .server
        .jwt_secret
        .clone()
        .or_else(|| std::env::var("JWT_SECRET").ok())
}

fn extract_client_ip(
    addr: &std::net::SocketAddr,
    headers: &axum::http::HeaderMap,
) -> Option<std::net::IpAddr> {
    for name in ["x-forwarded-for", "x-real-ip", "forwarded"] {
        if let Some(val) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let first = val.split(',').next().unwrap_or("").trim();
            if let Ok(ip) = first.parse::<std::net::IpAddr>() {
                return Some(ip);
            }
        }
    }
    Some(addr.ip())
}

fn build_sign_in_link_email(
    email: &str,
    link: &str,
    base_url: &str,
    lifetime_seconds: i64,
    client_ip: Option<std::net::IpAddr>,
) -> String {
    let minutes = lifetime_seconds / 60;
    let ip_line = match client_ip {
        Some(ip) => format!("\n\nRequested from {}.", ip),
        None => String::new(),
    };
    format!(
        "Someone asked to sign in to Sashiko as {email}.\n\n\
         Open this link within {minutes} minutes to continue:\n\n\
         {link}\
         {ip_line}\n\n\
         If you did not ask to sign in, ignore this message. Nothing has\n\
         changed and nobody has gained access.\n\n\
         -- \n\
         Sashiko AI review · {base_url}"
    )
}

#[derive(serde::Deserialize)]
struct RequestLinkRequest {
    #[serde(default)]
    email: String,
}

fn is_sign_in_eligible(
    email: &str,
    acl: &crate::settings::AclSettings,
    maintainers: Option<&crate::maintainers::MaintainersIndex>,
) -> bool {
    !email.is_empty()
        && !acl.is_blocklisted(email)
        && (acl.is_known_identity(email)
            || maintainers.is_some_and(|m| m.subsystems_for_address(email).is_some()))
}

const SIGN_IN_LINK_LIFETIME_SECONDS: i64 = 1800;

async fn request_link(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        let payload: Option<RequestLinkRequest> = serde_json::from_slice(&body).ok();
        let email = payload.as_ref().map(|p| p.email.trim()).unwrap_or("");
        let lifetime = SIGN_IN_LINK_LIFETIME_SECONDS;

        let client_ip = extract_client_ip(&addr, &headers);
        let client_ip_str = client_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| addr.ip().to_string());
        let rate_ok = state
            .sign_in_link_rate_limiter
            .check_and_record(email, &client_ip_str);

        let acl = &state.settings.server.acl;
        let maintainers = crate::maintainers::get_global_maintainers();
        let is_eligible = is_sign_in_eligible(email, acl, maintainers.as_deref());

        if is_eligible && rate_ok {
            let token = crate::auth::create_token(
                email,
                &secret,
                Some("sign_in_link".to_string()),
                lifetime.max(0) as u64,
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let base_url = state.settings.server.sign_in_base_url();
            let link = format!("{}/auth/verify?token={}", base_url, token);

            if state.settings.smtp.is_some() {
                let body = build_sign_in_link_email(email, &link, &base_url, lifetime, client_ip);
                let status = match &state.settings.smtp {
                    Some(s) if s.dry_run => "Dry-Run",
                    _ => "Pending",
                };
                state
                    .db
                    .insert_transactional_email(
                        crate::db::EmailKind::SignInLink,
                        status,
                        email,
                        "[sashiko] Your sign-in link",
                        &body,
                    )
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to queue sign-in email: {}", e);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
                tracing::info!("Queued sign-in link email for {}", email);
            } else if state.settings.server.log_sign_in_links {
                tracing::info!("SIGN-IN LINK REQUESTED for {}: {}", email, link);
            } else {
                // The link is a bearer credential, so it is not written to the
                // log by default. Without a transport there is nothing else to
                // do with it, which is worth saying plainly rather than
                // leaving the request looking like it succeeded.
                tracing::warn!(
                    "Sign-in link requested for {} but no SMTP transport is configured, \
                     so it could not be delivered. Set server.log_sign_in_links to print \
                     it to the log instead.",
                    email
                );
            }
        } else {
            if !rate_ok {
                tracing::warn!(
                    "Sign-in link request rate limit exceeded for address '{}' from IP '{}'",
                    email,
                    client_ip_str
                );
            }
            // Timing equalization: simulate token creation and DB roundtrip
            let _ = crate::auth::create_token(
                "timing-equalizer@sashiko.internal",
                &secret,
                Some("sign_in_link".to_string()),
                lifetime.max(0) as u64,
            );
            if state.settings.smtp.is_some() {
                let _ = state.db.conn.query("SELECT 1", ()).await;
            }
            tracing::debug!(
                "Ignored sign-in link request for ineligible or rate-limited address: {}",
                email
            );
        }

        Ok(StatusCode::OK)
    } else {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

#[derive(serde::Deserialize)]
struct VerifyLinkQuery {
    token: String,
}

async fn verify_link(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<VerifyLinkQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        let claims = crate::auth::verify_token(&query.token, &secret)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid sign-in link"))?;
        if claims.typ.as_deref() != Some("sign_in_link") {
            return Err((StatusCode::UNAUTHORIZED, "Invalid token type"));
        }
        if state.settings.server.acl.is_blocklisted(&claims.sub) {
            return Err((StatusCode::FORBIDDEN, "User is blocklisted"));
        }
        let session_token =
            crate::auth::create_token(&claims.sub, &secret, Some("session".to_string()), 86400)
                .map_err(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to create session",
                    )
                })?;
        Ok(Json(serde_json::json!({ "token": session_token })))
    } else {
        Err((StatusCode::NOT_IMPLEMENTED, "JWT not configured"))
    }
}

async fn refresh_token(
    auth: crate::auth::OptionalAuthUser,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        if let Some(user) = auth.0 {
            if state.settings.server.acl.is_blocklisted(&user.email) {
                return Err((StatusCode::FORBIDDEN, "User is blocklisted"));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs() as usize;

            let max_session_lifetime = 30 * 24 * 3600; // 30 days
            if let Some(iat) = user.iat
                && now.saturating_sub(iat) > max_session_lifetime
            {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "Session has reached maximum lifetime (30 days). Please log in again.",
                ));
            }

            let session_token = crate::auth::create_token_with_session(
                &user.email,
                &secret,
                Some("session".to_string()),
                86400,
                user.iat,
                user.sid,
            )
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create session",
                )
            })?;
            Ok(Json(serde_json::json!({ "token": session_token })))
        } else {
            Err((StatusCode::UNAUTHORIZED, "Missing or invalid token"))
        }
    } else {
        Err((StatusCode::NOT_IMPLEMENTED, "JWT not configured"))
    }
}

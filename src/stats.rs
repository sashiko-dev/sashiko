use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::sync::OnceLock;

pub fn global_registry() -> Arc<StatsRegistry> {
    static REGISTRY: OnceLock<Arc<StatsRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| StatsRegistry::new()).clone()
}

#[derive(Default)]
pub struct StatsRegistry {
    // Gauges (absolute values)
    pub queue_pending_patches: AtomicI64,
    pub queue_in_progress_patches: AtomicI64,

    // Counters (deltas)
    pub patches_ingested_total: AtomicU64,
    pub patches_reviewed_total: AtomicU64,
    pub review_failures_total: AtomicU64,

    // TTR sum and count for average
    pub time_to_review_sum_seconds: AtomicU64,
    pub time_to_review_count: AtomicU64,

    // Labeled counters
    // key: metric_name, sub-key: label -> value
    pub labeled_counters: Mutex<HashMap<String, HashMap<String, u64>>>,
}

impl StatsRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn inc_gauge(&self, metric: &str, amount: i64) {
        match metric {
            "sashiko_queue_pending_patches" => {
                self.queue_pending_patches
                    .fetch_add(amount, Ordering::Relaxed);
            }
            "sashiko_queue_in_progress_patches" => {
                self.queue_in_progress_patches
                    .fetch_add(amount, Ordering::Relaxed);
            }
            _ => (),
        }
    }

    pub fn dec_gauge(&self, metric: &str, amount: i64) {
        self.inc_gauge(metric, -amount);
    }

    pub fn set_gauge(&self, metric: &str, value: i64) {
        match metric {
            "sashiko_queue_pending_patches" => {
                self.queue_pending_patches.store(value, Ordering::Relaxed)
            }
            "sashiko_queue_in_progress_patches" => self
                .queue_in_progress_patches
                .store(value, Ordering::Relaxed),
            _ => (),
        }
    }

    pub fn inc_counter(&self, metric: &str) {
        match metric {
            "sashiko_patches_ingested_total" => {
                self.patches_ingested_total.fetch_add(1, Ordering::Relaxed);
            }
            "sashiko_patches_reviewed_total" => {
                self.patches_reviewed_total.fetch_add(1, Ordering::Relaxed);
            }
            "sashiko_review_failures_total" => {
                self.review_failures_total.fetch_add(1, Ordering::Relaxed);
            }
            _ => (),
        }
    }

    pub fn record_latency(&self, metric: &str, seconds: u64) {
        if metric == "sashiko_time_to_review_seconds" {
            self.time_to_review_sum_seconds
                .fetch_add(seconds, Ordering::Relaxed);
            self.time_to_review_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn inc_labeled_counter(&self, metric: &str, label: &str, amount: u64) {
        let mut outer = self.labeled_counters.lock().unwrap();
        let inner = outer.entry(metric.to_string()).or_default();
        *inner.entry(label.to_string()).or_insert(0) += amount;
    }

    /// Flushes all deltas, resetting counters to 0, and returns the snapshot.
    pub fn flush(&self) -> RegistrySnapshot {
        let mut snapshot = RegistrySnapshot {
            gauges: HashMap::new(),
            counters: HashMap::new(),
            labeled_counters: HashMap::new(),
        };

        // Read gauges
        snapshot.gauges.insert(
            "sashiko_queue_pending_patches".to_string(),
            self.queue_pending_patches.load(Ordering::Relaxed),
        );
        snapshot.gauges.insert(
            "sashiko_queue_in_progress_patches".to_string(),
            self.queue_in_progress_patches.load(Ordering::Relaxed),
        );

        // Swap counters with 0 to get deltas
        snapshot.counters.insert(
            "sashiko_patches_ingested_total".to_string(),
            self.patches_ingested_total.swap(0, Ordering::Relaxed),
        );
        snapshot.counters.insert(
            "sashiko_patches_reviewed_total".to_string(),
            self.patches_reviewed_total.swap(0, Ordering::Relaxed),
        );
        snapshot.counters.insert(
            "sashiko_review_failures_total".to_string(),
            self.review_failures_total.swap(0, Ordering::Relaxed),
        );
        snapshot.counters.insert(
            "sashiko_time_to_review_sum_seconds".to_string(),
            self.time_to_review_sum_seconds.swap(0, Ordering::Relaxed),
        );
        snapshot.counters.insert(
            "sashiko_time_to_review_count".to_string(),
            self.time_to_review_count.swap(0, Ordering::Relaxed),
        );

        // Take labeled counters
        {
            let mut outer = self.labeled_counters.lock().unwrap();
            snapshot.labeled_counters = std::mem::take(&mut *outer);
        }

        snapshot
    }
}

pub struct RegistrySnapshot {
    pub gauges: HashMap<String, i64>,
    pub counters: HashMap<String, u64>,
    pub labeled_counters: HashMap<String, HashMap<String, u64>>,
}

use crate::db::Database;
use chrono::{DateTime, Timelike, Utc};
use std::time::SystemTime;

pub async fn start_flusher(registry: Arc<StatsRegistry>, db: Arc<Database>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        flush_to_db(&registry, &db).await;
    }
}

pub async fn flush_to_db(registry: &StatsRegistry, db: &Database) {
    let snapshot = registry.flush();
    let now: DateTime<Utc> = SystemTime::now().into();

    // For gauges, we just upsert the current value.
    for (metric, value) in snapshot.gauges {
        let _ = db.upsert_stat_gauge(&metric, value).await;
    }

    // For timeseries, we floor the current time to the hour.
    let bucket_time = now
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let bucket_str = bucket_time.format("%Y-%m-%d %H:%M:%S").to_string();

    // Insert counters into timeseries
    for (metric, value) in snapshot.counters {
        if value > 0 {
            let _ = db
                .inc_stat_timeseries(&bucket_str, &metric, "none", value)
                .await;
        }
    }

    // Insert labeled counters
    for (metric, labels) in snapshot.labeled_counters {
        for (label, value) in labels {
            if value > 0 {
                let _ = db
                    .inc_stat_timeseries(&bucket_str, &metric, &label, value)
                    .await;
            }
        }
    }
}

use crate::events::{StatEvent, stat_events};

pub async fn start_stat_listener(registry: Arc<StatsRegistry>) {
    let mut rx = stat_events().subscribe();
    while let Ok(event) = rx.recv().await {
        match event {
            StatEvent::PatchIngested => {
                registry.inc_counter("sashiko_patches_ingested_total");
            }
            StatEvent::PatchReviewed {
                success,
                latency_secs,
            } => {
                if success {
                    registry.inc_counter("sashiko_patches_reviewed_total");
                    registry.record_latency("sashiko_time_to_review_seconds", latency_secs);
                } else {
                    registry.inc_counter("sashiko_review_failures_total");
                }
            }
            StatEvent::ReviewFinding { severity } => {
                registry.inc_labeled_counter("sashiko_findings_total", &severity, 1);
            }
            StatEvent::AiTokens {
                model,
                token_type,
                amount,
            } => {
                // Not the most robust serialization, but it fits the schema "label" column.
                let label = format!("model={},type={}", model, token_type);
                registry.inc_labeled_counter("sashiko_tokens_total", &label, amount);
            }
            StatEvent::ToolUsage { tool } => {
                registry.inc_labeled_counter("sashiko_tool_usage_total", &tool, 1);
            }
        }
    }
}

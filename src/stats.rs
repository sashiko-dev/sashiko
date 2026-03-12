use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
            "sashiko_queue_pending_patches" => { self.queue_pending_patches.fetch_add(amount, Ordering::Relaxed); }
            "sashiko_queue_in_progress_patches" => { self.queue_in_progress_patches.fetch_add(amount, Ordering::Relaxed); }
            _ => (),
        }
    }

    pub fn dec_gauge(&self, metric: &str, amount: i64) {
        self.inc_gauge(metric, -amount);
    }
    
    pub fn set_gauge(&self, metric: &str, value: i64) {
        match metric {
            "sashiko_queue_pending_patches" => self.queue_pending_patches.store(value, Ordering::Relaxed),
            "sashiko_queue_in_progress_patches" => self.queue_in_progress_patches.store(value, Ordering::Relaxed),
            _ => (),
        }
    }

    pub fn inc_counter(&self, metric: &str) {
        match metric {
            "sashiko_patches_ingested_total" => { self.patches_ingested_total.fetch_add(1, Ordering::Relaxed); }
            "sashiko_patches_reviewed_total" => { self.patches_reviewed_total.fetch_add(1, Ordering::Relaxed); }
            "sashiko_review_failures_total" => { self.review_failures_total.fetch_add(1, Ordering::Relaxed); }
            _ => (),
        }
    }

    pub fn record_latency(&self, metric: &str, seconds: u64) {
        if metric == "sashiko_time_to_review_seconds" {
            self.time_to_review_sum_seconds.fetch_add(seconds, Ordering::Relaxed);
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
        snapshot.gauges.insert("sashiko_queue_pending_patches".to_string(), self.queue_pending_patches.load(Ordering::Relaxed));
        snapshot.gauges.insert("sashiko_queue_in_progress_patches".to_string(), self.queue_in_progress_patches.load(Ordering::Relaxed));

        // Swap counters with 0 to get deltas
        snapshot.counters.insert("sashiko_patches_ingested_total".to_string(), self.patches_ingested_total.swap(0, Ordering::Relaxed));
        snapshot.counters.insert("sashiko_patches_reviewed_total".to_string(), self.patches_reviewed_total.swap(0, Ordering::Relaxed));
        snapshot.counters.insert("sashiko_review_failures_total".to_string(), self.review_failures_total.swap(0, Ordering::Relaxed));
        snapshot.counters.insert("sashiko_time_to_review_sum_seconds".to_string(), self.time_to_review_sum_seconds.swap(0, Ordering::Relaxed));
        snapshot.counters.insert("sashiko_time_to_review_count".to_string(), self.time_to_review_count.swap(0, Ordering::Relaxed));

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

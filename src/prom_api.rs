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

//! Prometheus text `0.0.4` exposition (`GET /metrics`) and Prometheus HTTP
//! Query API v1 (`/api/v1/query`, `/api/v1/query_range`, `/api/v1/labels`,
//! `/api/v1/label/{name}/values`, `/api/v1/metadata`) backed exclusively by
//! O(1) SQLite rollups, state gauges, pipeline summaries, and runtime atomics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::error;

use crate::access::{OptionalPrincipal, Principal};
use crate::db::{BugSubsystemFacetSummary, PipelineMultiWindowStats, RollupBucketRow};
use crate::metrics::{
    METRIC_CATALOG, MetricDef, MetricType, render_runtime_prometheus, unflushed_runtime_deltas,
    write_prometheus_header, write_prometheus_sample,
};
use crate::promql::{
    EvalSample, EvalValue, Expr, LabelMatcher, Labels, MAX_RANGE_POINTS, MIN_STEP_SECS,
    MatrixSeries, PromDataSource, PromError, RangeFunc, VectorSample, VectorSelector, eval_instant,
    format_prom_float, parse_prom_duration_secs, parse_prom_timestamp, parse_promql,
};
use crate::server::{AppState, presents_local_token};

pub static DB_METRIC_CATALOG: &[MetricDef] = &[
    MetricDef {
        name: "sashiko_reviews_stored",
        metric_type: MetricType::Gauge,
        help: "Current reviews stored in the database grouped by status.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_reviews_inherited_stored",
        metric_type: MetricType::Gauge,
        help: "Total stored reviews that inherited a clean verdict from an ancestor patchset.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_outbox_depth",
        metric_type: MetricType::Gauge,
        help: "Current outbound delivery queue items grouped by channel and status.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_fetch_queue_depth",
        metric_type: MetricType::Gauge,
        help: "Current ingestion fetch queue items grouped by status.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_bugs_total",
        metric_type: MetricType::Gauge,
        help: "Tracked bugs grouped by lifecycle status, pipeline state, engineering severity, and CVSS v4.0 security rating/class.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_bugs_with_attachment",
        metric_type: MetricType::Gauge,
        help: "Distinct bugs carrying a reproducer or proposed-fix attachment grouped by lifecycle status.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_bugs_by_subsystem",
        metric_type: MetricType::Gauge,
        help: "Access-controlled bug facet counts per MAINTAINERS subsystem.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_bugs_events_total",
        metric_type: MetricType::Counter,
        help: "Total verified bug lifecycle events (discovered, fixed, duplicate, dismissed).",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patchsets_ingested_by_version_total",
        metric_type: MetricType::Counter,
        help: "Total ingested patchsets grouped by series revision bucket.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patches_ingested_by_lineage_total",
        metric_type: MetricType::Counter,
        help: "Total completed patches grouped by ancestor diff lineage (new, modified, unchanged).",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patchsets_completed_total",
        metric_type: MetricType::Counter,
        help: "Total completed non-embargoed patchsets grouped by status and clean verdict.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_findings_discovered_total",
        metric_type: MetricType::Counter,
        help: "Total review findings on non-embargoed patchsets grouped by severity and preexisting flag.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_prior_feedback_total",
        metric_type: MetricType::Counter,
        help: "Total prior-version feedback resolutions grouped by reviewer origin and outcome.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_subsystem_patchsets_total",
        metric_type: MetricType::Counter,
        help: "Total ingested patchsets attributed to each mailing-list subsystem.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_subsystem_patches_total",
        metric_type: MetricType::Counter,
        help: "Total ingested patches attributed to each mailing-list subsystem.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_subsystem_findings_total",
        metric_type: MetricType::Counter,
        help: "Total patch-introduced findings on non-embargoed patchsets per subsystem and severity.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_stats_backfill_complete",
        metric_type: MetricType::Gauge,
        help: "1 when asynchronous historical statistics rollup backfill has completed, else 0.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_project_release_timestamp_seconds",
        metric_type: MetricType::Gauge,
        help: "UNIX timestamp of recent upstream project release tags from the configured git repository.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_pipeline_window_reviews",
        metric_type: MetricType::Gauge,
        help: "Number of completed non-inherited reviews aggregated in each N-review window.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_duration_seconds_sum",
        metric_type: MetricType::Gauge,
        help: "Sum of review wall-clock durations in seconds across the last N reviews.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_pipeline_tokens_sum",
        metric_type: MetricType::Gauge,
        help: "Sum of LLM tokens by direction across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_funnel_items",
        metric_type: MetricType::Gauge,
        help: "Item count at each stage of the concern-to-finding verification and dismissal funnel across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_stage_runs",
        metric_type: MetricType::Gauge,
        help: "Executed vs skipped stage invocations across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_stage_turns_sum",
        metric_type: MetricType::Gauge,
        help: "Sum of LLM conversation turns per analysis stage across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_stage_tokens_sum",
        metric_type: MetricType::Gauge,
        help: "Sum of LLM tokens per analysis stage and direction across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_stage_items",
        metric_type: MetricType::Gauge,
        help: "Stage discovery, exclusive finding, and dismissal counts across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_stage_overlap",
        metric_type: MetricType::Gauge,
        help: "Pairwise confirmed-issue co-discovery count between stages across the last N reviews (diagonal holds exclusive count).",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_tool_calls",
        metric_type: MetricType::Gauge,
        help: "Tool invocation count per tool across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_tool_output_bytes_sum",
        metric_type: MetricType::Gauge,
        help: "Total tool output bytes returned per tool across the last N reviews.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_pipeline_prior_alignment",
        metric_type: MetricType::Gauge,
        help: "Prior-feedback fixup and human-reviewer recall counts across the last N reviews.",
        unit: "",
    },
];

/// Constant-time check for `server.metrics_token` using SHA-256 digests so
/// neither secret contents nor secret length leaks via response timing.
pub(crate) fn presents_metrics_token(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(configured) = state
        .settings
        .server
        .metrics_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };

    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(raw) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }

    let Some(presented) = raw
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };

    let expected = Sha256::digest(configured.as_bytes());
    let actual = Sha256::digest(presented.as_bytes());
    expected.ct_eq(&actual).into()
}

pub async fn prometheus_metrics(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    let token_required = state
        .settings
        .server
        .metrics_token
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    if token_required
        && !presents_metrics_token(&headers, &state)
        && !presents_local_token(&headers, &state)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Unauthorized",
        )
            .into_response();
    }

    let now_ts = chrono::Utc::now().timestamp();
    if let Some(ref hb) = state.email_worker_heartbeat {
        let last = hb.load(std::sync::atomic::Ordering::Relaxed);
        crate::metrics::set_email_worker_heartbeat_age_secs(now_ts.saturating_sub(last).max(0));
    }
    let mut buf = String::with_capacity(32 * 1024);
    render_runtime_prometheus(&mut buf);

    match state.db.get_all_state_gauges().await {
        Ok(gauges) => append_db_state_gauges(&mut buf, &gauges),
        Err(e) => error!("Failed reading state gauges for /metrics: {e}"),
    }
    let backfill_done = state.db.is_stats_backfill_done().await.unwrap_or(false);
    write_prometheus_header(
        &mut buf,
        "sashiko_stats_backfill_complete",
        MetricType::Gauge,
        "1 when asynchronous historical statistics rollup backfill has completed, else 0.",
    );
    write_prometheus_sample(
        &mut buf,
        "sashiko_stats_backfill_complete",
        &[],
        if backfill_done { 1.0 } else { 0.0 },
    );

    match state.get_cached_pipeline_stats(now_ts).await {
        Ok(pipe) => append_pipeline_prometheus(&mut buf, &pipe),
        Err(e) => error!("Failed reading pipeline stats for /metrics: {e}"),
    }

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        buf,
    )
        .into_response()
}

fn append_db_state_gauges(buf: &mut String, gauges: &[(String, Labels, i64)]) {
    write_prometheus_header(
        buf,
        "sashiko_queue_depth",
        MetricType::Gauge,
        "Current item count in Sashiko processing queues.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_queue_depth",
        &[("queue", "patches_pending")],
        crate::metrics::get_pending_patches() as f64,
    );
    write_prometheus_sample(
        buf,
        "sashiko_queue_depth",
        &[("queue", "patches_reviewing")],
        crate::metrics::get_reviewing_patches() as f64,
    );

    let mut grouped: BTreeMap<&str, Vec<(&Labels, i64)>> = BTreeMap::new();
    for (m, l, v) in gauges {
        grouped.entry(m.as_str()).or_default().push((l, *v));
    }

    for (metric, items) in grouped {
        let help = DB_METRIC_CATALOG
            .iter()
            .chain(METRIC_CATALOG.iter())
            .find(|d| d.name == metric)
            .map_or("Sashiko database state gauge.", |d| d.help);
        write_prometheus_header(buf, metric, MetricType::Gauge, help);
        for (labels, val) in items {
            let pairs: Vec<(&str, &str)> = labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            write_prometheus_sample(buf, metric, &pairs, val as f64);
        }
    }
}

fn append_pipeline_prometheus(buf: &mut String, multi: &PipelineMultiWindowStats) {
    let samples = build_pipeline_samples(multi);
    for (metric, rows) in samples {
        let help = DB_METRIC_CATALOG
            .iter()
            .find(|d| d.name == metric)
            .map_or("Sashiko review pipeline summary metric.", |d| d.help);
        write_prometheus_header(buf, metric, MetricType::Gauge, help);
        for s in rows {
            let pairs: Vec<(&str, &str)> = s
                .labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            write_prometheus_sample(buf, metric, &pairs, s.value);
        }
    }
}

fn build_pipeline_samples(
    multi: &PipelineMultiWindowStats,
) -> BTreeMap<&'static str, Vec<EvalSample>> {
    let mut out: BTreeMap<&'static str, Vec<EvalSample>> = BTreeMap::new();
    for w in &multi.windows {
        let ws = w.window.to_string();
        let mk = |extra: &[(&str, &str)]| -> Labels {
            let mut m = Labels::new();
            m.insert("window".to_string(), ws.clone());
            for (k, v) in extra {
                m.insert((*k).to_string(), (*v).to_string());
            }
            m
        };
        out.entry("sashiko_pipeline_window_reviews")
            .or_default()
            .push(EvalSample {
                labels: mk(&[]),
                value: w.reviews_count as f64,
            });
        out.entry("sashiko_pipeline_duration_seconds_sum")
            .or_default()
            .push(EvalSample {
                labels: mk(&[]),
                value: w.duration_secs_sum as f64,
            });

        let uncached_in = w.tokens_in_sum.saturating_sub(w.tokens_cached_sum);
        for (dir, v) in [
            ("uncached_in", uncached_in),
            ("cached_in", w.tokens_cached_sum),
            ("out", w.tokens_out_sum),
        ] {
            out.entry("sashiko_pipeline_tokens_sum")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("direction", dir)]),
                    value: v as f64,
                });
        }

        for (step, v) in [
            ("raw_concerns", w.raw_concerns),
            ("stage1_dismissed", w.stage1_dismissed),
            ("deduplicated_clusters", w.deduplicated_clusters),
            ("verified_1a", w.verified_1a),
            ("dismissed_1b", w.dismissed_1b),
            ("inconclusive", w.inconclusive),
            ("verified_pv", w.verified_pv),
            ("dismissed_pv", w.dismissed_pv),
            ("findings_patch", w.findings_patch),
            ("findings_preexisting", w.findings_preexisting),
        ] {
            out.entry("sashiko_pipeline_funnel_items")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("step", step)]),
                    value: v as f64,
                });
        }

        for st in w.stages.values() {
            let sn = st.stage.as_str();
            for (oc, cnt) in [("executed", st.executed), ("skipped", st.skipped)] {
                out.entry("sashiko_pipeline_stage_runs")
                    .or_default()
                    .push(EvalSample {
                        labels: mk(&[("outcome", oc), ("stage", sn)]),
                        value: cnt as f64,
                    });
            }
            out.entry("sashiko_pipeline_stage_turns_sum")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("stage", sn)]),
                    value: st.turns as f64,
                });
            let st_uncached = st.tokens_in.saturating_sub(st.tokens_cached);
            for (dir, v) in [
                ("uncached_in", st_uncached),
                ("cached_in", st.tokens_cached),
                ("out", st.tokens_out),
            ] {
                out.entry("sashiko_pipeline_stage_tokens_sum")
                    .or_default()
                    .push(EvalSample {
                        labels: mk(&[("direction", dir), ("stage", sn)]),
                        value: v as f64,
                    });
            }
            for (kind, v) in [
                ("raw_concerns", st.raw_concerns),
                ("stage1_dismissed", st.stage1_dismissed),
                ("confirmed_findings", st.confirmed_findings),
                ("exclusive_findings", st.exclusive_findings),
                ("dismissed_in_verify", st.dismissed_in_verify),
            ] {
                out.entry("sashiko_pipeline_stage_items")
                    .or_default()
                    .push(EvalSample {
                        labels: mk(&[("kind", kind), ("stage", sn)]),
                        value: v as f64,
                    });
            }
        }

        for ((sa, sb), cnt) in &w.overlap {
            out.entry("sashiko_pipeline_stage_overlap")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("stage_a", sa.as_str()), ("stage_b", sb.as_str())]),
                    value: *cnt as f64,
                });
        }

        for (tool, (calls, bytes)) in &w.tools {
            out.entry("sashiko_pipeline_tool_calls")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("tool", tool.as_str())]),
                    value: *calls as f64,
                });
            out.entry("sashiko_pipeline_tool_output_bytes_sum")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("tool", tool.as_str())]),
                    value: *bytes as f64,
                });
        }

        for (m_name, cnt) in [
            ("pf_addressed", w.prior.pf_addressed),
            ("pf_unaddressed", w.prior.pf_unaddressed),
            ("hm_addressed", w.prior.hm_addressed),
            ("hm_unaddressed", w.prior.hm_unaddressed),
            ("hm_also_found", w.prior.hm_also_found),
        ] {
            out.entry("sashiko_pipeline_prior_alignment")
                .or_default()
                .push(EvalSample {
                    labels: mk(&[("metric", m_name)]),
                    value: cnt as f64,
                });
        }
    }
    out
}

// ============================================================================
// Prometheus HTTP API v1 Handlers
// ============================================================================

#[derive(Debug, Default, Deserialize)]
pub struct InstantQueryParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub time: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RangeQueryParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub step: String,
}

fn prom_err_response(err: PromError) -> Response {
    let code = match err {
        PromError::BadData(_) => StatusCode::BAD_REQUEST,
        PromError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        code,
        Json(json!({
            "status": "error",
            "errorType": err.error_type(),
            "error": err.message(),
        })),
    )
        .into_response()
}

fn merge_instant_params(
    method: &Method,
    mut params: InstantQueryParams,
    body: &Bytes,
) -> InstantQueryParams {
    if *method == Method::POST && !body.is_empty() {
        for (k, v) in url::form_urlencoded::parse(body) {
            match k.as_ref() {
                "query" => params.query = v.into_owned(),
                "time" => params.time = v.into_owned(),
                _ => {}
            }
        }
    }
    params
}

fn merge_range_params(
    method: &Method,
    mut params: RangeQueryParams,
    body: &Bytes,
) -> RangeQueryParams {
    if *method == Method::POST && !body.is_empty() {
        for (k, v) in url::form_urlencoded::parse(body) {
            match k.as_ref() {
                "query" => params.query = v.into_owned(),
                "start" => params.start = v.into_owned(),
                "end" => params.end = v.into_owned(),
                "step" => params.step = v.into_owned(),
                _ => {}
            }
        }
    }
    params
}

pub async fn prom_query(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    method: Method,
    Query(url_params): Query<InstantQueryParams>,
    body: Bytes,
) -> Response {
    let params = merge_instant_params(&method, url_params, &body);
    let now_ts = chrono::Utc::now().timestamp();
    let eval_ts = match parse_prom_timestamp(&params.time, now_ts) {
        Ok(ts) => ts,
        Err(e) => return prom_err_response(e),
    };
    let expr = match parse_promql(&params.query) {
        Ok(e) => e,
        Err(e) => return prom_err_response(e),
    };

    let ds = match preload_data_source(&state, &principal, &expr, eval_ts, eval_ts, now_ts).await {
        Ok(d) => d,
        Err(e) => return prom_err_response(e),
    };

    match eval_instant(&expr, &ds, eval_ts) {
        Ok(EvalValue::Scalar(v)) => Json(json!({
            "status": "success",
            "data": {
                "resultType": "scalar",
                "result": [eval_ts as f64, format_prom_float(v)],
            }
        }))
        .into_response(),
        Ok(EvalValue::Vector(samples)) => {
            let result: Vec<VectorSample> = samples
                .into_iter()
                .map(|s| VectorSample {
                    metric: s.labels,
                    value: (eval_ts as f64, format_prom_float(s.value)),
                })
                .collect();
            Json(json!({
                "status": "success",
                "data": {
                    "resultType": "vector",
                    "result": result,
                }
            }))
            .into_response()
        }
        Err(e) => prom_err_response(e),
    }
}

pub async fn prom_query_range(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    method: Method,
    Query(url_params): Query<RangeQueryParams>,
    body: Bytes,
) -> Response {
    let params = merge_range_params(&method, url_params, &body);
    let now_ts = chrono::Utc::now().timestamp();
    if params.start.trim().is_empty()
        || params.end.trim().is_empty()
        || params.step.trim().is_empty()
    {
        return prom_err_response(PromError::bad_data(
            "start, end, and step are required for query_range",
        ));
    }
    let start_ts = match parse_prom_timestamp(&params.start, now_ts) {
        Ok(t) => t,
        Err(e) => return prom_err_response(e),
    };
    let end_ts = match parse_prom_timestamp(&params.end, now_ts) {
        Ok(t) => t,
        Err(e) => return prom_err_response(e),
    };
    if end_ts < start_ts {
        return prom_err_response(PromError::bad_data("end timestamp must not precede start"));
    }
    let raw_step = match parse_prom_duration_secs(&params.step) {
        Ok(s) => s,
        Err(e) => return prom_err_response(e),
    };
    let step_secs = raw_step.max(MIN_STEP_SECS);
    let Some(span_secs) = end_ts.checked_sub(start_ts) else {
        return prom_err_response(PromError::bad_data("query_range span overflow"));
    };
    let num_points = (span_secs / step_secs).saturating_add(1);
    if num_points as usize > MAX_RANGE_POINTS {
        return prom_err_response(PromError::bad_data(format!(
            "query_range exceeds maximum {} points; increase step",
            MAX_RANGE_POINTS
        )));
    }

    let expr = match parse_promql(&params.query) {
        Ok(e) => e,
        Err(e) => return prom_err_response(e),
    };

    let ds = match preload_data_source(&state, &principal, &expr, start_ts, end_ts, now_ts).await {
        Ok(d) => d,
        Err(e) => return prom_err_response(e),
    };

    let mut series_map: BTreeMap<Labels, Vec<(f64, String)>> = BTreeMap::new();
    let mut t = start_ts;
    while t <= end_ts {
        match eval_instant(&expr, &ds, t) {
            Ok(EvalValue::Scalar(v)) => {
                if !v.is_nan() {
                    series_map
                        .entry(Labels::new())
                        .or_default()
                        .push((t as f64, format_prom_float(v)));
                }
            }
            Ok(EvalValue::Vector(samples)) => {
                for s in samples {
                    if !s.value.is_nan() {
                        series_map
                            .entry(s.labels)
                            .or_default()
                            .push((t as f64, format_prom_float(s.value)));
                    }
                }
            }
            Err(e) => return prom_err_response(e),
        }
        let Some(next_t) = t.checked_add(step_secs) else {
            break;
        };
        t = next_t;
    }

    let result: Vec<MatrixSeries> = series_map
        .into_iter()
        .map(|(metric, values)| MatrixSeries { metric, values })
        .collect();

    Json(json!({
        "status": "success",
        "data": {
            "resultType": "matrix",
            "result": result,
        }
    }))
    .into_response()
}

pub const MAX_BATCH_QUERIES: usize = 64;

#[derive(Debug, Deserialize)]
pub struct BatchQueryItem {
    pub id: String,
    pub query: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub step: String,
}

#[derive(Debug, Deserialize)]
pub struct BatchQueryRequest {
    pub queries: Vec<BatchQueryItem>,
}

/// Executes up to `MAX_BATCH_QUERIES` PromQL instant and range queries in a
/// single HTTP round-trip so the built-in dashboard never fans out dozens of
/// concurrent HTTP/1.1 requests.
pub async fn prom_query_batch(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchQueryRequest>,
) -> Response {
    if req.queries.is_empty() || req.queries.len() > MAX_BATCH_QUERIES {
        return prom_err_response(PromError::bad_data(format!(
            "batch must contain between 1 and {MAX_BATCH_QUERIES} queries"
        )));
    }
    let now_ts = chrono::Utc::now().timestamp();
    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    for item in req.queries {
        let is_range = !item.start.trim().is_empty()
            || !item.end.trim().is_empty()
            || !item.step.trim().is_empty();
        let val = if is_range {
            match eval_range_item(&state, &principal, &item, now_ts).await {
                Ok(v) => v,
                Err(e) => return prom_err_response(e),
            }
        } else {
            match eval_instant_item(&state, &principal, &item, now_ts).await {
                Ok(v) => v,
                Err(e) => return prom_err_response(e),
            }
        };
        results.insert(item.id, val);
    }

    Json(json!({
        "status": "success",
        "data": results,
    }))
    .into_response()
}

async fn eval_instant_item(
    state: &AppState,
    principal: &Principal,
    item: &BatchQueryItem,
    now_ts: i64,
) -> Result<Value, PromError> {
    let eval_ts = parse_prom_timestamp(&item.time, now_ts)?;
    let expr = parse_promql(&item.query)?;
    let ds = preload_data_source(state, principal, &expr, eval_ts, eval_ts, now_ts).await?;
    match eval_instant(&expr, &ds, eval_ts)? {
        EvalValue::Scalar(v) => Ok(json!({
            "resultType": "scalar",
            "result": [eval_ts as f64, format_prom_float(v)],
        })),
        EvalValue::Vector(samples) => {
            let result: Vec<VectorSample> = samples
                .into_iter()
                .map(|s| VectorSample {
                    metric: s.labels,
                    value: (eval_ts as f64, format_prom_float(s.value)),
                })
                .collect();
            Ok(json!({
                "resultType": "vector",
                "result": result,
            }))
        }
    }
}

async fn eval_range_item(
    state: &AppState,
    principal: &Principal,
    item: &BatchQueryItem,
    now_ts: i64,
) -> Result<Value, PromError> {
    if item.start.trim().is_empty() || item.end.trim().is_empty() || item.step.trim().is_empty() {
        return Err(PromError::bad_data(
            "start, end, and step are required for range queries",
        ));
    }
    let start_ts = parse_prom_timestamp(&item.start, now_ts)?;
    let end_ts = parse_prom_timestamp(&item.end, now_ts)?;
    if end_ts < start_ts {
        return Err(PromError::bad_data("end timestamp must not precede start"));
    }
    let step_secs = parse_prom_duration_secs(&item.step)?.max(MIN_STEP_SECS);
    let span_secs = end_ts
        .checked_sub(start_ts)
        .ok_or_else(|| PromError::bad_data("query_range span overflow"))?;
    let num_points = (span_secs / step_secs).saturating_add(1);
    if num_points as usize > MAX_RANGE_POINTS {
        return Err(PromError::bad_data(format!(
            "query_range exceeds maximum {} points; increase step",
            MAX_RANGE_POINTS
        )));
    }
    let expr = parse_promql(&item.query)?;
    let ds = preload_data_source(state, principal, &expr, start_ts, end_ts, now_ts).await?;
    let mut series_map: BTreeMap<Labels, Vec<(f64, String)>> = BTreeMap::new();
    let mut t = start_ts;
    while t <= end_ts {
        match eval_instant(&expr, &ds, t)? {
            EvalValue::Scalar(v) => {
                if !v.is_nan() {
                    series_map
                        .entry(Labels::new())
                        .or_default()
                        .push((t as f64, format_prom_float(v)));
                }
            }
            EvalValue::Vector(samples) => {
                for s in samples {
                    if !s.value.is_nan() {
                        series_map
                            .entry(s.labels)
                            .or_default()
                            .push((t as f64, format_prom_float(s.value)));
                    }
                }
            }
        }
        let Some(next_t) = t.checked_add(step_secs) else {
            break;
        };
        t = next_t;
    }
    let result: Vec<MatrixSeries> = series_map
        .into_iter()
        .map(|(metric, values)| MatrixSeries { metric, values })
        .collect();
    Ok(json!({
        "resultType": "matrix",
        "result": result,
    }))
}

pub async fn prom_labels() -> Json<Value> {
    let labels = [
        "__name__",
        "agg",
        "attachment",
        "channel",
        "class",
        "clean",
        "component",
        "direction",
        "event",
        "facet",
        "kind",
        "le",
        "level",
        "lineage",
        "metric",
        "op",
        "origin_kind",
        "outcome",
        "pipeline",
        "preexisting",
        "queue",
        "reason",
        "remote",
        "security_class",
        "security_rating",
        "severity",
        "source",
        "stage",
        "stage_a",
        "stage_b",
        "state",
        "status",
        "step",
        "subsystem",
        "tag",
        "tool",
        "url",
        "version",
        "version_bucket",
        "window",
        "worker",
        "workload",
    ];
    Json(json!({
        "status": "success",
        "data": &labels[..],
    }))
}

pub async fn prom_label_values(
    OptionalPrincipal(principal): OptionalPrincipal,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let mut vals = BTreeSet::new();
    match name.as_str() {
        "__name__" => {
            for m in METRIC_CATALOG.iter().chain(DB_METRIC_CATALOG.iter()) {
                vals.insert(m.name.to_string());
            }
        }
        "subsystem" => {
            if let Ok(pub_subs) = state.db.list_public_subsystem_names(200).await {
                vals.extend(pub_subs);
            }
            if principal.is_authenticated() {
                let scope = principal.readable_section_names();
                let vis = principal.visibility(&scope);
                if let Ok(facets) = state.db.get_bug_subsystem_facets_o1(None, vis, 200).await {
                    for f in facets {
                        vals.insert(f.subsystem);
                    }
                }
            }
        }
        "window" => {
            for w in ["100", "500", "1000"] {
                vals.insert(w.to_string());
            }
        }
        _ => {
            if let Ok(found) = state.db.list_series_label_values(&name, 200).await {
                vals.extend(found);
            }
        }
    }
    let data: Vec<String> = vals.into_iter().collect();
    Json(json!({
        "status": "success",
        "data": data,
    }))
    .into_response()
}

pub async fn prom_metadata() -> Json<Value> {
    let mut data: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for m in METRIC_CATALOG.iter().chain(DB_METRIC_CATALOG.iter()) {
        data.insert(
            m.name,
            vec![json!({
                "type": m.metric_type.as_str(),
                "help": m.help,
                "unit": m.unit,
            })],
        );
    }
    Json(json!({
        "status": "success",
        "data": data,
    }))
}

// ============================================================================
// O(1) Preloaded Data Source for PromQL Evaluation
// ============================================================================

struct AstRequirements {
    instant_metrics: BTreeSet<String>,
    range_metrics: BTreeSet<String>,
    range_selectors: BTreeMap<String, Vec<Vec<LabelMatcher>>>,
    min_range_secs: i64,
    max_lookback_secs: i64,
    sum_only_range: bool,
    single_window: Option<(i64, i64)>,
    multiple_windows: bool,
}

fn inspect_ast(expr: &Expr, req: &mut AstRequirements) {
    match expr {
        Expr::Number(_) => {}
        Expr::Selector(sel) => {
            req.instant_metrics.insert(sel.metric.clone());
            req.max_lookback_secs = req.max_lookback_secs.max(sel.offset_secs);
        }
        Expr::RangeCall { func, selector } => {
            req.range_metrics.insert(selector.metric.clone());
            req.range_selectors
                .entry(selector.metric.clone())
                .or_default()
                .push(selector.matchers.clone());
            if !matches!(
                func,
                RangeFunc::Increase | RangeFunc::Rate | RangeFunc::SumOverTime
            ) {
                req.sum_only_range = false;
            }
            let w = selector.range_secs.unwrap_or(300).max(60);
            let win_key = (w, selector.offset_secs);
            match req.single_window {
                None => req.single_window = Some(win_key),
                Some(prev) if prev != win_key => req.multiple_windows = true,
                _ => {}
            }
            if req.min_range_secs == 0 || w < req.min_range_secs {
                req.min_range_secs = w;
            }
            req.max_lookback_secs = req
                .max_lookback_secs
                .max(selector.offset_secs.saturating_add(w));
        }
        Expr::Aggregate { expr, .. }
        | Expr::TopK { expr, .. }
        | Expr::HistogramQuantile { expr, .. }
        | Expr::ClampMin { expr, .. } => inspect_ast(expr, req),
        Expr::Binary { lhs, rhs, .. } => {
            inspect_ast(lhs, req);
            inspect_ast(rhs, req);
        }
    }
}

fn select_resolution_tier(min_range_secs: i64, max_lookback_secs: i64) -> i64 {
    // Subtract one bucket width from the 48h (5m) and 45d (1h) retention
    // cutoffs so a left-edge straddling bucket cannot have been reaped.
    if max_lookback_secs > 45 * 86_400 - 3600 || min_range_secs > 45 * 86_400 - 3600 {
        86_400
    } else if max_lookback_secs > 48 * 3600 - 300 || min_range_secs > 48 * 3600 - 300 {
        3600
    } else {
        300
    }
}

struct PreloadedPromDataSource {
    now_ts: i64,
    instant_by_metric: BTreeMap<String, Vec<EvalSample>>,
    range_res_secs: i64,
    range_by_metric: BTreeMap<String, Vec<(Labels, i64, f64)>>,
}

impl PromDataSource for PreloadedPromDataSource {
    fn now_ts(&self) -> i64 {
        self.now_ts
    }

    fn select_instant(&self, selector: &VectorSelector, _eval_ts: i64) -> Vec<EvalSample> {
        let Some(rows) = self.instant_by_metric.get(&selector.metric) else {
            return Vec::new();
        };
        rows.iter()
            .filter(|s| selector.matchers.iter().all(|m| m.matches(&s.labels)))
            .cloned()
            .collect()
    }

    fn select_range(
        &self,
        func: RangeFunc,
        selector: &VectorSelector,
        eval_ts: i64,
    ) -> Vec<EvalSample> {
        let Some(rows) = self.range_by_metric.get(&selector.metric) else {
            return Vec::new();
        };
        let win = selector.range_secs.unwrap_or(300).max(1);
        let start = eval_ts.saturating_sub(win);
        let res = self.range_res_secs.max(1);

        let mut grouped: BTreeMap<Labels, (f64, usize, f64)> = BTreeMap::new();
        for (labels, bucket_ts, val) in rows {
            if *bucket_ts > eval_ts {
                continue;
            }
            let bucket_end = bucket_ts.saturating_add(res);
            let effective_end = if bucket_end > self.now_ts {
                self.now_ts.max(*bucket_ts)
            } else {
                bucket_end
            };
            if effective_end <= start && *bucket_ts < start {
                continue;
            }
            if !selector.matchers.iter().all(|m| m.matches(labels)) {
                continue;
            }
            let weight = if *bucket_ts >= start && effective_end <= eval_ts {
                1.0
            } else {
                let span = effective_end.saturating_sub(*bucket_ts).max(1) as f64;
                let ov_s = (*bucket_ts).max(start);
                let ov_e = effective_end.min(eval_ts);
                (ov_e.saturating_sub(ov_s).max(0) as f64 / span).clamp(0.0, 1.0)
            };
            if weight <= 0.0 {
                continue;
            }
            let scaled = *val * weight;
            let entry = grouped.entry(labels.clone()).or_insert((0.0, 0, 0.0));
            entry.0 += scaled;
            entry.1 += 1;
            if entry.1 == 1 || *val > entry.2 {
                entry.2 = *val;
            }
        }

        grouped
            .into_iter()
            .map(|(labels, (sum, cnt, max_val))| {
                let value = match func {
                    RangeFunc::Increase | RangeFunc::SumOverTime => sum,
                    RangeFunc::Rate => sum / win as f64,
                    RangeFunc::AvgOverTime => {
                        if cnt == 0 {
                            0.0
                        } else {
                            sum / cnt as f64
                        }
                    }
                    RangeFunc::MaxOverTime => max_val,
                };
                EvalSample { labels, value }
            })
            .collect()
    }
}

async fn preload_data_source(
    state: &AppState,
    principal: &Principal,
    expr: &Expr,
    min_eval_ts: i64,
    max_eval_ts: i64,
    now_ts: i64,
) -> Result<PreloadedPromDataSource, PromError> {
    let mut req = AstRequirements {
        instant_metrics: BTreeSet::new(),
        range_metrics: BTreeSet::new(),
        range_selectors: BTreeMap::new(),
        min_range_secs: 0,
        max_lookback_secs: 0,
        sum_only_range: true,
        single_window: None,
        multiple_windows: false,
    };
    inspect_ast(expr, &mut req);

    let mut instant_by_metric: BTreeMap<String, Vec<EvalSample>> = BTreeMap::new();
    if !req.instant_metrics.is_empty() {
        populate_instant_metrics(
            state,
            principal,
            &req.instant_metrics,
            now_ts,
            &mut instant_by_metric,
        )
        .await?;
    }

    let span_from_max = max_eval_ts.saturating_sub(min_eval_ts);
    let age_from_now = now_ts.saturating_sub(min_eval_ts);
    let total_lookback = age_from_now
        .max(span_from_max)
        .saturating_add(req.max_lookback_secs);
    let range_res_secs = select_resolution_tier(req.min_range_secs.max(300), total_lookback);
    let mut range_by_metric: BTreeMap<String, Vec<(Labels, i64, f64)>> = BTreeMap::new();
    if !req.range_metrics.is_empty() {
        let query_start = min_eval_ts
            .saturating_sub(req.max_lookback_secs)
            .saturating_sub(range_res_secs);
        let names: Vec<String> = req.range_metrics.iter().cloned().collect();
        let selectors_ref = &req.range_selectors;
        let accept = |metric: &str, labels: &Labels| {
            selectors_ref
                .get(metric)
                .is_some_and(|sets| sets.iter().any(|ms| ms.iter().all(|m| m.matches(labels))))
        };
        let rows = if min_eval_ts == max_eval_ts
            && req.sum_only_range
            && !req.multiple_windows
            && let Some((win_secs, off_secs)) = req.single_window
        {
            let shifted_end = max_eval_ts.saturating_sub(off_secs);
            let win_start = shifted_end.saturating_sub(win_secs);
            // Interior buckets fully inside [win_start, min(shifted_end, now_ts)] have weight = 1.0.
            let full_start = win_start;
            let full_end = shifted_end.min(now_ts).saturating_sub(range_res_secs);
            state
                .db
                .query_rollup_sums_filtered(
                    &names,
                    range_res_secs,
                    query_start,
                    full_start,
                    full_end,
                    shifted_end,
                    accept,
                )
                .await
        } else {
            state
                .db
                .query_rollup_buckets_filtered(
                    &names,
                    range_res_secs,
                    query_start,
                    max_eval_ts,
                    accept,
                )
                .await
        }
        .map_err(|e| {
            error!("Failed querying stats_rollups: {e}");
            PromError::internal("failed to read statistics rollups")
        })?;
        fold_rollup_rows_and_unflushed(
            rows,
            &req.range_metrics,
            range_res_secs,
            now_ts,
            max_eval_ts,
            &mut range_by_metric,
        );
    }

    Ok(PreloadedPromDataSource {
        now_ts,
        instant_by_metric,
        range_res_secs,
        range_by_metric,
    })
}

fn fold_rollup_rows_and_unflushed(
    rows: Vec<RollupBucketRow>,
    wanted: &BTreeSet<String>,
    res_secs: i64,
    now_ts: i64,
    max_eval_ts: i64,
    out: &mut BTreeMap<String, Vec<(Labels, i64, f64)>>,
) {
    let mut merged: BTreeMap<(String, Labels, i64), f64> = BTreeMap::new();
    for r in rows {
        *merged
            .entry((r.metric, r.labels, r.bucket_ts))
            .or_insert(0.0) += r.value;
    }

    let cur_bucket = (now_ts / res_secs) * res_secs;
    if max_eval_ts >= cur_bucket {
        for delta in unflushed_runtime_deltas() {
            if !wanted.contains(delta.metric) {
                continue;
            }
            let labels: Labels = delta
                .labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            *merged
                .entry((delta.metric.to_string(), labels, cur_bucket))
                .or_insert(0.0) += delta.delta;
        }
    }

    for ((metric, labels, bucket_ts), val) in merged {
        out.entry(metric)
            .or_default()
            .push((labels, bucket_ts, val));
    }
}

async fn populate_instant_metrics(
    state: &AppState,
    principal: &Principal,
    needed: &BTreeSet<String>,
    now_ts: i64,
    out: &mut BTreeMap<String, Vec<EvalSample>>,
) -> Result<(), PromError> {
    populate_runtime_instant_metrics(state, needed, now_ts, out);

    let needs_db_gauges = needed.iter().any(|m| {
        matches!(
            m.as_str(),
            "sashiko_messages_stored"
                | "sashiko_patchsets_stored"
                | "sashiko_reviews_stored"
                | "sashiko_reviews_inherited_stored"
                | "sashiko_outbox_depth"
                | "sashiko_fetch_queue_depth"
                | "sashiko_bugs_total"
                | "sashiko_bugs_with_attachment"
        )
    });
    if needs_db_gauges {
        let gauges = state.get_cached_state_gauges().await.map_err(|e| {
            error!("Failed reading stats_state_gauges: {e}");
            PromError::internal("failed to read state gauges")
        })?;
        for (m, labels, val) in gauges {
            if needed.contains(&m) {
                out.entry(m).or_default().push(EvalSample {
                    labels,
                    value: val as f64,
                });
            }
        }
    }

    if needed.contains("sashiko_stats_backfill_complete") {
        let done = state.db.is_stats_backfill_done().await.unwrap_or(false);
        out.insert(
            "sashiko_stats_backfill_complete".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: if done { 1.0 } else { 0.0 },
            }],
        );
    }

    if needed.contains("sashiko_project_release_timestamp_seconds") {
        let releases = state.get_cached_project_releases().await;
        let samples = releases
            .into_iter()
            .map(|(tag, ts)| {
                let mut l = Labels::new();
                l.insert("tag".to_string(), tag);
                EvalSample {
                    labels: l,
                    value: ts as f64,
                }
            })
            .collect();
        out.insert(
            "sashiko_project_release_timestamp_seconds".to_string(),
            samples,
        );
    }

    if needed.contains("sashiko_bugs_by_subsystem") {
        let facets = load_authorized_bug_subsystems(state, principal).await?;
        let mut samples = Vec::new();
        for f in facets {
            for (facet_name, cnt) in [
                ("open", f.open),
                ("fixed", f.fixed),
                ("duplicate", f.duplicate),
                ("dismissed", f.dismissed),
                ("total", f.total),
                ("high_crit_open", f.open_sev_critical + f.open_sev_high),
                ("sec_high_crit_open", f.open_sec_critical + f.open_sec_high),
            ] {
                if cnt > 0 || facet_name == "open" {
                    let mut l = Labels::new();
                    l.insert("facet".to_string(), facet_name.to_string());
                    l.insert("subsystem".to_string(), f.subsystem.clone());
                    samples.push(EvalSample {
                        labels: l,
                        value: cnt as f64,
                    });
                }
            }
        }
        out.insert("sashiko_bugs_by_subsystem".to_string(), samples);
    }

    if needed.iter().any(|m| m.starts_with("sashiko_pipeline_")) {
        let multi = state.get_cached_pipeline_stats(now_ts).await.map_err(|e| {
            error!("Failed loading pipeline stats: {e}");
            PromError::internal("failed to read pipeline summary stats")
        })?;
        for (m, rows) in build_pipeline_samples(&multi) {
            if needed.contains(m) {
                out.insert(m.to_string(), rows);
            }
        }
    }

    Ok(())
}

async fn load_authorized_bug_subsystems(
    state: &AppState,
    principal: &Principal,
) -> Result<Vec<BugSubsystemFacetSummary>, PromError> {
    if !principal.is_authenticated() {
        return Ok(Vec::new());
    }
    let scope = principal.readable_section_names();
    let vis = principal.visibility(&scope);
    let res = match vis {
        crate::db::BugVisibility::Unrestricted => state.get_cached_unrestricted_bug_facets().await,
        crate::db::BugVisibility::Sections(_) => {
            state.db.get_bug_subsystem_facets_o1(None, vis, 200).await
        }
    };
    res.map_err(|e| {
        error!("Failed reading bug subsystem facets: {e}");
        PromError::internal("failed to read bug subsystem stats")
    })
}

fn populate_runtime_instant_metrics(
    state: &AppState,
    needed: &BTreeSet<String>,
    now_ts: i64,
    out: &mut BTreeMap<String, Vec<EvalSample>>,
) {
    let single = |k: &str, v: &str| -> Labels {
        let mut m = Labels::new();
        m.insert(k.to_string(), v.to_string());
        m
    };

    if needed.contains("sashiko_build_info") {
        out.insert(
            "sashiko_build_info".to_string(),
            vec![EvalSample {
                labels: single("version", env!("CARGO_PKG_VERSION")),
                value: 1.0,
            }],
        );
    }
    if needed.contains("sashiko_queue_depth") {
        out.entry("sashiko_queue_depth".to_string())
            .or_default()
            .extend([
                EvalSample {
                    labels: single("queue", "patches_pending"),
                    value: crate::metrics::get_pending_patches() as f64,
                },
                EvalSample {
                    labels: single("queue", "patches_reviewing"),
                    value: crate::metrics::get_reviewing_patches() as f64,
                },
            ]);
    }
    if needed.contains("sashiko_repo_packs") {
        out.insert(
            "sashiko_repo_packs".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: crate::metrics::get_repo_packs() as f64,
            }],
        );
    }
    if needed.contains("sashiko_repo_pack_bytes") {
        out.insert(
            "sashiko_repo_pack_bytes".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: crate::metrics::get_repo_pack_bytes() as f64,
            }],
        );
    }
    if needed.contains("sashiko_git_remotes") {
        let samples = crate::metrics::GitRemoteState::ALL
            .into_iter()
            .map(|st| EvalSample {
                labels: single("state", st.as_str()),
                value: crate::metrics::get_git_remotes(st) as f64,
            })
            .collect();
        out.insert("sashiko_git_remotes".to_string(), samples);
    }
    if needed.contains("sashiko_git_remote_max_age_seconds") {
        out.insert(
            "sashiko_git_remote_max_age_seconds".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: crate::metrics::get_git_remote_max_age_secs() as f64,
            }],
        );
    }
    if needed.contains("sashiko_git_remote_stale_age_seconds") {
        let samples = crate::metrics::get_stale_git_remotes()
            .into_iter()
            .map(|item| {
                let mut labels = Labels::new();
                labels.insert("remote".to_string(), item.remote);
                labels.insert("state".to_string(), item.state.as_str().to_string());
                labels.insert("url".to_string(), item.url);
                EvalSample {
                    labels,
                    value: item.age_secs.map(|s| s as f64).unwrap_or(-1.0),
                }
            })
            .collect();
        out.insert("sashiko_git_remote_stale_age_seconds".to_string(), samples);
    }
    if needed.contains("sashiko_nntp_lag_articles") {
        let (tot, max_g) = crate::metrics::get_nntp_lag();
        out.insert(
            "sashiko_nntp_lag_articles".to_string(),
            vec![
                EvalSample {
                    labels: single("agg", "total"),
                    value: tot as f64,
                },
                EvalSample {
                    labels: single("agg", "max_group"),
                    value: max_g as f64,
                },
            ],
        );
    }
    if needed.contains("sashiko_ai_inflight_requests") {
        out.insert(
            "sashiko_ai_inflight_requests".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: crate::metrics::get_ai_inflight_requests() as f64,
            }],
        );
    }
    if needed.contains("sashiko_ai_quota_cooldown_seconds") {
        out.insert(
            "sashiko_ai_quota_cooldown_seconds".to_string(),
            vec![EvalSample {
                labels: Labels::new(),
                value: crate::metrics::get_ai_quota_cooldown_secs() as f64,
            }],
        );
    }
    if needed.contains("sashiko_email_worker_heartbeat_age_seconds") {
        let hb_age = if let Some(ref hb) = state.email_worker_heartbeat {
            let last = hb.load(std::sync::atomic::Ordering::Relaxed);
            now_ts.saturating_sub(last).max(0)
        } else {
            crate::metrics::get_email_worker_heartbeat_age_secs()
        };
        if hb_age >= 0 {
            out.insert(
                "sashiko_email_worker_heartbeat_age_seconds".to_string(),
                vec![EvalSample {
                    labels: Labels::new(),
                    value: hb_age as f64,
                }],
            );
        }
    }

    for sample in crate::metrics::current_runtime_counter_samples() {
        if needed.contains(sample.metric) {
            let labels: Labels = sample
                .labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            out.entry(sample.metric.to_string())
                .or_default()
                .push(EvalSample {
                    labels,
                    value: sample.delta,
                });
        }
    }
}

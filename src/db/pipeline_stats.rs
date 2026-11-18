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

//! Pre-extracted per-review pipeline telemetry (`review_pipeline_stats`) and
//! single-pass O(1) aggregation across Last-`N` (`100`, `500`, `1000`) windows.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Database;

/// Normalizes sharded stage names (`prior-feedback-2`, `verification-1`,
/// `post-verification-3`) to their canonical logical stage identifier while
/// preserving hyphenated stage names such as `error-handling`.
pub fn normalize_stage_name(raw: &str) -> String {
    let trimmed = raw.trim();
    for prefix in [
        "prior-feedback-",
        "verification-",
        "post-verification-",
        "prior-attribution-",
        "related-preexisting-check-",
    ] {
        if let Some(suffix) = trimmed.strip_prefix(prefix)
            && !suffix.is_empty()
            && suffix.bytes().all(|b| b.is_ascii_digit())
        {
            return prefix.trim_end_matches('-').to_string();
        }
    }
    trimmed.to_string()
}

fn is_discovery_stage(stage: &str) -> bool {
    !stage.is_empty()
        && !matches!(
            stage,
            "pre-screen"
                | "planning"
                | "deduplication"
                | "verification"
                | "post-verification"
                | "prior-attribution"
                | "report"
                | "summary"
        )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactStageStat {
    #[serde(rename = "s")]
    pub stage: String,
    #[serde(rename = "ex", default)]
    pub executed: u32,
    #[serde(rename = "sk", default)]
    pub skipped: u32,
    #[serde(rename = "t", default)]
    pub turns: u64,
    #[serde(rename = "ti", default)]
    pub tokens_in: u64,
    #[serde(rename = "to", default)]
    pub tokens_out: u64,
    #[serde(rename = "tc", default)]
    pub tokens_cached: u64,
    #[serde(rename = "rc", default)]
    pub raw_concerns: u64,
    #[serde(rename = "sd", default)]
    pub stage1_dismissed: u64,
    #[serde(rename = "cf", default)]
    pub confirmed_findings: u64,
    #[serde(rename = "xf", default)]
    pub exclusive_findings: u64,
    #[serde(rename = "dv", default)]
    pub dismissed_in_verify: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactPriorStat {
    #[serde(default)]
    pub pf_addressed: u64,
    #[serde(default)]
    pub pf_unaddressed: u64,
    #[serde(default)]
    pub hm_addressed: u64,
    #[serde(default)]
    pub hm_unaddressed: u64,
    #[serde(default)]
    pub hm_also_found: u64,
}

#[derive(Debug, Clone)]
pub struct ReviewPipelineSummary {
    pub review_id: i64,
    pub patchset_id: i64,
    pub created_at: i64,
    pub duration_secs: i64,
    pub project: String,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub inherited: bool,
    pub raw_concerns: i64,
    pub stage1_dismissed: i64,
    pub deduplicated_clusters: i64,
    pub verified_1a: i64,
    pub dismissed_1b: i64,
    pub inconclusive: i64,
    pub verified_pv: i64,
    pub dismissed_pv: i64,
    pub findings_patch: i64,
    pub findings_preexisting: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cached: i64,
    pub stages_json: String,
    pub overlap_json: String,
    pub tools_json: String,
    pub prior_json: String,
}

pub struct ExtractSummaryInput<'a> {
    pub review_id: i64,
    pub patchset_id: i64,
    pub created_at: i64,
    pub completed_at: i64,
    pub provider: &'a str,
    pub model: &'a str,
    pub status: &'a str,
    pub inherited: bool,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cached: i64,
    pub review_json: Option<&'a Value>,
    pub tool_rows: &'a [(String, i64, i64)],
}

/// Extracts a compact uncompressed pipeline summary from a completed review's
/// `WorkerResult.output` JSON and `tool_usages` aggregates.
pub fn extract_review_pipeline_summary(inp: ExtractSummaryInput<'_>) -> ReviewPipelineSummary {
    let duration_secs = inp.completed_at.saturating_sub(inp.created_at).max(0);
    let mut project = String::new();
    let mut raw_concerns = 0i64;
    let mut stage1_dismissed = 0i64;
    let mut deduplicated_clusters = 0i64;
    let mut verified_1a = 0i64;
    let mut dismissed_1b = 0i64;
    let mut inconclusive = 0i64;
    let mut verified_pv = 0i64;
    let mut dismissed_pv = 0i64;
    let mut findings_patch = 0i64;
    let mut findings_preexisting = 0i64;
    let mut stages_map: BTreeMap<String, CompactStageStat> = BTreeMap::new();
    let mut overlap_map: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut prior = CompactPriorStat::default();

    if let Some(root) = inp.review_json {
        if let Some(f_arr) = root.get("findings").and_then(Value::as_array) {
            for f in f_arr {
                if f.get("preexisting").and_then(Value::as_bool) == Some(true) {
                    findings_preexisting += 1;
                } else {
                    findings_patch += 1;
                }
            }
        }

        if let Some(rm) = root.get("review_map") {
            project = rm
                .get("project")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            raw_concerns = array_len(rm.get("raw_concerns"));
            stage1_dismissed = array_len(rm.get("raw_dismissed_concerns"));
            deduplicated_clusters = array_len(rm.get("deduplicated_clusters"));
            if let Some(ver) = rm.get("verification") {
                verified_1a = array_len(ver.get("findings"));
                inconclusive = array_len(ver.get("inconclusive_concerns"));
                dismissed_1b = count_concern_dismissed_1b(rm, ver);
            }
            if let Some(pv) = rm.get("post_verification") {
                verified_pv = array_len(pv.get("findings"));
                dismissed_pv = array_len(pv.get("dismissed_concerns"));
            }
            if findings_preexisting == 0
                && let Some(threads) = rm.get("threads").and_then(Value::as_array)
            {
                findings_preexisting = threads
                    .iter()
                    .filter(|th| {
                        th.get("outcome").and_then(Value::as_str) == Some("preexisting_concern")
                    })
                    .count() as i64;
            }
            extract_stage_records(rm, &mut stages_map);
            extract_thread_lineage(rm, &mut stages_map, &mut overlap_map);
        }

        let res_source = root
            .get("review_map")
            .and_then(|rm| rm.get("prior_resolutions"))
            .or_else(|| root.get("prior_resolutions"));
        if let Some(arr) = res_source.and_then(Value::as_array) {
            extract_prior_stats(arr, &mut prior);
        }
    }

    let stages_vec: Vec<CompactStageStat> = stages_map.into_values().collect();
    let overlap_vec: Vec<(String, String, u64)> = overlap_map
        .into_iter()
        .map(|((a, b), c)| (a, b, c))
        .collect();
    let tools_vec: Vec<(&str, i64, i64)> = inp
        .tool_rows
        .iter()
        .map(|(n, c, b)| (n.as_str(), *c, *b))
        .collect();

    ReviewPipelineSummary {
        review_id: inp.review_id,
        patchset_id: inp.patchset_id,
        created_at: inp.created_at,
        duration_secs,
        project,
        provider: inp.provider.to_string(),
        model: inp.model.to_string(),
        status: inp.status.to_string(),
        inherited: inp.inherited,
        raw_concerns,
        stage1_dismissed,
        deduplicated_clusters,
        verified_1a,
        dismissed_1b,
        inconclusive,
        verified_pv,
        dismissed_pv,
        findings_patch,
        findings_preexisting,
        tokens_in: inp.tokens_in.max(0),
        tokens_out: inp.tokens_out.max(0),
        tokens_cached: inp.tokens_cached.max(0),
        stages_json: serde_json::to_string(&stages_vec).unwrap_or_else(|_| "[]".to_string()),
        overlap_json: serde_json::to_string(&overlap_vec).unwrap_or_else(|_| "[]".to_string()),
        tools_json: serde_json::to_string(&tools_vec).unwrap_or_else(|_| "[]".to_string()),
        prior_json: serde_json::to_string(&prior).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn array_len(val: Option<&Value>) -> i64 {
    val.and_then(Value::as_array).map_or(0, |a| a.len() as i64)
}

/// Returns `true` when a verification thread originated solely from Stage-1
/// self-dismissed items (`raw_dismissed_ids` non-empty, `raw_concern_ids` empty).
/// Verification audits those to ensure nothing was prematurely dismissed, so
/// confirming their dismissal is not a Stage-1 false positive.
fn is_stage1_self_dismissed_only(item: &Value) -> bool {
    let has_raw_concern = item
        .get("raw_concern_ids")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let has_raw_dismissed = item
        .get("raw_dismissed_ids")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    !has_raw_concern && has_raw_dismissed
}

fn count_concern_dismissed_1b(rm: &Value, ver: &Value) -> i64 {
    if let Some(threads) = rm.get("threads").and_then(Value::as_array)
        && !threads.is_empty()
    {
        return threads
            .iter()
            .filter(|th| {
                th.get("outcome").and_then(Value::as_str) == Some("dismissed_1b")
                    && !is_stage1_self_dismissed_only(th)
            })
            .count() as i64;
    }
    ver.get("dismissed_concerns")
        .and_then(Value::as_array)
        .map_or(0, |arr| {
            arr.iter()
                .filter(|item| !is_stage1_self_dismissed_only(item))
                .count() as i64
        })
}

fn extract_stage_records(rm: &Value, stages_map: &mut BTreeMap<String, CompactStageStat>) {
    let Some(stages) = rm.get("stages").and_then(Value::as_array) else {
        return;
    };
    for st in stages {
        let Some(raw_name) = st.get("name").and_then(Value::as_str) else {
            continue;
        };
        let norm = normalize_stage_name(raw_name);
        if norm.is_empty() {
            continue;
        }
        let skipped = st.get("skipped").and_then(Value::as_bool).unwrap_or(false);
        let entry = stages_map
            .entry(norm.clone())
            .or_insert_with(|| CompactStageStat {
                stage: norm,
                ..Default::default()
            });
        if skipped {
            if entry.executed == 0 {
                entry.skipped = 1;
            }
        } else {
            entry.executed = 1;
            entry.skipped = 0;
        }
        entry.turns += st.get("turns").and_then(Value::as_u64).unwrap_or(0);
        entry.tokens_in += st.get("tokens_in").and_then(Value::as_u64).unwrap_or(0);
        entry.tokens_out += st.get("tokens_out").and_then(Value::as_u64).unwrap_or(0);
        entry.tokens_cached += st.get("tokens_cached").and_then(Value::as_u64).unwrap_or(0);
        entry.raw_concerns += st
            .get("concerns_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        entry.stage1_dismissed += st
            .get("dismissed_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
}

fn extract_thread_lineage(
    rm: &Value,
    stages_map: &mut BTreeMap<String, CompactStageStat>,
    overlap_map: &mut BTreeMap<(String, String), u64>,
) {
    let Some(threads) = rm.get("threads").and_then(Value::as_array) else {
        return;
    };
    for th in threads {
        let outcome = th.get("outcome").and_then(Value::as_str).unwrap_or("");
        let mut disc_stages = BTreeSet::new();
        if let Some(arr) = th.get("stages").and_then(Value::as_array) {
            for s in arr.iter().filter_map(Value::as_str) {
                let norm = normalize_stage_name(s);
                if is_discovery_stage(&norm) {
                    disc_stages.insert(norm);
                }
            }
        }
        if disc_stages.is_empty() {
            continue;
        }
        let is_confirmed = matches!(outcome, "finding" | "preexisting_concern");
        let is_dismissed = match outcome {
            "dismissed_1b" => !is_stage1_self_dismissed_only(th),
            "dismissed_post_verification" | "unresolved_inconclusive" | "unverified_concern" => {
                true
            }
            _ => false,
        };
        if is_confirmed {
            let exclusive = disc_stages.len() == 1;
            let ordered: Vec<String> = disc_stages.into_iter().collect();
            for s in &ordered {
                let entry = stages_map
                    .entry(s.clone())
                    .or_insert_with(|| CompactStageStat {
                        stage: s.clone(),
                        executed: 1,
                        ..Default::default()
                    });
                entry.confirmed_findings += 1;
                if exclusive {
                    entry.exclusive_findings += 1;
                }
            }
            if ordered.len() == 1 {
                let only = ordered[0].clone();
                *overlap_map.entry((only.clone(), only)).or_insert(0) += 1;
            } else {
                for i in 0..ordered.len() {
                    for j in (i + 1)..ordered.len() {
                        *overlap_map
                            .entry((ordered[i].clone(), ordered[j].clone()))
                            .or_insert(0) += 1;
                    }
                }
            }
        } else if is_dismissed {
            for s in disc_stages {
                let entry = stages_map
                    .entry(s.clone())
                    .or_insert_with(|| CompactStageStat {
                        stage: s,
                        executed: 1,
                        ..Default::default()
                    });
                entry.dismissed_in_verify += 1;
            }
        }
    }
}

fn extract_prior_stats(arr: &[Value], out: &mut CompactPriorStat) {
    for r in arr {
        let origin = r.get("origin_kind").and_then(Value::as_str).unwrap_or("");
        let verdict = r.get("verdict").and_then(Value::as_str).unwrap_or("");
        let vn_out = r.get("v_n_outcome").and_then(Value::as_str).unwrap_or("");
        let also = match r.get("also_found_by_sashiko") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
            _ => false,
        };
        let addressed = vn_out == "addressed" || verdict == "addressed_in_vN";
        let unaddressed = vn_out == "confirmed_finding" || verdict == "still_unaddressed";
        match origin {
            "sashiko_review" => {
                if addressed {
                    out.pf_addressed += 1;
                } else if unaddressed {
                    out.pf_unaddressed += 1;
                }
            }
            "human_reviewer" => {
                if addressed {
                    out.hm_addressed += 1;
                } else if unaddressed {
                    out.hm_unaddressed += 1;
                    if also {
                        out.hm_also_found += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

// ============================================================================
// Aggregated Last-N Window Output
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelStatusKey {
    pub provider: String,
    pub model: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelReviewAggregate {
    pub count: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PipelineWindowAggregate {
    pub window: usize,
    pub reviews_count: u64,
    pub duration_secs_sum: u64,
    pub tokens_in_sum: u64,
    pub tokens_out_sum: u64,
    pub tokens_cached_sum: u64,
    pub raw_concerns: u64,
    pub stage1_dismissed: u64,
    pub deduplicated_clusters: u64,
    pub verified_1a: u64,
    pub dismissed_1b: u64,
    pub inconclusive: u64,
    pub verified_pv: u64,
    pub dismissed_pv: u64,
    pub findings_patch: u64,
    pub findings_preexisting: u64,
    pub stages: BTreeMap<String, CompactStageStat>,
    pub overlap: BTreeMap<(String, String), u64>,
    pub tools: BTreeMap<String, (u64, u64)>,
    pub prior: CompactPriorStat,
    pub by_model: BTreeMap<ModelStatusKey, ModelReviewAggregate>,
}

#[derive(Debug, Clone, Default)]
pub struct PipelineMultiWindowStats {
    pub windows: [PipelineWindowAggregate; 3],
}

impl PipelineWindowAggregate {
    fn new(window: usize) -> Self {
        Self {
            window,
            ..Default::default()
        }
    }

    fn fold_row(&mut self, row: &ReviewPipelineSummary) {
        self.reviews_count += 1;
        self.duration_secs_sum += row.duration_secs.max(0) as u64;
        self.tokens_in_sum += row.tokens_in.max(0) as u64;
        self.tokens_out_sum += row.tokens_out.max(0) as u64;
        self.tokens_cached_sum += row.tokens_cached.max(0) as u64;
        self.raw_concerns += row.raw_concerns.max(0) as u64;
        self.stage1_dismissed += row.stage1_dismissed.max(0) as u64;
        self.deduplicated_clusters += row.deduplicated_clusters.max(0) as u64;
        self.verified_1a += row.verified_1a.max(0) as u64;
        self.dismissed_1b += row.dismissed_1b.max(0) as u64;
        self.inconclusive += row.inconclusive.max(0) as u64;
        self.verified_pv += row.verified_pv.max(0) as u64;
        self.dismissed_pv += row.dismissed_pv.max(0) as u64;
        self.findings_patch += row.findings_patch.max(0) as u64;
        self.findings_preexisting += row.findings_preexisting.max(0) as u64;

        let model_entry = self
            .by_model
            .entry(ModelStatusKey {
                provider: row.provider.clone(),
                model: row.model.clone(),
                status: row.status.clone(),
            })
            .or_default();
        model_entry.count += 1;
        model_entry.tokens_in += row.tokens_in.max(0) as u64;
        model_entry.tokens_out += row.tokens_out.max(0) as u64;
        model_entry.tokens_cached += row.tokens_cached.max(0) as u64;

        if let Ok(stages) = serde_json::from_str::<Vec<CompactStageStat>>(&row.stages_json) {
            for st in stages {
                let dst = self
                    .stages
                    .entry(st.stage.clone())
                    .or_insert_with(|| CompactStageStat {
                        stage: st.stage.clone(),
                        ..Default::default()
                    });
                dst.executed += st.executed;
                dst.skipped += st.skipped;
                dst.turns += st.turns;
                dst.tokens_in += st.tokens_in;
                dst.tokens_out += st.tokens_out;
                dst.tokens_cached += st.tokens_cached;
                dst.raw_concerns += st.raw_concerns;
                dst.stage1_dismissed += st.stage1_dismissed;
                dst.confirmed_findings += st.confirmed_findings;
                dst.exclusive_findings += st.exclusive_findings;
                dst.dismissed_in_verify += st.dismissed_in_verify;
            }
        }
        if let Ok(ov) = serde_json::from_str::<Vec<(String, String, u64)>>(&row.overlap_json) {
            for (a, b, c) in ov {
                *self.overlap.entry((a, b)).or_insert(0) += c;
            }
        }
        if let Ok(tools) = serde_json::from_str::<Vec<(String, i64, i64)>>(&row.tools_json) {
            for (name, cnt, bytes) in tools {
                let e = self.tools.entry(name).or_insert((0, 0));
                e.0 += cnt.max(0) as u64;
                e.1 += bytes.max(0) as u64;
            }
        }
        if let Ok(pr) = serde_json::from_str::<CompactPriorStat>(&row.prior_json) {
            self.prior.pf_addressed += pr.pf_addressed;
            self.prior.pf_unaddressed += pr.pf_unaddressed;
            self.prior.hm_addressed += pr.hm_addressed;
            self.prior.hm_unaddressed += pr.hm_unaddressed;
            self.prior.hm_also_found += pr.hm_also_found;
        }
    }
}

impl Database {
    /// Upserts a single pre-extracted row into `review_pipeline_stats`.
    pub async fn upsert_review_pipeline_stats(&self, s: &ReviewPipelineSummary) -> Result<()> {
        self.execute(
            "INSERT INTO review_pipeline_stats (
                review_id, patchset_id, created_at, duration_secs, project, provider, model,
                status, inherited, raw_concerns, stage1_dismissed, deduplicated_clusters,
                verified_1a, dismissed_1b, inconclusive, verified_pv, dismissed_pv,
                findings_patch, findings_preexisting, tokens_in, tokens_out, tokens_cached,
                stages_json, overlap_json, tools_json, prior_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(review_id) DO UPDATE SET
                duration_secs = excluded.duration_secs,
                project = excluded.project,
                provider = excluded.provider,
                model = excluded.model,
                status = excluded.status,
                inherited = excluded.inherited,
                raw_concerns = excluded.raw_concerns,
                stage1_dismissed = excluded.stage1_dismissed,
                deduplicated_clusters = excluded.deduplicated_clusters,
                verified_1a = excluded.verified_1a,
                dismissed_1b = excluded.dismissed_1b,
                inconclusive = excluded.inconclusive,
                verified_pv = excluded.verified_pv,
                dismissed_pv = excluded.dismissed_pv,
                findings_patch = excluded.findings_patch,
                findings_preexisting = excluded.findings_preexisting,
                tokens_in = excluded.tokens_in,
                tokens_out = excluded.tokens_out,
                tokens_cached = excluded.tokens_cached,
                stages_json = excluded.stages_json,
                overlap_json = excluded.overlap_json,
                tools_json = excluded.tools_json,
                prior_json = excluded.prior_json",
            libsql::params![
                s.review_id,
                s.patchset_id,
                s.created_at,
                s.duration_secs,
                s.project.as_str(),
                s.provider.as_str(),
                s.model.as_str(),
                s.status.as_str(),
                i64::from(s.inherited),
                s.raw_concerns,
                s.stage1_dismissed,
                s.deduplicated_clusters,
                s.verified_1a,
                s.dismissed_1b,
                s.inconclusive,
                s.verified_pv,
                s.dismissed_pv,
                s.findings_patch,
                s.findings_preexisting,
                s.tokens_in,
                s.tokens_out,
                s.tokens_cached,
                s.stages_json.as_str(),
                s.overlap_json.as_str(),
                s.tools_json.as_str(),
                s.prior_json.as_str(),
            ],
        )
        .await?;
        Ok(())
    }

    /// Queries `tool_usages` for a single `review_id` via `idx_tool_usages_review`.
    pub async fn get_review_tool_aggregates(
        &self,
        review_id: i64,
    ) -> Result<Vec<(String, i64, i64)>> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT COALESCE(tool_name, 'unknown'), COUNT(*), COALESCE(SUM(output_length), 0)
                 FROM tool_usages
                 WHERE review_id = ?
                 GROUP BY COALESCE(tool_name, 'unknown')",
                libsql::params![review_id],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push((r.get::<String>(0)?, r.get::<i64>(1)?, r.get::<i64>(2)?));
        }
        Ok(out)
    }

    /// Computes Last-100, Last-500, and Last-1000 pipeline aggregates in one
    /// O(1) pass (`LIMIT 1000` by primary key `review_id DESC`), excluding
    /// inherited clean-review skips and currently embargoed patchsets.
    pub async fn get_pipeline_multi_window_stats(
        &self,
        now_ts: i64,
    ) -> Result<PipelineMultiWindowStats> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT rps.review_id, rps.patchset_id, rps.created_at, rps.duration_secs,
                        rps.project, rps.provider, rps.model, rps.status, rps.inherited,
                        rps.raw_concerns, rps.stage1_dismissed, rps.deduplicated_clusters,
                        rps.verified_1a, rps.dismissed_1b, rps.inconclusive,
                        rps.verified_pv, rps.dismissed_pv, rps.findings_patch,
                        rps.findings_preexisting, rps.tokens_in, rps.tokens_out,
                        rps.tokens_cached, rps.stages_json, rps.overlap_json,
                        rps.tools_json, rps.prior_json
                 FROM review_pipeline_stats rps
                 JOIN patchsets ps ON ps.id = rps.patchset_id
                 WHERE rps.inherited = 0
                   AND rps.status = 'Reviewed'
                   AND (ps.embargo_until IS NULL OR ps.embargo_until <= ?)
                 ORDER BY rps.review_id DESC
                 LIMIT 1000",
                libsql::params![now_ts],
            )
            .await?;

        let mut w100 = PipelineWindowAggregate::new(100);
        let mut w500 = PipelineWindowAggregate::new(500);
        let mut w1000 = PipelineWindowAggregate::new(1000);
        let mut idx = 0usize;

        while let Some(r) = rows.next().await? {
            let item = ReviewPipelineSummary {
                review_id: r.get(0)?,
                patchset_id: r.get(1)?,
                created_at: r.get(2)?,
                duration_secs: r.get(3)?,
                project: r.get(4)?,
                provider: r.get(5)?,
                model: r.get(6)?,
                status: r.get(7)?,
                inherited: r.get::<i64>(8)? != 0,
                raw_concerns: r.get(9)?,
                stage1_dismissed: r.get(10)?,
                deduplicated_clusters: r.get(11)?,
                verified_1a: r.get(12)?,
                dismissed_1b: r.get(13)?,
                inconclusive: r.get(14)?,
                verified_pv: r.get(15)?,
                dismissed_pv: r.get(16)?,
                findings_patch: r.get(17)?,
                findings_preexisting: r.get(18)?,
                tokens_in: r.get(19)?,
                tokens_out: r.get(20)?,
                tokens_cached: r.get(21)?,
                stages_json: r.get(22)?,
                overlap_json: r.get(23)?,
                tools_json: r.get(24)?,
                prior_json: r.get(25)?,
            };
            if idx < 100 {
                w100.fold_row(&item);
            }
            if idx < 500 {
                w500.fold_row(&item);
            }
            w1000.fold_row(&item);
            idx += 1;
        }

        Ok(PipelineMultiWindowStats {
            windows: [w100, w500, w1000],
        })
    }

    /// Extracts and persists `review_pipeline_stats` for a newly completed
    /// review, returning its computed wall-clock `duration_secs`.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_completed_review_telemetry(
        &self,
        review_id: i64,
        patchset_id: i64,
        provider: &str,
        model: &str,
        status: &str,
        inherited: bool,
        tokens_in: i64,
        tokens_out: i64,
        tokens_cached: i64,
        review_json: Option<&Value>,
    ) -> Result<u64> {
        let now_ts = chrono::Utc::now().timestamp();
        let tx = self.begin_immediate_transaction().await?;
        let created_at = {
            let mut rows = tx
                .query(
                    "SELECT COALESCE(created_at, ?) FROM reviews WHERE id = ?",
                    libsql::params![now_ts, review_id],
                )
                .await?;
            let val = match rows.next().await? {
                Some(r) => r.get::<i64>(0)?,
                None => now_ts,
            };
            drop(rows);
            val
        };
        let mut tool_rows = Vec::new();
        {
            let mut tr = tx
                .query(
                    "SELECT COALESCE(tool_name, 'unknown'), COUNT(*), COALESCE(SUM(output_length), 0)
                     FROM tool_usages
                     WHERE review_id = ?
                     GROUP BY COALESCE(tool_name, 'unknown')",
                    libsql::params![review_id],
                )
                .await?;
            while let Some(r) = tr.next().await? {
                tool_rows.push((r.get::<String>(0)?, r.get::<i64>(1)?, r.get::<i64>(2)?));
            }
        }
        let s = extract_review_pipeline_summary(ExtractSummaryInput {
            review_id,
            patchset_id,
            created_at,
            completed_at: now_ts,
            provider,
            model,
            status,
            inherited,
            tokens_in,
            tokens_out,
            tokens_cached,
            review_json,
            tool_rows: &tool_rows,
        });
        let dur = s.duration_secs.max(0) as u64;
        upsert_review_pipeline_stats_in_tx(&tx, &s).await?;
        tx.commit().await?;
        Ok(dur)
    }

    /// Atomically persists a batch of backfilled `review_pipeline_stats` rows
    /// alongside the updated pipeline backfill cursor and count in `meta`.
    pub async fn persist_pipeline_backfill_batch(
        &self,
        summaries: &[ReviewPipelineSummary],
        new_cursor: i64,
        new_done_count: usize,
    ) -> Result<()> {
        let tx = self.begin_immediate_transaction().await?;
        for s in summaries {
            upsert_review_pipeline_stats_in_tx(&tx, s).await?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:v1:cursor:pipeline', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![new_cursor.to_string()],
        )
        .await?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:v1:pipeline_count', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![new_done_count.to_string()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn upsert_review_pipeline_stats_in_tx(
    tx: &super::DatabaseTransaction,
    s: &ReviewPipelineSummary,
) -> Result<()> {
    tx.execute(
        "INSERT INTO review_pipeline_stats (
            review_id, patchset_id, created_at, duration_secs, project, provider, model,
            status, inherited, raw_concerns, stage1_dismissed, deduplicated_clusters,
            verified_1a, dismissed_1b, inconclusive, verified_pv, dismissed_pv,
            findings_patch, findings_preexisting, tokens_in, tokens_out, tokens_cached,
            stages_json, overlap_json, tools_json, prior_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(review_id) DO UPDATE SET
            duration_secs = excluded.duration_secs,
            project = excluded.project,
            provider = excluded.provider,
            model = excluded.model,
            status = excluded.status,
            inherited = excluded.inherited,
            raw_concerns = excluded.raw_concerns,
            stage1_dismissed = excluded.stage1_dismissed,
            deduplicated_clusters = excluded.deduplicated_clusters,
            verified_1a = excluded.verified_1a,
            dismissed_1b = excluded.dismissed_1b,
            inconclusive = excluded.inconclusive,
            verified_pv = excluded.verified_pv,
            dismissed_pv = excluded.dismissed_pv,
            findings_patch = excluded.findings_patch,
            findings_preexisting = excluded.findings_preexisting,
            tokens_in = excluded.tokens_in,
            tokens_out = excluded.tokens_out,
            tokens_cached = excluded.tokens_cached,
            stages_json = excluded.stages_json,
            overlap_json = excluded.overlap_json,
            tools_json = excluded.tools_json,
            prior_json = excluded.prior_json",
        libsql::params![
            s.review_id,
            s.patchset_id,
            s.created_at,
            s.duration_secs,
            s.project.as_str(),
            s.provider.as_str(),
            s.model.as_str(),
            s.status.as_str(),
            i64::from(s.inherited),
            s.raw_concerns,
            s.stage1_dismissed,
            s.deduplicated_clusters,
            s.verified_1a,
            s.dismissed_1b,
            s.inconclusive,
            s.verified_pv,
            s.dismissed_pv,
            s.findings_patch,
            s.findings_preexisting,
            s.tokens_in,
            s.tokens_out,
            s.tokens_cached,
            s.stages_json.as_str(),
            s.overlap_json.as_str(),
            s.tools_json.as_str(),
            s.prior_json.as_str(),
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use serde_json::json;

    #[test]
    fn test_normalize_stage_name_collapses_shards_only() {
        assert_eq!(normalize_stage_name("prior-feedback-3"), "prior-feedback");
        assert_eq!(normalize_stage_name("verification-1"), "verification");
        assert_eq!(
            normalize_stage_name("post-verification-12"),
            "post-verification"
        );
        assert_eq!(normalize_stage_name("error-handling"), "error-handling");
        assert_eq!(normalize_stage_name("api-contracts"), "api-contracts");
        assert_eq!(normalize_stage_name("pre-screen"), "pre-screen");
    }

    #[tokio::test]
    async fn test_extract_and_window_pipeline_stats_respects_embargo() -> Result<()> {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;

        let now = 1_750_000_000i64;
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, embargo_until) VALUES
             (1, 'public ps', 'Reviewed', ?, NULL),
             (2, 'embargoed ps', 'Reviewed', ?, ?)",
            libsql::params![now - 60, now - 60, now + 3600],
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, status, created_at) VALUES
             (101, 1, 'Reviewed', ?),
             (102, 2, 'Reviewed', ?)",
            libsql::params![now - 30, now - 20],
        )
        .await?;
        db.execute(
            "INSERT INTO tool_usages (review_id, tool_name, output_length, created_at) VALUES
             (101, 'git_grep', 1200, ?)",
            libsql::params![now - 25],
        )
        .await?;

        let sample_json = json!({
            "findings": [
                {"severity": "High", "preexisting": false},
                {"severity": "Medium", "preexisting": true}
            ],
            "review_map": {
                "project": "linux",
                "stages": [
                    {"name": "goal", "skipped": false, "turns": 2, "tokens_in": 1000, "tokens_out": 100, "tokens_cached": 600, "concerns_count": 2, "dismissed_count": 1},
                    {"name": "locking", "skipped": false, "turns": 3, "tokens_in": 2000, "tokens_out": 200, "tokens_cached": 1200, "concerns_count": 1, "dismissed_count": 0},
                    {"name": "security", "skipped": true, "turns": 0, "tokens_in": 0, "tokens_out": 0, "tokens_cached": 0, "concerns_count": 0, "dismissed_count": 0}
                ],
                "raw_concerns": [{"id": "C1"}, {"id": "C2"}, {"id": "C3"}],
                "raw_dismissed_concerns": [{"id": "D1"}],
                "deduplicated_clusters": [{"id": "G1"}, {"id": "G2"}],
                "verification": {
                    "findings": [{"id": "VF1"}],
                    "inconclusive_concerns": [{"id": "I1"}],
                    "dismissed_concerns": [{"id": "VD1"}]
                },
                "post_verification": {
                    "findings": [{"id": "PVF1"}],
                    "dismissed_concerns": []
                },
                "prior_resolutions": [
                    {"origin_kind": "human_reviewer", "verdict": "still_unaddressed", "v_n_outcome": "confirmed_finding", "also_found_by_sashiko": true},
                    {"origin_kind": "sashiko_review", "verdict": "addressed_in_vN", "v_n_outcome": "addressed", "also_found_by_sashiko": true}
                ],
                "threads": [
                    {"outcome": "finding", "stages": ["goal", "locking"]},
                    {"outcome": "preexisting_concern", "stages": ["goal"]},
                    {"outcome": "dismissed_1b", "stages": ["locking"]}
                ]
            }
        });

        db.record_completed_review_telemetry(
            101,
            1,
            "gemini",
            "gemini-2.5-pro",
            "Reviewed",
            false,
            3000,
            300,
            1800,
            Some(&sample_json),
        )
        .await?;
        db.record_completed_review_telemetry(
            102,
            2,
            "gemini",
            "gemini-2.5-pro",
            "Reviewed",
            false,
            3000,
            300,
            1800,
            Some(&sample_json),
        )
        .await?;

        // Embargoed patchset 2 must be excluded at `now`, leaving only review 101.
        let multi = db.get_pipeline_multi_window_stats(now).await?;
        let w100 = &multi.windows[0];
        assert_eq!(w100.reviews_count, 1);
        assert_eq!(w100.findings_patch, 1);
        assert_eq!(w100.findings_preexisting, 1);
        assert_eq!(w100.stages["goal"].confirmed_findings, 2);
        assert_eq!(w100.stages["goal"].exclusive_findings, 1);
        assert_eq!(w100.stages["locking"].confirmed_findings, 1);
        assert_eq!(w100.stages["locking"].exclusive_findings, 0);
        assert_eq!(w100.stages["locking"].dismissed_in_verify, 1);
        assert_eq!(w100.stages["security"].skipped, 1);
        assert_eq!(
            w100.overlap
                .get(&("goal".to_string(), "locking".to_string())),
            Some(&1)
        );
        assert_eq!(
            w100.overlap.get(&("goal".to_string(), "goal".to_string())),
            Some(&1)
        );
        assert_eq!(w100.tools.get("git_grep"), Some(&(1, 1200)));
        assert_eq!(w100.prior.hm_unaddressed, 1);
        assert_eq!(w100.prior.hm_also_found, 1);
        assert_eq!(w100.prior.pf_addressed, 1);

        // Once embargo expires (`now + 3601`), both reviews are included.
        let unlocked = db.get_pipeline_multi_window_stats(now + 3601).await?;
        assert_eq!(unlocked.windows[0].reviews_count, 2);

        Ok(())
    }

    #[test]
    fn test_extract_excludes_stage1_self_dismissed_and_tracks_performance_stage() {
        let review_json = json!({
            "findings": [{"severity": "High", "preexisting": false}],
            "review_map": {
                "project": "linux",
                "stages": [
                    {"name": "performance", "skipped": false, "turns": 4, "tokens_in": 4000, "tokens_out": 500, "tokens_cached": 2000, "concerns_count": 1, "dismissed_count": 2},
                    {"name": "locking", "skipped": false, "turns": 2, "tokens_in": 1000, "tokens_out": 100, "tokens_cached": 500, "concerns_count": 1, "dismissed_count": 1},
                    {"name": "related-preexisting-check-2", "skipped": false, "turns": 1, "tokens_in": 300, "tokens_out": 50, "tokens_cached": 100, "concerns_count": 0, "dismissed_count": 0}
                ],
                "raw_concerns": [{"id": "C1"}, {"id": "C2"}],
                "raw_dismissed_concerns": [{"id": "D1"}, {"id": "D2"}, {"id": "D3"}],
                "deduplicated_clusters": [{"id": "G1"}, {"id": "G2"}],
                "verification": {
                    "findings": [{"id": "VF1"}],
                    "inconclusive_concerns": [],
                    "dismissed_concerns": [{"id": "VD1"}, {"id": "VD2"}, {"id": "VD3"}]
                },
                "threads": [
                    {"outcome": "finding", "stages": ["performance"], "raw_concern_ids": ["C1"], "raw_dismissed_ids": []},
                    {"outcome": "preexisting_concern", "stages": ["performance"], "raw_concern_ids": ["C3"], "raw_dismissed_ids": []},
                    {"outcome": "dismissed_1b", "stages": ["locking"], "raw_concern_ids": ["C2"], "raw_dismissed_ids": []},
                    {"outcome": "dismissed_1b", "stages": ["performance"], "raw_concern_ids": [], "raw_dismissed_ids": ["D1", "D2"]},
                    {"outcome": "dismissed_1b", "stages": ["locking"], "raw_concern_ids": [], "raw_dismissed_ids": ["D3"]}
                ]
            }
        });
        let summary = extract_review_pipeline_summary(ExtractSummaryInput {
            review_id: 99,
            patchset_id: 10,
            created_at: 1000,
            completed_at: 1020,
            provider: "gemini",
            model: "gemini-2.5-pro",
            status: "Reviewed",
            inherited: false,
            tokens_in: 5300,
            tokens_out: 650,
            tokens_cached: 2600,
            review_json: Some(&review_json),
            tool_rows: &[],
        });

        // Only C2 (real concern rejected in 1b) counts; D1/D2/D3 were Stage-1 self-dismissed.
        assert_eq!(summary.dismissed_1b, 1);
        assert_eq!(summary.findings_patch, 1);
        assert_eq!(summary.findings_preexisting, 1);
        let stages: Vec<CompactStageStat> = serde_json::from_str(&summary.stages_json).unwrap();
        let perf = stages.iter().find(|s| s.stage == "performance").unwrap();
        assert_eq!(perf.executed, 1);
        assert_eq!(perf.confirmed_findings, 2);
        assert_eq!(perf.exclusive_findings, 2);
        assert_eq!(perf.dismissed_in_verify, 0);

        let lock = stages.iter().find(|s| s.stage == "locking").unwrap();
        assert_eq!(lock.dismissed_in_verify, 1);

        assert!(
            stages
                .iter()
                .any(|s| s.stage == "related-preexisting-check")
        );
    }
}

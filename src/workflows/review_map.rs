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

//! Structured Review Map builder for single-screen lineage visualization.
//!
//! Assembles the `review_map` JSON payload from [`LinuxPatchReviewState`] and
//! [`StageRunRecord`] telemetry after workflow execution completes.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::project::ProjectId;
use crate::workflow::StageRunRecord;
use crate::workflows::gcc_patch_review;
use crate::workflows::guard::sanitize_prompt_relpath;
use crate::workflows::linux_patch_review::{self, LinuxPatchReviewState};
use crate::workflows::qemu_patch_review;
use crate::workflows::sashiko_patch_review;

fn push_unique(list: &mut Vec<String>, val: &str) {
    let trimmed = val.trim();
    if !trimmed.is_empty() && !list.iter().any(|existing| existing == trimmed) {
        list.push(trimmed.to_string());
    }
}

fn extract_string_array(item: &Value, field: &str) -> Vec<String> {
    item.get(field)
        .and_then(Value::as_array)
        .map(|arr| {
            let mut out = Vec::new();
            for v in arr {
                if let Some(s) = v.as_str() {
                    push_unique(&mut out, s);
                }
            }
            out
        })
        .unwrap_or_default()
}

fn split_raw_ids(ids: &[String]) -> (Vec<String>, Vec<String>) {
    let mut concerns = Vec::new();
    let mut dismissed = Vec::new();
    for id in ids {
        if id.starts_with('C') {
            push_unique(&mut concerns, id);
        } else if id.starts_with('D') {
            push_unique(&mut dismissed, id);
        }
    }
    (concerns, dismissed)
}

fn collect_raw_ids_from_pv_item(
    item: &Value,
    inconclusive_concerns: &[Value],
) -> (Vec<String>, Vec<String>) {
    let mut raw_ids = extract_string_array(item, "raw_source_ids");
    let i_ids = extract_string_array(item, "source_ids");
    for iid in &i_ids {
        if iid.starts_with('C') || iid.starts_with('D') {
            push_unique(&mut raw_ids, iid);
            continue;
        }
        if let Some(ic) = inconclusive_concerns
            .iter()
            .find(|ic| ic.get("id").and_then(Value::as_str) == Some(iid.as_str()))
        {
            for sid in extract_string_array(ic, "source_ids") {
                push_unique(&mut raw_ids, &sid);
            }
        }
    }
    split_raw_ids(&raw_ids)
}

fn severity_rank(sev: &str) -> u8 {
    match sev.trim().to_ascii_lowercase().as_str() {
        "critical" => 5,
        "high" => 4,
        "medium" => 3,
        "low" => 2,
        "unknown" => 1,
        _ => 0,
    }
}

fn outcome_rank(outcome: &str) -> u8 {
    match outcome {
        "finding" => 6,
        "preexisting_concern" => 5,
        "dismissed_post_verification" => 4,
        "unresolved_inconclusive" => 3,
        "dismissed_1b" => 2,
        "unverified_concern" => 1,
        _ => 0,
    }
}

fn item_title(item: &Value) -> String {
    for key in ["problem", "description", "type"] {
        if let Some(s) = item
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    "Unnamed candidate".to_string()
}

fn item_type(item: &Value, state: &LinuxPatchReviewState) -> String {
    if let Some(t) = item
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return t.to_string();
    }
    let mut candidate_ids = extract_string_array(item, "source_ids");
    for rid in extract_string_array(item, "raw_source_ids") {
        push_unique(&mut candidate_ids, &rid);
    }
    for cid in &candidate_ids {
        if let Some(ic) = state
            .inconclusive_concerns
            .iter()
            .find(|ic| ic.get("id").and_then(Value::as_str) == Some(cid.as_str()))
        {
            if let Some(t) = ic
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return t.to_string();
            }
            for sid in extract_string_array(ic, "source_ids") {
                if let Some(raw) = state
                    .all_concerns
                    .iter()
                    .chain(state.all_dismissed_concerns.iter())
                    .find(|r| r.get("id").and_then(Value::as_str) == Some(sid.as_str()))
                    && let Some(t) = raw
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    return t.to_string();
                }
            }
        }
        if let Some(raw) = state
            .all_concerns
            .iter()
            .chain(state.all_dismissed_concerns.iter())
            .find(|r| r.get("id").and_then(Value::as_str) == Some(cid.as_str()))
            && let Some(t) = raw
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return t.to_string();
        }
    }
    String::new()
}

fn item_severity(item: &Value, inconclusive_concerns: &[Value]) -> String {
    for key in ["severity", "estimated_severity"] {
        if let Some(s) = item
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    for iid in extract_string_array(item, "source_ids") {
        if let Some(ic) = inconclusive_concerns
            .iter()
            .find(|ic| ic.get("id").and_then(Value::as_str) == Some(iid.as_str()))
            && let Some(s) = ic
                .get("estimated_severity")
                .or_else(|| ic.get("severity"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    String::new()
}

fn item_locations(item: &Value, state: &LinuxPatchReviewState) -> Vec<Value> {
    if let Some(arr) = item
        .get("locations")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        return arr.clone();
    }
    let mut candidate_ids = extract_string_array(item, "source_ids");
    for rid in extract_string_array(item, "raw_source_ids") {
        push_unique(&mut candidate_ids, &rid);
    }
    for cid in &candidate_ids {
        if let Some(ic) = state
            .inconclusive_concerns
            .iter()
            .find(|ic| ic.get("id").and_then(Value::as_str) == Some(cid.as_str()))
            && let Some(arr) = ic
                .get("locations")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
        {
            return arr.clone();
        }
        if let Some(raw) = state
            .all_concerns
            .iter()
            .chain(state.all_dismissed_concerns.iter())
            .find(|r| r.get("id").and_then(Value::as_str) == Some(cid.as_str()))
            && let Some(arr) = raw
                .get("locations")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
        {
            return arr.clone();
        }
    }
    Vec::new()
}

fn static_stage_guides(project: ProjectId, stage_name: &str) -> Vec<String> {
    let mut guides = Vec::new();
    let analysis_def = match project {
        ProjectId::Linux => linux_patch_review::analysis_stage_by_name(stage_name),
        ProjectId::Sashiko => sashiko_patch_review::analysis_stage_by_name(stage_name),
        ProjectId::Gcc => gcc_patch_review::analysis_stage_by_name(stage_name),
        ProjectId::Qemu => qemu_patch_review::analysis_stage_by_name(stage_name),
    };
    if let Some(def) = analysis_def {
        for g in def.guides {
            let trimmed = g.trim();
            if sanitize_prompt_relpath(trimmed) {
                push_unique(&mut guides, trimmed);
            }
        }
        return guides;
    }

    match stage_name {
        "pre-screen" => push_unique(&mut guides, "subsystem/subsystem.md"),
        s if s == "verification" || s.starts_with("verification-") => {
            push_unique(&mut guides, "false-positive-guide.md");
            push_unique(&mut guides, "severity.md");
        }
        s if s.starts_with("post-verification") => {
            push_unique(&mut guides, "false-positive-guide.md");
            push_unique(&mut guides, "severity.md");
        }
        "report" => match project {
            ProjectId::Linux | ProjectId::Gcc => push_unique(&mut guides, "inline-template.md"),
            ProjectId::Sashiko => push_unique(&mut guides, "github-summary-template.md"),
            ProjectId::Qemu => push_unique(&mut guides, "report-template.md"),
        },
        _ => {}
    }
    guides
}

fn build_prompts_manifest(
    project: ProjectId,
    state: &LinuxPatchReviewState,
    stage_runs: &[StageRunRecord],
) -> Value {
    let base: Vec<String> = match project {
        ProjectId::Linux => vec![
            "false-positive-guide.md".to_string(),
            "severity.md".to_string(),
        ],
        ProjectId::Sashiko | ProjectId::Gcc => vec![
            "review-core.md".to_string(),
            "false-positive-guide.md".to_string(),
            "severity.md".to_string(),
        ],
        ProjectId::Qemu => vec![
            "review-core.md".to_string(),
            "false-positive-guide.md".to_string(),
            "severity.md".to_string(),
        ],
    };

    let mut pre_screen = Vec::new();
    for g in &state.selected_guides {
        let trimmed = g.trim();
        if sanitize_prompt_relpath(trimmed) {
            push_unique(&mut pre_screen, trimmed);
        }
    }

    let mut stage_guides_map = serde_json::Map::new();
    let mut tool_read_by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for run in stage_runs {
        if run.skipped {
            continue;
        }
        let guides = static_stage_guides(project, &run.name);
        if !guides.is_empty() {
            stage_guides_map.insert(run.name.clone(), json!(guides));
        }
        for read_path in &run.prompts_read {
            let trimmed = read_path.trim();
            if sanitize_prompt_relpath(trimmed) {
                let entry = tool_read_by_path.entry(trimmed.to_string()).or_default();
                push_unique(entry, &run.name);
            }
        }
    }

    let tool_read: Vec<Value> = tool_read_by_path
        .into_iter()
        .map(|(path, stages)| json!({ "path": path, "stages": stages }))
        .collect();

    json!({
        "base": base,
        "pre_screen": pre_screen,
        "stage_guides": stage_guides_map,
        "tool_read": tool_read,
    })
}

fn stage_signal_counts(state: &LinuxPatchReviewState, stage_name: &str) -> (usize, usize) {
    if stage_name == "deduplication" {
        return (state.deduplicated_clusters.len(), 0);
    }
    if stage_name == "verification" || stage_name.starts_with("verification-") {
        let matches_stage = |item: &Value| {
            let origin = item
                .get("verification_stage")
                .or_else(|| item.get("origin"))
                .and_then(Value::as_str);
            match origin {
                Some(s) => s == stage_name,
                None => stage_name == "verification",
            }
        };
        let kept = state
            .verification_findings
            .iter()
            .filter(|f| matches_stage(f))
            .count()
            .saturating_add(
                state
                    .inconclusive_concerns
                    .iter()
                    .filter(|h| matches_stage(h))
                    .count(),
            );
        let dismissed = state
            .verification_dismissed
            .iter()
            .filter(|d| matches_stage(d))
            .count();
        return (kept, dismissed);
    }
    if stage_name.starts_with("post-verification") {
        let confirmed = state
            .post_verification_findings
            .iter()
            .filter(|f| {
                f.get("post_verification_stage")
                    .or_else(|| f.get("origin"))
                    .and_then(Value::as_str)
                    == Some(stage_name)
            })
            .count();
        let dismissed = state
            .post_verification_dismissed
            .iter()
            .filter(|d| {
                d.get("post_verification_stage")
                    .or_else(|| d.get("origin"))
                    .and_then(Value::as_str)
                    == Some(stage_name)
            })
            .count();
        return (confirmed, dismissed);
    }

    let concerns = state
        .all_concerns
        .iter()
        .filter(|c| c.get("stage").and_then(Value::as_str) == Some(stage_name))
        .count();
    let dismissed = state
        .all_dismissed_concerns
        .iter()
        .filter(|d| d.get("stage").and_then(Value::as_str) == Some(stage_name))
        .count();
    (concerns, dismissed)
}

fn build_threads(state: &LinuxPatchReviewState) -> Vec<Value> {
    let mut threads = Vec::new();
    let mut accounted_raw_ids = BTreeSet::new();
    let mut resolved_inconclusive_ids = BTreeSet::new();

    // 1. Direct verification findings (Category 1a: VF*)
    for vf in &state.verification_findings {
        let vf_id = vf
            .get("stage_item_id")
            .or_else(|| vf.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let source_ids = extract_string_array(vf, "source_ids");
        let (raw_concerns, raw_dismissed) = split_raw_ids(&source_ids);
        for id in raw_concerns.iter().chain(raw_dismissed.iter()) {
            accounted_raw_ids.insert(id.clone());
        }
        let preexisting = vf
            .get("preexisting")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if preexisting && !state.report_preexisting {
            "preexisting_concern"
        } else {
            "finding"
        };

        threads.push(json!({
            "outcome": outcome,
            "title": item_title(vf),
            "type": item_type(vf, state),
            "severity": item_severity(vf, &state.inconclusive_concerns),
            "preexisting": preexisting,
            "locations": item_locations(vf, state),
            "stages": extract_string_array(vf, "stages"),
            "prompts": extract_string_array(vf, "prompts"),
            "raw_concern_ids": raw_concerns,
            "raw_dismissed_ids": raw_dismissed,
            "verification_kind": "finding_1a",
            "verification_id": vf_id,
            "verification_ids": vf_id.iter().cloned().collect::<Vec<_>>(),
            "post_verification_stage": Value::Null,
            "post_verification_id": Value::Null,
        }));
    }

    // 2a. Post-verification confirmed findings (PVF*)
    for pvf in &state.post_verification_findings {
        let pvf_id = pvf
            .get("stage_item_id")
            .or_else(|| pvf.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let i_ids: Vec<String> = extract_string_array(pvf, "source_ids")
            .into_iter()
            .filter(|id| id.starts_with('I'))
            .collect();
        for iid in &i_ids {
            resolved_inconclusive_ids.insert(iid.clone());
        }
        let (raw_concerns, raw_dismissed) =
            collect_raw_ids_from_pv_item(pvf, &state.inconclusive_concerns);
        for id in raw_concerns.iter().chain(raw_dismissed.iter()) {
            accounted_raw_ids.insert(id.clone());
        }
        let preexisting = pvf
            .get("preexisting")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if preexisting && !state.report_preexisting {
            "preexisting_concern"
        } else {
            "finding"
        };
        let pv_stage = pvf
            .get("post_verification_stage")
            .or_else(|| pvf.get("origin"))
            .and_then(Value::as_str);

        threads.push(json!({
            "outcome": outcome,
            "title": item_title(pvf),
            "type": item_type(pvf, state),
            "severity": item_severity(pvf, &state.inconclusive_concerns),
            "preexisting": preexisting,
            "locations": item_locations(pvf, state),
            "stages": extract_string_array(pvf, "stages"),
            "prompts": extract_string_array(pvf, "prompts"),
            "raw_concern_ids": raw_concerns,
            "raw_dismissed_ids": raw_dismissed,
            "verification_kind": "inconclusive",
            "verification_id": i_ids.first().cloned(),
            "verification_ids": i_ids,
            "post_verification_stage": pv_stage,
            "post_verification_id": pvf_id,
        }));
    }

    // 2b. Inconclusive concerns dismissed in post-verification (PVD*)
    for pvd in &state.post_verification_dismissed {
        let pvd_id = pvd.get("id").and_then(Value::as_str).map(str::to_string);
        let i_ids: Vec<String> = extract_string_array(pvd, "source_ids")
            .into_iter()
            .filter(|id| id.starts_with('I'))
            .collect();
        for iid in &i_ids {
            resolved_inconclusive_ids.insert(iid.clone());
        }
        let (raw_concerns, raw_dismissed) =
            collect_raw_ids_from_pv_item(pvd, &state.inconclusive_concerns);
        for id in raw_concerns.iter().chain(raw_dismissed.iter()) {
            accounted_raw_ids.insert(id.clone());
        }
        let pv_stage = pvd
            .get("post_verification_stage")
            .or_else(|| pvd.get("origin"))
            .and_then(Value::as_str);

        threads.push(json!({
            "outcome": "dismissed_post_verification",
            "title": item_title(pvd),
            "type": item_type(pvd, state),
            "severity": item_severity(pvd, &state.inconclusive_concerns),
            "preexisting": false,
            "locations": item_locations(pvd, state),
            "stages": extract_string_array(pvd, "stages"),
            "prompts": extract_string_array(pvd, "prompts"),
            "raw_concern_ids": raw_concerns,
            "raw_dismissed_ids": raw_dismissed,
            "verification_kind": "inconclusive",
            "verification_id": i_ids.first().cloned(),
            "verification_ids": i_ids,
            "post_verification_stage": pv_stage,
            "post_verification_id": pvd_id,
        }));
    }

    // 2c. Any inconclusive concerns not resolved by post-verification (e.g. partial stage runs)
    for ic in &state.inconclusive_concerns {
        let Some(iid) = ic.get("id").and_then(Value::as_str) else {
            continue;
        };
        if resolved_inconclusive_ids.contains(iid) {
            continue;
        }
        let source_ids = extract_string_array(ic, "source_ids");
        let (raw_concerns, raw_dismissed) = split_raw_ids(&source_ids);
        for id in raw_concerns.iter().chain(raw_dismissed.iter()) {
            accounted_raw_ids.insert(id.clone());
        }
        threads.push(json!({
            "outcome": "unresolved_inconclusive",
            "title": item_title(ic),
            "type": item_type(ic, state),
            "severity": item_severity(ic, &state.inconclusive_concerns),
            "preexisting": ic.get("preexisting").and_then(Value::as_bool).unwrap_or(false),
            "locations": item_locations(ic, state),
            "stages": extract_string_array(ic, "stages"),
            "prompts": extract_string_array(ic, "prompts"),
            "raw_concern_ids": raw_concerns,
            "raw_dismissed_ids": raw_dismissed,
            "verification_kind": "inconclusive",
            "verification_id": iid,
            "verification_ids": [iid],
            "post_verification_stage": ic.get("assigned_stage").and_then(Value::as_str),
            "post_verification_id": Value::Null,
        }));
    }

    // 3. Direct dismissals in verification (Category 1b: VD*)
    for vd in &state.verification_dismissed {
        let vd_id = vd.get("id").and_then(Value::as_str).map(str::to_string);
        let source_ids = extract_string_array(vd, "source_ids");
        let (raw_concerns, raw_dismissed) = split_raw_ids(&source_ids);
        for id in raw_concerns.iter().chain(raw_dismissed.iter()) {
            accounted_raw_ids.insert(id.clone());
        }
        threads.push(json!({
            "outcome": "dismissed_1b",
            "title": item_title(vd),
            "type": item_type(vd, state),
            "severity": "",
            "preexisting": false,
            "locations": item_locations(vd, state),
            "stages": extract_string_array(vd, "stages"),
            "prompts": extract_string_array(vd, "prompts"),
            "raw_concern_ids": raw_concerns,
            "raw_dismissed_ids": raw_dismissed,
            "verification_kind": "dismissed_1b",
            "verification_id": vd_id,
            "verification_ids": vd_id.iter().cloned().collect::<Vec<_>>(),
            "post_verification_stage": Value::Null,
            "post_verification_id": Value::Null,
        }));
    }

    // 4. Fallback for raw items when verification was skipped or bypassed via --stages
    for c in &state.all_concerns {
        let Some(cid) = c.get("id").and_then(Value::as_str) else {
            continue;
        };
        if accounted_raw_ids.contains(cid) {
            continue;
        }
        threads.push(json!({
            "outcome": "unverified_concern",
            "title": item_title(c),
            "type": item_type(c, state),
            "severity": "",
            "preexisting": c.get("preexisting").and_then(Value::as_bool).unwrap_or(false),
            "locations": item_locations(c, state),
            "stages": extract_string_array(c, "stages"),
            "prompts": extract_string_array(c, "prompts"),
            "raw_concern_ids": [cid],
            "raw_dismissed_ids": [],
            "verification_kind": "none",
            "verification_id": Value::Null,
            "verification_ids": [],
            "post_verification_stage": Value::Null,
            "post_verification_id": Value::Null,
        }));
    }

    for d in &state.all_dismissed_concerns {
        let Some(did) = d.get("id").and_then(Value::as_str) else {
            continue;
        };
        if accounted_raw_ids.contains(did) {
            continue;
        }
        threads.push(json!({
            "outcome": "unverified_dismissed",
            "title": item_title(d),
            "type": item_type(d, state),
            "severity": "",
            "preexisting": false,
            "locations": item_locations(d, state),
            "stages": extract_string_array(d, "stages"),
            "prompts": extract_string_array(d, "prompts"),
            "raw_concern_ids": [],
            "raw_dismissed_ids": [did],
            "verification_kind": "none",
            "verification_id": Value::Null,
            "verification_ids": [],
            "post_verification_stage": Value::Null,
            "post_verification_id": Value::Null,
        }));
    }

    threads.sort_by(|a, b| {
        let out_a = a.get("outcome").and_then(Value::as_str).unwrap_or("");
        let out_b = b.get("outcome").and_then(Value::as_str).unwrap_or("");
        let sev_a = a.get("severity").and_then(Value::as_str).unwrap_or("");
        let sev_b = b.get("severity").and_then(Value::as_str).unwrap_or("");
        outcome_rank(out_b)
            .cmp(&outcome_rank(out_a))
            .then_with(|| severity_rank(sev_b).cmp(&severity_rank(sev_a)))
    });

    for (idx, thread) in threads.iter_mut().enumerate() {
        if let Some(map) = thread.as_object_mut() {
            map.insert(
                "thread_id".to_string(),
                json!(format!("T{}", idx.saturating_add(1))),
            );
        }
    }

    threads
}

/// Builds the complete `review_map` JSON object for persistence in `reviews.result_json`.
pub fn build_review_map(
    project: ProjectId,
    state: &LinuxPatchReviewState,
    stage_runs: &[StageRunRecord],
    history_offset: usize,
) -> Value {
    let prompts = build_prompts_manifest(project, state, stage_runs);

    let stages: Vec<Value> = stage_runs
        .iter()
        .map(|r| {
            let (concerns_count, dismissed_count) = stage_signal_counts(state, &r.name);
            let label = crate::workflows::stage_short_label(project, &r.name).unwrap_or(&r.name);
            json!({
                "name": r.name,
                "label": label,
                "skipped": r.skipped,
                "turns": r.turns,
                "tokens_in": r.tokens_in,
                "tokens_out": r.tokens_out,
                "tokens_cached": r.tokens_cached,
                "prompts_read": r.prompts_read,
                "concerns_count": concerns_count,
                "dismissed_count": dismissed_count,
                "log_start": r.history_start.saturating_add(history_offset),
                "log_end": r.history_end.saturating_add(history_offset),
            })
        })
        .collect();

    let threads = build_threads(state);

    json!({
        "version": 1,
        "project": project.as_str(),
        "prompts": prompts,
        "stages": stages,
        "raw_concerns": state.all_concerns,
        "raw_dismissed_concerns": state.all_dismissed_concerns,
        "deduplicated_clusters": state.deduplicated_clusters,
        "verification": {
            "findings": state.verification_findings,
            "inconclusive_concerns": state.inconclusive_concerns,
            "dismissed_concerns": state.verification_dismissed,
        },
        "post_verification": {
            "findings": state.post_verification_findings,
            "dismissed_concerns": state.post_verification_dismissed,
        },
        "threads": threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_review_map_sorts_threads_and_computes_stage_bounds() {
        let state = LinuxPatchReviewState {
            project: "linux".to_string(),
            selected_guides: vec!["networking.md".to_string()],
            all_concerns: vec![
                json!({
                    "id": "C1",
                    "stage": "goal",
                    "stages": ["goal"],
                    "type": "Logic Defect",
                    "description": "Unchecked return value in foo()",
                    "locations": [{"file": "net/foo.c", "line": 42, "function_or_symbol": "foo"}],
                }),
                json!({
                    "id": "C2",
                    "stage": "locking",
                    "stages": ["locking"],
                    "type": "Concurrency Hazard",
                    "description": "Possible lock inversion in bar()",
                    "locations": [{"file": "net/bar.c", "line": 88, "function_or_symbol": "bar"}],
                }),
            ],
            all_dismissed_concerns: vec![json!({
                "id": "D1",
                "stage": "implementation",
                "stages": ["implementation"],
                "type": "Null Pointer Dereference",
                "description": "Checked by caller",
                "reasoning": "Caller validates ptr != NULL",
                "locations": [{"file": "net/baz.c", "line": 10, "function_or_symbol": "baz"}],
            })],
            verification_findings: vec![json!({
                "id": "VF1",
                "stage_item_id": "VF1",
                "source_ids": ["C1"],
                "problem": "net: unchecked return value in foo()",
                "severity": "High",
                "preexisting": false,
                "stages": ["goal"],
                "prompts": ["networking.md"],
                "locations": [{"file": "net/foo.c", "line": 42, "function_or_symbol": "foo"}],
            })],
            inconclusive_concerns: vec![json!({
                "id": "I1",
                "source_ids": ["C2"],
                "type": "Concurrency Hazard",
                "description": "Possible lock inversion in bar()",
                "estimated_severity": "Medium",
                "assigned_stage": "post-verification-1",
                "stages": ["locking"],
                "locations": [{"file": "net/bar.c", "line": 88, "function_or_symbol": "bar"}],
            })],
            verification_dismissed: vec![json!({
                "id": "VD1",
                "source_ids": ["D1"],
                "type": "Null Pointer Dereference",
                "description": "Checked by caller",
                "reasoning": "Caller validates ptr != NULL",
                "stages": ["implementation"],
                "locations": [{"file": "net/baz.c", "line": 10, "function_or_symbol": "baz"}],
            })],
            post_verification_dismissed: vec![json!({
                "id": "PVD1",
                "source_ids": ["I1"],
                "raw_source_ids": ["C2"],
                "post_verification_stage": "post-verification-1",
                "description": "Lock order is consistent across all callers",
                "reasoning": "All callers acquire lock_a before lock_b",
                "stages": ["locking"],
                "locations": [{"file": "net/bar.c", "line": 88, "function_or_symbol": "bar"}],
            })],
            ..Default::default()
        };

        let stage_runs = vec![
            StageRunRecord {
                name: "goal".to_string(),
                skipped: false,
                turns: 1,
                tokens_in: 100,
                tokens_out: 50,
                tokens_cached: 0,
                prompts_read: vec![],
                history_start: 0,
                history_end: 2,
            },
            StageRunRecord {
                name: "verification".to_string(),
                skipped: false,
                turns: 2,
                tokens_in: 200,
                tokens_out: 80,
                tokens_cached: 0,
                prompts_read: vec!["subsystem/bpf.md".to_string()],
                history_start: 2,
                history_end: 6,
            },
        ];

        let map = build_review_map(ProjectId::Linux, &state, &stage_runs, 1);
        assert_eq!(map["version"], 1);
        assert_eq!(map["project"], "linux");
        assert_eq!(map["stages"][0]["log_start"], 1);
        assert_eq!(map["stages"][0]["log_end"], 3);
        assert_eq!(map["stages"][0]["concerns_count"], 1);
        assert_eq!(map["stages"][1]["concerns_count"], 2);
        assert_eq!(map["stages"][1]["dismissed_count"], 1);

        let threads = map["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 3);
        // Confirmed finding first, then post-verification dismissal, then 1b dismissal
        assert_eq!(threads[0]["thread_id"], "T1");
        assert_eq!(threads[0]["outcome"], "finding");
        assert_eq!(threads[0]["type"], "Logic Defect");
        assert_eq!(threads[0]["raw_concern_ids"], json!(["C1"]));

        assert_eq!(threads[1]["thread_id"], "T2");
        assert_eq!(threads[1]["outcome"], "dismissed_post_verification");
        assert_eq!(threads[1]["verification_id"], "I1");
        assert_eq!(threads[1]["post_verification_id"], "PVD1");
        assert_eq!(threads[1]["raw_concern_ids"], json!(["C2"]));

        assert_eq!(threads[2]["thread_id"], "T3");
        assert_eq!(threads[2]["outcome"], "dismissed_1b");
        assert_eq!(threads[2]["verification_id"], "VD1");
        assert_eq!(threads[2]["raw_dismissed_ids"], json!(["D1"]));
    }
}

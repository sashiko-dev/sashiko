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

use std::collections::BTreeSet;

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

fn is_raw_concern_id(id: &str) -> bool {
    id.starts_with('C') || id.starts_with("PC")
}

fn is_raw_dismissed_id(id: &str) -> bool {
    id.starts_with('D') || id.starts_with("PD")
}

fn split_raw_ids(ids: &[String]) -> (Vec<String>, Vec<String>) {
    let mut concerns = Vec::new();
    let mut dismissed = Vec::new();
    for id in ids {
        if is_raw_concern_id(id) {
            push_unique(&mut concerns, id);
        } else if is_raw_dismissed_id(id) {
            push_unique(&mut dismissed, id);
        }
    }
    (concerns, dismissed)
}

fn collect_raw_ids_from_item(item: &Value) -> (Vec<String>, Vec<String>) {
    let mut combined = extract_string_array(item, "raw_source_ids");
    for sid in extract_string_array(item, "source_ids") {
        push_unique(&mut combined, &sid);
    }
    split_raw_ids(&combined)
}

fn collect_raw_ids_from_pv_item(
    item: &Value,
    inconclusive_concerns: &[Value],
) -> (Vec<String>, Vec<String>) {
    let mut raw_ids = extract_string_array(item, "raw_source_ids");
    let i_ids = extract_string_array(item, "source_ids");
    for iid in &i_ids {
        if is_raw_concern_id(iid) || is_raw_dismissed_id(iid) {
            push_unique(&mut raw_ids, iid);
            continue;
        }
        if let Some(ic) = inconclusive_concerns
            .iter()
            .find(|ic| ic.get("id").and_then(Value::as_str) == Some(iid.as_str()))
        {
            for sid in extract_string_array(ic, "raw_source_ids") {
                push_unique(&mut raw_ids, &sid);
            }
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

fn extract_xml_attr<'a>(line: &'a str, tag_prefix: &str, attr: &str) -> Option<&'a str> {
    let trimmed = line.trim();
    if !trimmed.starts_with(tag_prefix) {
        return None;
    }
    let needle = format!("{attr}=\"");
    let start = trimmed.find(&needle)? + needle.len();
    let rest = &trimmed[start..];
    let end = rest.find('"')?;
    Some(rest[..end].trim()).filter(|s| !s.is_empty())
}

fn extract_maintainer_notes(block: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for line in block.lines() {
        if let Some(sub) = extract_xml_attr(line, "<maintainer_note ", "subsystem") {
            push_unique(&mut notes, sub);
        }
    }
    notes
}

fn extract_code_notes(block: &str) -> (Option<String>, Vec<String>) {
    let mut set_name = None;
    let mut units = Vec::new();
    for line in block.lines() {
        if set_name.is_none()
            && let Some(s) = extract_xml_attr(line, "<code_notes ", "set")
        {
            set_name = Some(s.to_string());
        }
        if let Some(unit) = extract_xml_attr(line, "<unit_note ", "unit") {
            push_unique(&mut units, unit);
        }
    }
    (set_name, units)
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
    let maintainer_notes = extract_maintainer_notes(&state.maintainer_notes);
    let (code_notes_set, code_notes) = extract_code_notes(&state.code_notes);

    let mut stage_guides_map = serde_json::Map::new();
    for run in stage_runs {
        if run.skipped {
            continue;
        }
        let guides = static_stage_guides(project, &run.name);
        if !guides.is_empty() {
            stage_guides_map.insert(run.name.clone(), json!(guides));
        }
    }

    json!({
        "base": base,
        "pre_screen": pre_screen,
        "maintainer_notes": maintainer_notes,
        "code_notes": code_notes,
        "code_notes_set": code_notes_set,
        "stage_guides": stage_guides_map,
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
        let (raw_concerns, raw_dismissed) = collect_raw_ids_from_item(vf);
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
            "prior_source_ids": extract_string_array(vf, "prior_source_ids"),
            "prior_attribution": vf.get("prior_attribution").cloned().unwrap_or(Value::Null),
            "prior_attribution_cleared": vf
                .get("prior_attribution_cleared")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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
            "prior_source_ids": extract_string_array(pvf, "prior_source_ids"),
            "prior_attribution": pvf.get("prior_attribution").cloned().unwrap_or(Value::Null),
            "prior_attribution_cleared": pvf
                .get("prior_attribution_cleared")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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
            "prior_source_ids": extract_string_array(pvd, "prior_source_ids"),
            "prior_attribution": pvd.get("prior_attribution").cloned().unwrap_or(Value::Null),
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
        let (raw_concerns, raw_dismissed) = collect_raw_ids_from_item(ic);
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
            "prior_source_ids": extract_string_array(ic, "prior_source_ids"),
            "prior_attribution": ic.get("prior_attribution").cloned().unwrap_or(Value::Null),
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
        let (raw_concerns, raw_dismissed) = collect_raw_ids_from_item(vd);
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
            "prior_source_ids": extract_string_array(vd, "prior_source_ids"),
            "prior_attribution": Value::Null,
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
            "prior_source_ids": extract_string_array(c, "prior_source_ids"),
            "prior_attribution": c.get("prior_attribution").cloned().unwrap_or(Value::Null),
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
            "prior_source_ids": extract_string_array(d, "prior_source_ids"),
            "prior_attribution": Value::Null,
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

fn is_fresh_stage_name(name: &str) -> bool {
    let s = name.trim();
    !s.is_empty()
        && !crate::workflows::prior_feedback::is_prior_feedback_stage_name(s)
        && !s.starts_with("verification")
        && !s.starts_with("post-verification")
        && !s.starts_with("prior-attribution")
        && s != "deduplication"
        && s != "report"
}

fn thread_matches_prior_id(thread: &Value, pid: &str) -> bool {
    if pid.is_empty() {
        return false;
    }
    let attr_cleared = thread
        .get("prior_attribution_cleared")
        .and_then(Value::as_bool)
        == Some(true);
    if !attr_cleared
        && extract_string_array(thread, "prior_source_ids")
            .iter()
            .any(|s| s == pid)
    {
        return true;
    }
    let dot_prefix = format!("{pid}.");
    thread
        .get("prior_attribution")
        .and_then(|a| a.get("evidence"))
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter().any(|ev| {
                ev.get("source_ref")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|r| r == pid || r.starts_with(&dot_prefix))
            })
        })
}

fn thread_also_found_by_sashiko(thread: &Value) -> bool {
    if let Some(fb) = thread
        .get("prior_attribution")
        .and_then(|a| a.get("found_by"))
        .and_then(Value::as_str)
    {
        if fb == "both" || fb == "fresh" {
            return true;
        }
        if fb == "prior_feedback" {
            return false;
        }
    }
    let raw_concerns = extract_string_array(thread, "raw_concern_ids");
    if raw_concerns.iter().any(|cid| cid.starts_with('C')) {
        return true;
    }
    if raw_concerns.iter().any(|cid| cid.starts_with("PC")) {
        return false;
    }
    if extract_string_array(thread, "raw_dismissed_ids")
        .iter()
        .any(|did| did.starts_with('D'))
    {
        return true;
    }
    extract_string_array(thread, "stages")
        .iter()
        .any(|s| is_fresh_stage_name(s))
}

fn compute_v_n_outcome(
    res: &Value,
    matched_threads: &[&Value],
    cleared_by_attribution: bool,
) -> &'static str {
    match res.get("verdict").and_then(Value::as_str).unwrap_or("") {
        "addressed_in_vN" => "addressed",
        "disproven_by_code" => "disproven",
        "settled_by_authority" => "settled",
        "nit_or_untrusted_unverifiable" => "dismissed",
        "still_unaddressed" => {
            if matched_threads.iter().any(|t| {
                matches!(
                    t.get("outcome").and_then(Value::as_str),
                    Some("finding" | "preexisting_concern")
                )
            }) {
                "confirmed_finding"
            } else if cleared_by_attribution
                || matched_threads.iter().any(|t| {
                    t.get("outcome")
                        .and_then(Value::as_str)
                        .is_some_and(|o| o.starts_with("dismissed"))
                })
            {
                "dismissed_in_verification"
            } else {
                "unverified"
            }
        }
        _ => "dismissed",
    }
}

fn extract_technical_tokens(text: &str) -> BTreeSet<String> {
    const STOPWORDS: &[&str] = &[
        "also",
        "been",
        "both",
        "call",
        "called",
        "calling",
        "calls",
        "change",
        "changed",
        "changes",
        "code",
        "comment",
        "confirmed",
        "could",
        "does",
        "each",
        "from",
        "function",
        "have",
        "into",
        "instead",
        "line",
        "lines",
        "make",
        "more",
        "must",
        "need",
        "needs",
        "noted",
        "only",
        "other",
        "patch",
        "patchset",
        "prior",
        "pointed",
        "reported",
        "review",
        "reviewer",
        "same",
        "should",
        "since",
        "still",
        "such",
        "Suggested",
        "that",
        "their",
        "them",
        "then",
        "there",
        "these",
        "they",
        "this",
        "through",
        "under",
        "used",
        "uses",
        "using",
        "version",
        "were",
        "what",
        "when",
        "where",
        "which",
        "while",
        "with",
        "without",
        "would",
    ];
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map(|tok| tok.trim_matches('_').to_ascii_lowercase())
        .filter(|tok| tok.len() >= 4 && !STOPWORDS.contains(&tok.as_str()))
        .collect()
}

fn is_code_identifier_token(tok: &str) -> bool {
    tok.contains('_') || tok.bytes().any(|b| b.is_ascii_digit())
}

fn pf_resolution_matches_hm(pf: &Value, hm: &Value) -> bool {
    let pf_verdict = pf.get("verdict").and_then(Value::as_str).unwrap_or("");
    let hm_verdict = hm.get("verdict").and_then(Value::as_str).unwrap_or("");
    if pf_verdict.is_empty() || pf_verdict != hm_verdict {
        return false;
    }
    let pf_sum = pf.get("summary").and_then(Value::as_str).unwrap_or("");
    let hm_sum = hm.get("summary").and_then(Value::as_str).unwrap_or("");
    let pf_reas = pf.get("reasoning").and_then(Value::as_str).unwrap_or("");
    let hm_reas = hm.get("reasoning").and_then(Value::as_str).unwrap_or("");

    let pf_sum_tokens = extract_technical_tokens(pf_sum);
    let hm_sum_tokens = extract_technical_tokens(hm_sum);
    let shared_sum = pf_sum_tokens.intersection(&hm_sum_tokens).count();

    let pf_all_tokens = extract_technical_tokens(&format!("{pf_sum} {pf_reas}"));
    let hm_all_tokens = extract_technical_tokens(&format!("{hm_sum} {hm_reas}"));
    let shared_code_ids = pf_all_tokens
        .intersection(&hm_all_tokens)
        .filter(|tok| is_code_identifier_token(tok))
        .count();
    let shared_all = pf_all_tokens.intersection(&hm_all_tokens).count();
    let min_all = pf_all_tokens.len().min(hm_all_tokens.len());

    shared_code_ids >= 1
        && (shared_sum >= 2
            || (shared_sum >= 1 && shared_code_ids >= 3 && shared_all * 100 >= min_all * 20)
            || (shared_code_ids >= 2 && shared_all >= 4 && shared_all * 100 >= min_all * 34))
}

fn pf_resolution_duplicates_sashiko_hm(pf: &Value, hm: &Value) -> bool {
    let pf_ver = pf.get("version").and_then(Value::as_u64).unwrap_or(0);
    let hm_ver = hm.get("version").and_then(Value::as_u64).unwrap_or(0);
    if pf_ver == 0 || pf_ver != hm_ver {
        return false;
    }
    let pf_sum = pf.get("summary").and_then(Value::as_str).unwrap_or("");
    let hm_sum = hm.get("summary").and_then(Value::as_str).unwrap_or("");
    let pf_reas = pf.get("reasoning").and_then(Value::as_str).unwrap_or("");
    let hm_reas = hm.get("reasoning").and_then(Value::as_str).unwrap_or("");
    let pf_tokens = extract_technical_tokens(&format!("{pf_sum} {pf_reas}"));
    let hm_tokens = extract_technical_tokens(&format!("{hm_sum} {hm_reas}"));
    let shared_code_ids = pf_tokens
        .intersection(&hm_tokens)
        .filter(|tok| is_code_identifier_token(tok))
        .count();
    let shared = pf_tokens.intersection(&hm_tokens).count();
    let min_len = pf_tokens.len().min(hm_tokens.len());
    min_len > 0
        && ((shared >= 7 && shared * 100 >= min_len * 30)
            || (shared_code_ids >= 1 && shared >= 4 && shared * 100 >= min_len * 20))
}

/// Stamps `found_by` on every final finding and enriches `prior_resolutions`
/// with `also_found_by_sashiko` and `v_n_outcome` after verification and
/// attribution complete.
pub fn enrich_prior_resolutions(state: &mut LinuxPatchReviewState) {
    for finding in &mut state.findings {
        let fb = crate::workflows::prior_attribution::found_by(finding).as_str();
        if let Some(map) = finding.as_object_mut() {
            map.insert("found_by".to_string(), json!(fb));
        }
    }
    if state.prior_resolutions.is_empty() {
        return;
    }
    let threads = build_threads(state);
    let pf_resolutions: Vec<Value> = state
        .prior_resolutions
        .iter()
        .filter(|r| {
            r.get("prior_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.trim().starts_with("PF"))
                || r.get("origin_kind").and_then(Value::as_str) == Some("sashiko_review")
        })
        .cloned()
        .collect();
    let strict_pf_resolutions: Vec<Value> = state
        .prior_resolutions
        .iter()
        .filter(|r| {
            r.get("prior_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.trim().starts_with("PF"))
        })
        .cloned()
        .collect();
    for res in &mut state.prior_resolutions {
        let pid = res
            .get("prior_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let matched: Vec<&Value> = threads
            .iter()
            .filter(|t| thread_matches_prior_id(t, &pid))
            .collect();
        let cleared_by_attribution = matched.is_empty()
            && !pid.is_empty()
            && threads.iter().any(|t| {
                t.get("prior_attribution_cleared").and_then(Value::as_bool) == Some(true)
                    && extract_string_array(t, "prior_source_ids")
                        .iter()
                        .any(|s| s == &pid)
            });
        let is_pf = pid.starts_with("PF")
            || res.get("origin_kind").and_then(Value::as_str) == Some("sashiko_review");
        let also_found = is_pf
            || matched.iter().copied().any(thread_also_found_by_sashiko)
            || pf_resolutions
                .iter()
                .any(|pf| pf_resolution_matches_hm(pf, res));
        let outcome = compute_v_n_outcome(res, &matched, cleared_by_attribution);
        if let Some(map) = res.as_object_mut() {
            map.insert("also_found_by_sashiko".to_string(), json!(also_found));
            map.insert("v_n_outcome".to_string(), json!(outcome));
        }
    }
    // Drop HM* resolutions rooted at Sashiko's own Lore email (kept in the
    // prompt only so the LLM could read the patch author's reply) when that
    // ancestor version already has structured PF* resolutions.
    state.prior_resolutions.retain(|res| {
        let is_sashiko_hm = res
            .get("prior_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.trim().starts_with("HM"))
            && res.get("origin_kind").and_then(Value::as_str) == Some("sashiko_review");
        if !is_sashiko_hm {
            return true;
        }
        let hm_ver = res.get("version").and_then(Value::as_u64).unwrap_or(0);
        let same_ver_pfs: Vec<&Value> = strict_pf_resolutions
            .iter()
            .filter(|pf| pf.get("version").and_then(Value::as_u64) == Some(hm_ver))
            .collect();
        !(same_ver_pfs.len() == 1
            || same_ver_pfs
                .iter()
                .any(|pf| pf_resolution_duplicates_sashiko_hm(pf, res)))
    });
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
        "prior_resolutions": state.prior_resolutions,
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

    #[test]
    fn test_build_review_map_tracks_prior_feedback_lineage_and_resolutions() {
        let state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![json!({
                "stage": "prior-feedback",
                "prior_id": "PF1",
                "verdict": "addressed_in_vN",
                "reasoning": "Fixed in v2",
                "version": 1,
                "thread_participants": ["Sashiko"],
                "reference_url": null,
            })],
            all_concerns: vec![json!({
                "id": "PC1",
                "stage": "prior-feedback",
                "stages": ["prior-feedback"],
                "type": "Locking",
                "description": "Sleeping under spinlock",
                "prior_source_ids": ["HM1"],
                "prior_attribution": {
                    "origin_kind": "human_reviewer",
                    "originally_raised_by": "Jan Kara <jack@suse.cz>",
                    "originally_raised_in_version": 1,
                    "reference_url": "https://lore.kernel.org/r/msg@suse.cz"
                }
            })],
            all_dismissed_concerns: vec![json!({
                "id": "PD1",
                "stage": "prior-feedback",
                "stages": ["prior-feedback"],
                "type": "Hardware",
                "description": "Hardware latches CR2",
                "reasoning": "Settled by maintainer",
                "prior_source_ids": ["HM2"]
            })],
            post_verification_findings: vec![json!({
                "id": "PVF1",
                "stage_item_id": "PVF1",
                "source_ids": ["I1"],
                "raw_source_ids": ["PC1"],
                "prior_source_ids": ["HM1"],
                "prior_attribution": {
                    "origin_kind": "human_reviewer",
                    "originally_raised_by": "Jan Kara <jack@suse.cz>",
                    "originally_raised_in_version": 1,
                    "reference_url": "https://lore.kernel.org/r/msg@suse.cz"
                },
                "post_verification_stage": "post-verification-1",
                "problem": "fs: sleeping under spinlock",
                "severity": "High",
                "preexisting": false,
                "stages": ["prior-feedback"]
            })],
            verification_dismissed: vec![json!({
                "id": "VD1",
                "source_ids": ["G2"],
                "raw_source_ids": ["PD1"],
                "prior_source_ids": ["HM2"],
                "description": "Hardware latches CR2",
                "reasoning": "Settled by maintainer",
                "stages": ["prior-feedback"]
            })],
            ..Default::default()
        };

        let map = build_review_map(ProjectId::Linux, &state, &[], 0);
        assert_eq!(map["prior_resolutions"].as_array().unwrap().len(), 1);
        let threads = map["threads"].as_array().unwrap();
        // Both PC1 and PD1 are accounted for via PVF1 and VD1 (no duplicate fallback rows).
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0]["raw_concern_ids"], json!(["PC1"]));
        assert_eq!(
            threads[0]["prior_attribution"]["originally_raised_by"],
            "Jan Kara <jack@suse.cz>"
        );
        assert_eq!(threads[1]["raw_dismissed_ids"], json!(["PD1"]));
    }

    #[test]
    fn test_enrich_prior_resolutions_marks_missed_vs_also_found_by_sashiko() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM1",
                    "origin_kind": "human_reviewer",
                    "summary": "Sleeping under spinlock in foo_lock()",
                    "verdict": "still_unaddressed",
                    "reasoning": "foo_lock() still calls mutex_lock() under spin_lock().",
                    "version": 1,
                    "thread_participants": ["Jan Kara <jack@suse.cz>"],
                }),
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM2",
                    "origin_kind": "human_reviewer",
                    "summary": "Missing usb_put_dev() on probe error path",
                    "verdict": "still_unaddressed",
                    "reasoning": "usb_put_dev() is still missing before return err.",
                    "version": 1,
                    "thread_participants": ["Oliver Neukum <oneukum@suse.com>"],
                }),
            ],
            findings: vec![
                json!({
                    "id": "VF1",
                    "raw_source_ids": ["PC1"],
                    "prior_source_ids": ["HM1"],
                    "stages": ["prior-feedback"],
                    "problem": "fs: sleeping under spinlock",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "id": "VF2",
                    "raw_source_ids": ["C1", "PC2"],
                    "prior_source_ids": ["HM2"],
                    "stages": ["resources", "prior-feedback"],
                    "problem": "net: mctp: missing usb_put_dev()",
                    "severity": "Medium",
                    "preexisting": false,
                }),
            ],
            verification_findings: vec![
                json!({
                    "id": "VF1",
                    "raw_source_ids": ["PC1"],
                    "prior_source_ids": ["HM1"],
                    "stages": ["prior-feedback"],
                    "problem": "fs: sleeping under spinlock",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "id": "VF2",
                    "raw_source_ids": ["C1", "PC2"],
                    "prior_source_ids": ["HM2"],
                    "stages": ["resources", "prior-feedback"],
                    "problem": "net: mctp: missing usb_put_dev()",
                    "severity": "Medium",
                    "preexisting": false,
                }),
            ],
            ..Default::default()
        };

        enrich_prior_resolutions(&mut state);
        assert_eq!(state.findings[0]["found_by"], "prior_feedback");
        assert_eq!(state.findings[1]["found_by"], "both");
        assert_eq!(state.prior_resolutions[0]["also_found_by_sashiko"], false);
        assert_eq!(
            state.prior_resolutions[0]["v_n_outcome"],
            "confirmed_finding"
        );
        assert_eq!(state.prior_resolutions[1]["also_found_by_sashiko"], true);
        assert_eq!(
            state.prior_resolutions[1]["v_n_outcome"],
            "confirmed_finding"
        );
    }

    #[test]
    fn test_enrich_prior_resolutions_preserves_addressed_outcome_and_matches_pf() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "PF1",
                    "origin_kind": "sashiko_review",
                    "summary": "renesas_sdhi_set_clock() can enter an infinite while (best_freq < new_clock) loop",
                    "verdict": "addressed_in_vN",
                    "reasoning": "In v2, the while loop was replaced with a bounded for (i = 0; i < 1024; i++) loop.",
                    "version": 1,
                    "thread_participants": ["Sashiko"],
                }),
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM3",
                    "origin_kind": "human_reviewer",
                    "summary": "Dan Carpenter noted that the while (best_freq < new_clock) loop in renesas_sdhi_set_clock() could loop infinitely",
                    "verdict": "addressed_in_vN",
                    "reasoning": "In v2, the while loop was replaced with a bounded for (i = 0; i < 1024; i++) loop.",
                    "version": 1,
                    "thread_participants": ["Dan Carpenter <dan.carpenter@linaro.org>"],
                }),
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM4",
                    "origin_kind": "human_reviewer",
                    "summary": "Missing clk_disable_unprepare() in renesas_sdhi_probe() error path",
                    "verdict": "addressed_in_vN",
                    "reasoning": "In v2, goto err_clk was added before returning ret.",
                    "version": 1,
                    "thread_participants": ["Geert Uytterhoeven <geert+renesas@glider.be>"],
                }),
            ],
            // Even if a stage emitted a dismissed_concern referencing HM3,
            // verdict == "addressed_in_vN" must still map to v_n_outcome == "addressed".
            all_dismissed_concerns: vec![json!({
                "id": "PD1",
                "prior_source_ids": ["HM3"],
                "stage": "prior-feedback",
                "concern": "infinite loop in renesas_sdhi_set_clock()",
                "dismissal_reasoning": "Fixed in v2."
            })],
            verification_findings: vec![json!({
                "id": "VF1",
                "stages": ["resources", "prior-feedback"],
                "raw_source_ids": ["C1", "PC4"],
                "prior_source_ids": ["HM4"],
                "prior_attribution_cleared": true,
                "problem": "unrelated leak",
                "severity": "Medium",
                "preexisting": false
            })],
            ..Default::default()
        };

        enrich_prior_resolutions(&mut state);
        assert_eq!(state.prior_resolutions[0]["also_found_by_sashiko"], true);
        assert_eq!(state.prior_resolutions[0]["v_n_outcome"], "addressed");
        assert_eq!(
            state.prior_resolutions[1]["also_found_by_sashiko"], true,
            "HM3 shares the same defect as PF1 so also_found_by_sashiko must be true"
        );
        assert_eq!(
            state.prior_resolutions[1]["v_n_outcome"], "addressed",
            "addressed_in_vN must not be overwritten by dismissed_in_verification"
        );
        assert_eq!(
            state.prior_resolutions[2]["also_found_by_sashiko"], false,
            "HM4 is an unrelated bug so also_found_by_sashiko must remain false"
        );
        assert_eq!(state.prior_resolutions[2]["v_n_outcome"], "addressed");
    }

    #[test]
    fn test_enrich_prior_resolutions_dedups_sashiko_hm_and_matches_fix_phrased_hm() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "PF1",
                    "origin_kind": "sashiko_review",
                    "summary": "Data race when updating live max_active limits with plain stores in workqueue_init_topology",
                    "verdict": "addressed_in_vN",
                    "reasoning": "In v4, wq_init_max_active() is only called during initial allocation before publication, while workqueue_init_topology() and runtime setters update saved and live limits through wq_rescale_max_active() and wq_adjust_max_active() using WRITE_ONCE().",
                    "version": 3,
                    "thread_participants": ["Sashiko"],
                }),
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM1",
                    "origin_kind": "sashiko_review",
                    "summary": "Bot review thread on data race when updating live max_active limits in workqueue_init_topology",
                    "verdict": "addressed_in_vN",
                    "reasoning": "Author replied acknowledging the race; v4 updates workqueue_init_topology() to go through wq_rescale_max_active() and wq_adjust_max_active() using WRITE_ONCE() instead of wq_init_max_active().",
                    "version": 3,
                    "thread_participants": ["Sashiko"],
                }),
                json!({
                    "stage": "prior-feedback",
                    "prior_id": "HM2",
                    "origin_kind": "human_reviewer",
                    "summary": "Avoid calling wq_init_max_active in workqueue_init_topology and share a rescale helper",
                    "verdict": "addressed_in_vN",
                    "reasoning": "In v4, the topology rescale patch was folded into this commit and replaced the wq_init_max_active() call in workqueue_init_topology() with a shared wq_rescale_max_active() helper publishing updated limits via wq_adjust_max_active().",
                    "version": 3,
                    "thread_participants": ["Tejun Heo <tj@kernel.org>"],
                }),
            ],
            ..Default::default()
        };

        enrich_prior_resolutions(&mut state);
        assert_eq!(
            state.prior_resolutions.len(),
            2,
            "Redundant HM1 (sashiko_review) duplicating PF1 on v3 must be dropped"
        );
        assert_eq!(state.prior_resolutions[0]["prior_id"], "PF1");
        assert_eq!(state.prior_resolutions[1]["prior_id"], "HM2");
        assert_eq!(
            state.prior_resolutions[1]["also_found_by_sashiko"], true,
            "Fix-phrased HM2 sharing 1 summary token and 4 C identifiers in reasoning must match PF1"
        );
    }

    #[test]
    fn test_enrich_prior_resolutions_marks_cleared_attribution_as_dismissed_in_verification() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![json!({
                "stage": "prior-feedback",
                "prior_id": "HM1",
                "origin_kind": "human_reviewer",
                "summary": "Malformed vendor-prefixes.yaml and commit log",
                "verdict": "still_unaddressed",
                "reasoning": "v7 introduces a new YAML key placement error.",
                "version": 6,
                "thread_participants": ["Krzysztof Kozlowski <krzk@kernel.org>"],
            })],
            verification_findings: vec![json!({
                "id": "VF1",
                "stages": ["implementation", "prior-feedback"],
                "raw_source_ids": ["C1", "PC1"],
                "prior_source_ids": ["HM1"],
                "prior_attribution_cleared": true,
                "problem": "dt-bindings: vendor-prefixes: invalid YAML placement",
                "severity": "High",
                "preexisting": false
            })],
            ..Default::default()
        };

        enrich_prior_resolutions(&mut state);
        assert_eq!(
            state.prior_resolutions[0]["v_n_outcome"], "dismissed_in_verification",
            "When prior-attribution clears attribution on a still_unaddressed item, v_n_outcome must be dismissed_in_verification rather than unverified"
        );
    }

    #[test]
    fn test_enrich_prior_resolutions_contested_pc_with_fresh_dismissal_not_also_found_by_sashiko() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            prior_resolutions: vec![json!({
                "stage": "prior-feedback",
                "prior_id": "HM1",
                "origin_kind": "human_reviewer",
                "summary": "Missing lock around list traversal",
                "verdict": "still_unaddressed",
                "reasoning": "Lock is still omitted on v2.",
                "version": 1,
                "thread_participants": ["Jan Kara <jack@suse.cz>"],
            })],
            inconclusive_concerns: vec![json!({
                "id": "I1",
                "stages": ["prior-feedback", "locking"],
                "source_ids": ["G1"],
                "raw_source_ids": ["PC1", "D1"],
                "prior_source_ids": ["HM1"],
            })],
            post_verification_findings: vec![json!({
                "id": "PVF1",
                "stages": ["prior-feedback", "locking"],
                "source_ids": ["I1"],
                "raw_source_ids": ["PC1", "D1", "G1"],
                "prior_source_ids": ["HM1"],
                "prior_attribution": {
                    "originally_raised_by": "Jan Kara",
                    "originally_raised_in_version": 1,
                    "origin_kind": "human_reviewer",
                    "found_by": "prior_feedback"
                },
                "problem": "Missing lock around list traversal",
                "severity": "High",
                "preexisting": false
            })],
            ..Default::default()
        };

        enrich_prior_resolutions(&mut state);
        assert_eq!(
            state.prior_resolutions[0]["v_n_outcome"],
            "confirmed_finding"
        );
        assert_eq!(
            state.prior_resolutions[0]["also_found_by_sashiko"], false,
            "A PC1 concern contested only by a fresh stage's D1 dismissal was not found by a fresh Sashiko stage"
        );
    }

    #[test]
    fn test_build_prompts_manifest_extracts_maintainer_and_code_notes() {
        let state = LinuxPatchReviewState {
            project: "linux".to_string(),
            maintainer_notes: "\n\n<maintainer_notes>\n<maintainer_note subsystem=\"General\">\n- rule\n</maintainer_note>\n<maintainer_note subsystem=\"Networking Drivers\">\n- net rule\n</maintainer_note>\n</maintainer_notes>".to_string(),
            code_notes: "\n\n<code_notes set=\"7.3\" revision=\"v6.18\">\n<unit_note unit=\"NETWORKING DRIVERS — drivers/net (part 3)\" reason=\"touched_files\">\n- note\n</unit_note>\n</code_notes>".to_string(),
            ..Default::default()
        };
        let map = build_review_map(ProjectId::Linux, &state, &[], 0);
        assert_eq!(
            map["prompts"]["maintainer_notes"],
            json!(["General", "Networking Drivers"])
        );
        assert_eq!(map["prompts"]["code_notes_set"], json!("7.3"));
        assert_eq!(
            map["prompts"]["code_notes"],
            json!(["NETWORKING DRIVERS — drivers/net (part 3)"])
        );
    }
}

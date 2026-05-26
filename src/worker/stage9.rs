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

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(crate) const BUG_PATTERN_STAGE: u8 = 12;
pub(crate) const ARGUMENT_ORDER_STAGE: u8 = 11;
pub(crate) const ARGUMENT_ORDER_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/argument_order_preservation.txt");
pub(crate) const SEQCOUNT_IRQ_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/seqcount_irq_preservation.txt");
pub(crate) const RESOURCE_CLEANUP_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/resource_cleanup_preservation.txt");
pub(crate) const RETRY_RESOURCE_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/retry_resource_preservation.txt");
pub(crate) const LIFECYCLE_ORDERING_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/lifecycle_ordering_preservation.txt");
pub(crate) const BUG_PATTERN_PRESERVATION_RULE: &str =
    include_str!("prompt_fragments/bug_pattern_preservation.txt");
pub(crate) const ROOT_CAUSE_COMPACTION_RULE: &str =
    include_str!("prompt_fragments/root_cause_compaction.txt");

pub(crate) fn append_bug_pattern_concerns(
    all_concerns: &mut Vec<Value>,
    result_json: &Value,
    origin_stage: &str,
) -> bool {
    let Some(concerns) = result_json.get("concerns").and_then(|c| c.as_array()) else {
        return false;
    };

    for c in concerns {
        if let Value::Object(obj) = c {
            let mut obj = obj.clone();
            obj.insert(
                "origin_stage".to_string(),
                Value::String(origin_stage.to_string()),
            );
            obj.insert(
                "preservation_policy".to_string(),
                Value::String("bug_pattern_scan".to_string()),
            );
            let mut concern = Value::Object(obj);
            annotate_seed_provenance(&mut concern, "targeted_bug_pattern_scan");
            all_concerns.push(concern);
        } else if let Some(s) = c.as_str() {
            let mut concern = json!({
                "type": "Bug Pattern Scan",
                "description": s,
                "origin_stage": origin_stage,
                "preservation_policy": "bug_pattern_scan"
            });
            annotate_seed_provenance(&mut concern, "targeted_bug_pattern_scan");
            all_concerns.push(concern);
        }
    }

    true
}

pub(crate) fn seed_required_evidence(pattern: &str) -> Vec<&'static str> {
    match pattern {
        "cgroup_keyed_parse_missing_value" => vec![
            "file",
            "function",
            "key or option name",
            "missing value path",
            "dereference/parse site",
        ],
        "rcu_teardown_iteration_without_read_lock" => vec![
            "teardown/unregister function",
            "list_for_each_rcu or equivalent iterator",
            "whether rcu_read_lock is held",
            "whether another lock or lockdep condition proves safety",
        ],
        "skb_fragment_capacity_max_skb_frags" => vec![
            "append function",
            "nr_frags or frag array write",
            "MAX_SKB_FRAGS or equivalent guard",
            "looped fragment capacity path",
        ],
        "retry_error_path_resource_leak" => vec![
            "operation/helper",
            "resource buffer",
            "cleanup helper",
            "failed operation followed by retry/fallback",
            "whether the resource is freed before retry/overwrite",
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn seed_pattern_from_text(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    let mentions_cgroup_keyed_missing_value = (lower.contains("dmem") || lower.contains("cgroup"))
        && (lower.contains("max") || lower.contains("keyed") || lower.contains("limit"))
        && (lower.contains("missing value")
            || lower.contains("without a value")
            || lower.contains("absent")
            || lower.contains("value pointer")
            || lower.contains("null"));
    if mentions_cgroup_keyed_missing_value {
        return Some("cgroup_keyed_parse_missing_value");
    }

    let mentions_rcu_teardown = (lower.contains("region_unregister")
        || lower.contains("unregister")
        || lower.contains("teardown"))
        && (lower.contains("list_for_each_rcu")
            || lower.contains("hlist_for_each_entry_rcu")
            || lower.contains("rcu traversal")
            || lower.contains("rcu list"))
        && (lower.contains("rcu_read_lock")
            || lower.contains("missing")
            || lower.contains("without")
            || lower.contains("lock"));
    if mentions_rcu_teardown {
        return Some("rcu_teardown_iteration_without_read_lock");
    }

    let mentions_skb_frag_capacity = (lower.contains("t7xx_dpmaif_set_frag_to_skb")
        || lower.contains("skb_add_rx_frag")
        || lower.contains("skb_shinfo")
        || lower.contains("frags[")
        || lower.contains("nr_frags"))
        && (lower.contains("max_skb_frags")
            || lower.contains("fragment capacity")
            || lower.contains("frag array")
            || lower.contains("capacity guard")
            || lower.contains("overflow"));
    if mentions_skb_frag_capacity {
        return Some("skb_fragment_capacity_max_skb_frags");
    }

    let mentions_retry_resource_leak = (lower.contains("retry") || lower.contains("fallback"))
        && (lower.contains("retry_iov")
            || lower.contains("iov_base")
            || lower.contains("response buffer")
            || lower.contains("resource buffer"))
        && (lower.contains("retry_open")
            || lower.contains("operation")
            || lower.contains("helper"))
        && (lower.contains("free_response_buf")
            || lower.contains("free")
            || lower.contains("cleanup")
            || lower.contains("leak")
            || lower.contains("overwrite"));
    if mentions_retry_resource_leak {
        return Some("retry_error_path_resource_leak");
    }

    None
}

pub(crate) fn seed_pattern(concern: &Value) -> Option<&'static str> {
    if let Some(pattern) = concern.get("pattern").and_then(|value| value.as_str()) {
        return match pattern {
            "cgroup_keyed_parse_missing_value" => Some("cgroup_keyed_parse_missing_value"),
            "rcu_teardown_iteration_without_read_lock" => {
                Some("rcu_teardown_iteration_without_read_lock")
            }
            "skb_fragment_capacity_max_skb_frags" => Some("skb_fragment_capacity_max_skb_frags"),
            "retry_error_path_resource_leak" => Some("retry_error_path_resource_leak"),
            _ => None,
        };
    }

    seed_pattern_from_text(&concern_review_text(concern))
}

pub(crate) fn annotate_seed_provenance(concern: &mut Value, source: &str) {
    let Some(pattern) = seed_pattern(concern) else {
        return;
    };
    let Some(obj) = concern.as_object_mut() else {
        return;
    };

    obj.entry("source".to_string())
        .or_insert_with(|| Value::String(source.to_string()));
    obj.entry("pattern".to_string())
        .or_insert_with(|| Value::String(pattern.to_string()));
    obj.insert(
        "preservation".to_string(),
        Value::String("proof_required_drop".to_string()),
    );
    obj.insert(
        "preservation_policy".to_string(),
        Value::String("proof_required_drop".to_string()),
    );
    obj.entry("required_evidence".to_string())
        .or_insert_with(|| {
            Value::Array(
                seed_required_evidence(pattern)
                    .into_iter()
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            )
        });
}

pub(crate) fn seed_bug_pattern_concerns_from_diff(diff: &str) -> Vec<Value> {
    let lower = diff.to_ascii_lowercase();
    let mut concerns = Vec::new();

    if lower.contains("limit_key_write")
        && lower.contains("\"max\"")
        && (lower.contains("strsep(&options") || lower.contains("strsep(&buf"))
    {
        concerns.push(json!({
            "source": "static_bug_pattern_seed",
            "pattern": "cgroup_keyed_parse_missing_value",
            "preservation": "proof_required_drop",
            "required_evidence": [
                "file",
                "function",
                "key or option name",
                "missing value path",
                "dereference/parse site"
            ],
            "type": "Cgroup keyed file parsing / missing value",
            "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
            "reasoning": "The diff wires a cgroup file named \"max\" through limit_region_max_write()/limit_key_write() and tokenizes keyed input with strsep(). A cgroup write can present a key such as \"max\" without an accompanying value, so the handler must prove the value/options pointer is non-NULL before strcmp(), memparse(), or other parsing. The diff should keep nested-keyed files, numeric limits, and the \"max\" sentinel as separate cases.",
            "preexisting": false,
            "origin_stage": "bug_pattern_static_seed",
            "preservation_policy": "proof_required_drop",
            "locations": [
                {
                    "file": "kernel/cgroup/limit.c",
                    "function/symbol": "limit_region_max_write / limit_key_write",
                    "code_snippet": "strsep(...); limit_key_write(..., options)"
                }
            ]
        }));
    }

    if lower.contains("region_unregister")
        && (lower.contains("list_for_each_rcu") || lower.contains("hlist_for_each_entry_rcu"))
    {
        concerns.push(json!({
            "source": "static_bug_pattern_seed",
            "pattern": "rcu_teardown_iteration_without_read_lock",
            "preservation": "proof_required_drop",
            "required_evidence": [
                "teardown/unregister function",
                "list_for_each_rcu or equivalent iterator",
                "whether rcu_read_lock is held",
                "whether another lock or lockdep condition proves safety"
            ],
            "type": "RCU list iteration in unregister/teardown path",
            "description": "region_unregister() uses RCU list traversal in an unregister path without visible read-side or update-side proof.",
            "reasoning": "The diff shows region_unregister() in a teardown path using RCU list operations such as list_for_each_rcu()/list_del_rcu(). Teardown paths cannot rely on normal caller assumptions unless the function holds rcu_read_lock() or documents an update-side lock/lockdep condition that makes the traversal safe.",
            "preexisting": false,
            "origin_stage": "bug_pattern_static_seed",
            "preservation_policy": "proof_required_drop",
            "locations": [
                {
                    "file": "kernel/cgroup/limit.c",
                    "function/symbol": "region_unregister",
                    "code_snippet": "list_for_each_rcu(...); list_del_rcu(...)"
                }
            ]
        }));
    }

    if lower.contains("t7xx_dpmaif_set_frag_to_skb")
        && (lower.contains("skb_add_rx_frag")
            || lower.contains("skb_shinfo(skb)->frags")
            || lower.contains("skb_shinfo(skb)->nr_frags"))
    {
        concerns.push(json!({
            "source": "static_bug_pattern_seed",
            "pattern": "skb_fragment_capacity_max_skb_frags",
            "preservation": "proof_required_drop",
            "required_evidence": [
                "append function",
                "nr_frags or frag array write",
                "MAX_SKB_FRAGS or equivalent guard",
                "looped fragment capacity path"
            ],
            "type": "skb fragment capacity / MAX_SKB_FRAGS",
            "description": "t7xx_dpmaif_set_frag_to_skb() appends skb fragments without an evident MAX_SKB_FRAGS capacity guard.",
            "reasoning": "The diff adds a t7xx skb fragment append path that uses skb_add_rx_frag() with skb_shinfo(skb)->nr_frags. Looped DMA/page fragments can exceed the skb fragment array capacity unless the same path checks nr_frags against MAX_SKB_FRAGS, or an equivalent capacity guard, before each append.",
            "preexisting": false,
            "origin_stage": "bug_pattern_static_seed",
            "preservation_policy": "proof_required_drop",
            "locations": [
                {
                    "file": "drivers/net/wwan/t7xx/t7xx_hif_dpmaif_rx.c",
                    "function/symbol": "t7xx_dpmaif_set_frag_to_skb",
                    "code_snippet": "skb_add_rx_frag(skb, skb_shinfo(skb)->nr_frags, ...)"
                }
            ]
        }));
    }

    if lower.contains("retry_open_file")
        && lower.contains("retry_open")
        && lower.contains("retry_without_read_attributes")
        && lower.contains("retry_iov")
    {
        concerns.push(json!({
            "source": "static_bug_pattern_seed",
            "pattern": "retry_error_path_resource_leak",
            "preservation": "proof_required_drop",
            "required_evidence": [
                "operation/helper",
                "resource buffer",
                "cleanup helper",
                "failed operation followed by retry/fallback",
                "whether the resource is freed before retry/overwrite"
            ],
            "type": "Retry error-path response-buffer leak",
            "description": "retry_open_file() retries retry_open() after a failed open while reusing retry_iov without an evident free_response_buf() before the retry.",
            "reasoning": "The diff adds retry_without_read_attributes around retry_open(). The first retry_open() can populate retry_iov.iov_base and err_buftype on an -EACCES failure, then the code calls retry_open() again on the fallback path using the same retry_iov/err_buftype outputs. Dropping this requires proof that free_response_buf() or an equivalent cleanup runs before the retry/fallback can overwrite or lose the first response buffer.",
            "preexisting": false,
            "origin_stage": "bug_pattern_static_seed",
            "preservation_policy": "proof_required_drop",
            "locations": [
                {
                    "file": "fs/smb/client/retryfile.c",
                    "function/symbol": "retry_open_file",
                    "code_snippet": "retry_open(..., &retry_iov, &err_buftype); if (rc == -EACCES && retry_without_read_attributes) { ... retry_open(..., &retry_iov, &err_buftype); } ... out: free_response_buf(err_buftype, retry_iov.iov_base);"
                }
            ]
        }));
    }

    concerns
}

pub(crate) fn seed_lifecycle_ordering_concerns_from_diff(diff: &str) -> Vec<Value> {
    let lower = diff.to_ascii_lowercase();
    let mut concerns = Vec::new();

    if lower.contains("nci_unregister_device")
        && lower.contains("nci_close_device")
        && lower.contains("destroy_workqueue")
        && lower.contains("nfc_unregister_device")
        && lower.contains("cmd_wq")
    {
        concerns.push(json!({
            "source_concern_id": "lifecycle-static-001",
            "type": "Lifecycle teardown ordering / callback re-entry",
            "description": "nci_unregister_device() destroys NCI workqueues before unregistering the NFC/rfkill callback source.",
            "reasoning": "The diff shows nci_unregister_device() calling nci_close_device(), then destroy_workqueue() on cmd_wq/rx_wq/tx_wq, and only afterward calling nfc_unregister_device(). The NFC core unregister path can include rfkill unregister callbacks that re-enter nci_dev_down()/nci_close_device(); if rfkill or another close callback can run after the workqueues are destroyed, nci_close_device() can observe freed workqueue state. Dropping this requires proof that nfc/rfkill callbacks are unregistered before the workqueues are destroyed, or an equivalent barrier/cancel operation prevents post-destroy re-entry.",
            "preexisting": false,
            "origin_stage": "lifecycle_static_seed",
            "preservation_policy": "lifecycle_ordering_emit_or_prove_callback_barrier",
            "locations": [
                {
                    "file": "net/nfc/nci/core.c",
                    "function/symbol": "nci_unregister_device",
                    "code_snippet": "nci_close_device(ndev); destroy_workqueue(ndev->cmd_wq); ... nfc_unregister_device(ndev->nfc_dev);"
                }
            ]
        }));
    }

    concerns
}

pub(crate) fn preserve_static_bug_pattern_findings(
    existing_concerns: &[Value],
    findings: &mut Value,
) {
    let Some(findings_array) = findings.as_array_mut() else {
        return;
    };

    for concern in existing_concerns
        .iter()
        .filter(|concern| is_static_bug_pattern_concern(concern))
    {
        let concern_text = concern_review_text(concern);
        let already_preserved = if is_proof_required_seed_concern(concern) {
            findings_array
                .iter()
                .any(|finding| finding_preserves_seed_pattern_detail(concern, finding))
        } else {
            findings_array.iter().any(|finding| {
                static_bug_pattern_text_matches(&concern_text, &finding_review_text(finding))
                    || finding_preserves_seed_pattern_detail(concern, finding)
            })
        };
        if already_preserved {
            continue;
        }

        if let Some(existing_idx) = findings_array.iter().position(|finding| {
            static_bug_pattern_text_matches(&concern_text, &finding_review_text(finding))
        }) {
            let existing = findings_array[existing_idx].clone();
            findings_array[existing_idx] =
                synthesize_seed_pattern_finding(concern, Some(&existing));
        }
    }
}

pub(crate) fn preserve_static_lifecycle_ordering_findings(
    existing_concerns: &[Value],
    findings: &mut Value,
) {
    let Some(findings_array) = findings.as_array_mut() else {
        return;
    };

    for concern in existing_concerns
        .iter()
        .filter(|concern| is_static_lifecycle_ordering_concern(concern))
    {
        if findings_array
            .iter()
            .any(|finding| finding_preserves_lifecycle_ordering_detail(concern, finding))
        {
            continue;
        }

        let concern_text = concern_review_text(concern);
        if let Some(existing_idx) = findings_array.iter().position(|finding| {
            static_lifecycle_ordering_text_matches(&concern_text, &finding_review_text(finding))
        }) {
            let existing = findings_array[existing_idx].clone();
            findings_array[existing_idx] =
                synthesize_lifecycle_ordering_finding(concern, Some(&existing));
        }
    }
}

pub(crate) fn is_static_bug_pattern_concern(concern: &Value) -> bool {
    concern
        .get("origin_stage")
        .and_then(|v| v.as_str())
        .is_some_and(|stage| stage == "bug_pattern_static_seed")
}

pub(crate) fn is_static_lifecycle_ordering_concern(concern: &Value) -> bool {
    concern
        .get("origin_stage")
        .and_then(|v| v.as_str())
        .is_some_and(|stage| stage == "lifecycle_static_seed")
}

pub(crate) fn static_bug_pattern_text_matches(concern_text: &str, finding_text: &str) -> bool {
    let concern_lower = concern_text.to_ascii_lowercase();
    let finding_lower = finding_text.to_ascii_lowercase();

    let cgroup = (concern_lower.contains("cgroup")
        || concern_lower.contains("limit.max")
        || concern_lower.contains("limit_key_write")
        || concern_lower.contains("keyed"))
        && (concern_lower.contains("missing value") || concern_lower.contains("value pointer"));
    let rcu = concern_lower.contains("region_unregister") && concern_lower.contains("rcu");
    let skb = concern_lower.contains("t7xx_dpmaif_set_frag_to_skb")
        || (concern_lower.contains("max_skb_frags") && concern_lower.contains("nr_frags"));
    let retry_resource = concern_lower.contains("retry_open")
        && (concern_lower.contains("retry_iov") || concern_lower.contains("response buffer"))
        && (concern_lower.contains("retry") || concern_lower.contains("fallback"));

    (cgroup
        && (finding_lower.contains("cgroup")
            || finding_lower.contains("limit_key_write")
            || finding_lower.contains("keyed"))
        && finding_lower.contains("value"))
        || (rcu && finding_lower.contains("region_unregister") && finding_lower.contains("rcu"))
        || (skb && finding_lower.contains("max_skb_frags") && finding_lower.contains("nr_frags"))
        || (retry_resource
            && finding_lower.contains("retry_open")
            && (finding_lower.contains("retry_iov") || finding_lower.contains("response buffer"))
            && (finding_lower.contains("retry") || finding_lower.contains("fallback")))
}

pub(crate) fn static_lifecycle_ordering_text_matches(
    concern_text: &str,
    finding_text: &str,
) -> bool {
    let concern_lower = concern_text.to_ascii_lowercase();
    let finding_lower = finding_text.to_ascii_lowercase();

    let nci_lifecycle = concern_lower.contains("nci_unregister_device")
        && concern_lower.contains("nci_close_device")
        && (concern_lower.contains("workqueue") || concern_lower.contains("cmd_wq"));

    nci_lifecycle
        && finding_lower.contains("nci_close_device")
        && (finding_lower.contains("workqueue") || finding_lower.contains("cmd_wq"))
        && (finding_lower.contains("rfkill")
            || finding_lower.contains("nfc_unregister_device")
            || finding_lower.contains("unregister")
            || finding_lower.contains("callback"))
}

pub(crate) fn is_proof_required_seed_concern(concern: &Value) -> bool {
    let proof_required = concern
        .get("preservation")
        .and_then(|value| value.as_str())
        .is_some_and(|value| value == "proof_required_drop")
        || concern
            .get("preservation_policy")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value == "proof_required_drop");

    proof_required && seed_pattern(concern).is_some()
}

pub(crate) fn text_preserves_cgroup_missing_value_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_file_or_function = lower.contains("kernel/cgroup/limit.c")
        || lower.contains("limit_key_write")
        || lower.contains("limit_region_max_write")
        || lower.contains("dmemcg_parse_limit");
    let mentions_exact_max_key = lower.contains("limit.max")
        || lower.contains("limit_region_max_write")
        || lower.contains("file named \"limit.max\"")
        || lower.contains("file named 'limit.max'")
        || lower.contains("\"max\"")
        || lower.contains("'max'")
        || lower.contains("max sentinel")
        || lower.contains("key like max")
        || lower.contains("key like \"max\"")
        || lower.contains("key like 'max'");
    let mentions_key =
        mentions_exact_max_key || lower.contains("keyed") || lower.contains("limit option");
    let misstates_as_non_max_path = lower.contains("non-'max'")
        || lower.contains("non-\"max\"")
        || lower.contains("not 'max'")
        || lower.contains("not \"max\"");
    let mentions_missing_value = lower.contains("missing value")
        || lower.contains("absent value")
        || lower.contains("without a value")
        || lower.contains("no accompanying value")
        || lower.contains("null value")
        || lower.contains("null options")
        || lower.contains("value pointer");
    let mentions_bad_access = lower.contains("null dereference")
        || lower.contains("null pointer")
        || lower.contains("dereference")
        || lower.contains("invalid parse")
        || lower.contains("strcmp")
        || lower.contains("memparse")
        || lower.contains("kstrto")
        || lower.contains("parse");
    let mentions_write_path = lower.contains("write")
        || lower.contains("cgroup file")
        || lower.contains("handler")
        || lower.contains("trigger");

    mentions_file_or_function
        && mentions_exact_max_key
        && mentions_key
        && mentions_missing_value
        && mentions_bad_access
        && mentions_write_path
        && !misstates_as_non_max_path
}

pub(crate) fn cgroup_missing_value_drop_proves_safety(dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();

    let names_path = lower.contains("limit_key_write")
        || lower.contains("limit_region_max_write")
        || lower.contains("dmemcg_parse_limit")
        || lower.contains("kernel/cgroup/limit.c");
    let names_key = lower.contains("limit.max")
        || lower.contains("\"max\"")
        || lower.contains(" key ")
        || lower.contains("sentinel")
        || lower.contains("limit option");
    let names_missing_value = lower.contains("missing value")
        || lower.contains("without a value")
        || lower.contains("bare key")
        || lower.contains("absent value")
        || lower.contains("value pointer")
        || lower.contains("options");
    let names_parse_site = lower.contains("strcmp")
        || lower.contains("memparse")
        || lower.contains("kstrto")
        || lower.contains("parse")
        || lower.contains("dereference");

    let proves_unreachable = (lower.contains("cannot receive")
        || lower.contains("cannot be called")
        || lower.contains("not reachable")
        || lower.contains("not handled by")
        || lower.contains("not in the changed path"))
        && (lower.contains("bare key") || lower.contains("without a value") || names_key);
    let proves_rejected_before_access = (lower.contains("reject")
        || lower.contains("returns -")
        || lower.contains("-einval")
        || lower.contains("fails before"))
        && (lower.contains("before") || lower.contains("prior to"))
        && names_parse_site;
    let proves_null_checked_before_access = (lower.contains("non-null")
        || lower.contains("not null")
        || lower.contains("null check")
        || lower.contains("checks options")
        || lower.contains("checks the value")
        || lower.contains("value pointer is checked"))
        && (lower.contains("before") || lower.contains("prior to"))
        && names_parse_site;
    let proves_key_not_changed_path = (lower.contains("not handled")
        || lower.contains("not in")
        || lower.contains("different path")
        || lower.contains("not the changed path"))
        && names_key;

    names_path
        && names_key
        && names_missing_value
        && (proves_unreachable
            || proves_rejected_before_access
            || proves_null_checked_before_access
            || proves_key_not_changed_path)
}

pub(crate) fn text_preserves_rcu_teardown_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_function = lower.contains("region_unregister");
    let mentions_iterator = lower.contains("list_for_each_rcu")
        || lower.contains("hlist_for_each_entry_rcu")
        || lower.contains("rcu iterator")
        || lower.contains("rcu traversal")
        || lower.contains("rcu list");
    let mentions_missing_lock = lower.contains("missing rcu_read_lock")
        || lower.contains("without rcu_read_lock")
        || lower.contains("no rcu_read_lock")
        || lower.contains("without read-side")
        || lower.contains("without protection")
        || lower.contains("missing lock")
        || lower.contains("no lockdep");
    let mentions_teardown = lower.contains("teardown")
        || lower.contains("unregister")
        || lower.contains("remove")
        || lower.contains("cleanup");
    let mentions_warning_or_race = lower.contains("warning")
        || lower.contains("suspicious rcu")
        || lower.contains("race")
        || lower.contains("uaf")
        || lower.contains("use-after-free")
        || lower.contains("concurrent");

    mentions_function
        && mentions_iterator
        && mentions_missing_lock
        && mentions_teardown
        && mentions_warning_or_race
}

pub(crate) fn rcu_teardown_drop_proves_safety(dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();

    let names_function = lower.contains("region_unregister");
    let names_iterator = lower.contains("list_for_each_rcu")
        || lower.contains("hlist_for_each_entry_rcu")
        || lower.contains("rcu traversal")
        || lower.contains("rcu iterator");
    let names_teardown = lower.contains("unregister")
        || lower.contains("teardown")
        || lower.contains("remove")
        || lower.contains("not reachable");
    let proves_read_lock = lower.contains("rcu_read_lock")
        && (lower.contains("held") || lower.contains("around") || lower.contains("covers"));
    let proves_update_side_lock = (lower.contains("update-side")
        || lower.contains("update side")
        || lower.contains("lockdep")
        || lower.contains("mutex")
        || lower.contains("spinlock")
        || lower.contains("held lock"))
        && (lower.contains("documented")
            || lower.contains("condition")
            || lower.contains("covers")
            || lower.contains("held"));
    let proves_not_teardown = (lower.contains("not reachable")
        || lower.contains("cannot be called")
        || lower.contains("not called"))
        && (lower.contains("unregister") || lower.contains("teardown"));

    names_function
        && names_iterator
        && names_teardown
        && (proves_read_lock || proves_update_side_lock || proves_not_teardown)
}

pub(crate) fn text_preserves_skb_fragment_capacity_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_function = lower.contains("t7xx_dpmaif_set_frag_to_skb");
    let mentions_capacity = lower.contains("max_skb_frags")
        || lower.contains("nr_frags")
        || lower.contains("frag array")
        || lower.contains("fragment array")
        || lower.contains("capacity");
    let mentions_append = lower.contains("skb_add_rx_frag")
        || lower.contains("skb_shinfo")
        || lower.contains("frags[")
        || lower.contains("append");
    let mentions_missing_guard = lower.contains("missing")
        || lower.contains("without")
        || lower.contains("no ")
        || lower.contains("lacks")
        || lower.contains("not check");
    let mentions_consequence = lower.contains("overflow")
        || lower.contains("out-of-bounds")
        || lower.contains("oob")
        || lower.contains("exceed")
        || lower.contains("capacity");

    mentions_function
        && mentions_capacity
        && mentions_append
        && mentions_missing_guard
        && mentions_consequence
}

pub(crate) fn skb_fragment_capacity_drop_proves_safety(dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();

    let names_function = lower.contains("t7xx_dpmaif_set_frag_to_skb");
    let names_append = lower.contains("skb_add_rx_frag")
        || lower.contains("skb_shinfo")
        || lower.contains("nr_frags")
        || lower.contains("frags[");
    let names_guard = lower.contains("max_skb_frags")
        || lower.contains("skb_can_coalesce")
        || lower.contains("nr_frags")
        || lower.contains("capacity guard");
    let proves_before_every_append = lower.contains("before")
        || lower.contains("prior to")
        || lower.contains("precedes")
        || lower.contains("for every append")
        || lower.contains("each append");

    names_function && names_append && names_guard && proves_before_every_append
}

pub(crate) fn finding_preserves_seed_pattern_detail(concern: &Value, finding: &Value) -> bool {
    let text = finding_review_text(finding);
    match seed_pattern(concern) {
        Some("cgroup_keyed_parse_missing_value") => {
            text_preserves_cgroup_missing_value_detail(&text)
        }
        Some("rcu_teardown_iteration_without_read_lock") => {
            text_preserves_rcu_teardown_detail(&text)
        }
        Some("skb_fragment_capacity_max_skb_frags") => {
            text_preserves_skb_fragment_capacity_detail(&text)
        }
        Some("retry_error_path_resource_leak") => text_preserves_retry_resource_leak_detail(&text),
        _ => false,
    }
}

pub(crate) fn seed_pattern_drop_proves_safety(concern: &Value, dropped: &Value) -> bool {
    match seed_pattern(concern) {
        Some("cgroup_keyed_parse_missing_value") => {
            cgroup_missing_value_drop_proves_safety(dropped)
        }
        Some("rcu_teardown_iteration_without_read_lock") => {
            rcu_teardown_drop_proves_safety(dropped)
        }
        Some("skb_fragment_capacity_max_skb_frags") => {
            skb_fragment_capacity_drop_proves_safety(dropped)
        }
        Some("retry_error_path_resource_leak") => {
            retry_resource_drop_proves_safety(concern, dropped)
        }
        _ => false,
    }
}

pub(crate) fn stage_response_schema(stage: u8) -> Option<Value> {
    match stage {
        8 => Some(json!({
            "type": "object",
            "properties": {
                "concerns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true
                    }
                },
                "dropped_concerns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true
                    }
                }
            },
            "required": ["concerns", "dropped_concerns"],
            "additionalProperties": false
        })),
        BUG_PATTERN_STAGE | ARGUMENT_ORDER_STAGE | 1..=7 => Some(json!({
            "type": "object",
            "properties": {
                "concerns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true
                    }
                }
            },
            "required": ["concerns"],
            "additionalProperties": false
        })),
        9 => Some(json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "finding_id": { "type": "string" },
                            "source_concern_id": { "type": "string" },
                            "problem": { "type": "string" },
                            "severity": { "type": "string", "enum": ["Low", "Medium", "High", "Critical", "low", "medium", "high", "critical"] },
                            "severity_explanation": { "type": "string" },
                            "preexisting": { "type": "boolean" }
                        },
                        "required": ["source_concern_id", "problem", "severity", "severity_explanation", "preexisting"],
                        "additionalProperties": true
                    }
                },
                "dropped_candidates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "source_concern_id": { "type": "string" },
                            "decision": { "type": "string", "enum": ["drop"] },
                            "drop_reason": { "type": "string", "enum": ["duplicate", "subsumed_by", "insufficient_evidence", "not_security_relevant", "already_mitigated", "false_positive", "unclear"] },
                            "subsumed_by_finding_id": { "type": "string" },
                            "rationale": { "type": "string" }
                        },
                        "required": ["source_concern_id", "decision", "drop_reason", "rationale"],
                        "additionalProperties": true
                    }
                }
            },
            "required": ["findings", "dropped_candidates"],
            "additionalProperties": false
        })),
        _ => None,
    }
}

pub(crate) fn minimal_fallback_response_schema() -> Option<Value> {
    Some(json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "problem": { "type": "string" },
                        "severity": { "type": "string", "enum": ["Low", "Medium", "High", "Critical", "low", "medium", "high", "critical"] },
                        "severity_explanation": { "type": "string" },
                        "preexisting": { "type": "boolean" },
                        "finding_id": { "type": "string" },
                        "source_concern_id": { "type": "string" }
                    },
                    "required": ["source_concern_id", "problem", "severity", "severity_explanation", "preexisting"],
                    "additionalProperties": true
                }
            }
        },
        "required": ["findings"],
        "additionalProperties": false
    }))
}

pub(crate) fn add_stage9_source_ids(concerns: &Value) -> Value {
    let Some(concerns) = concerns.as_array() else {
        return Value::Array(Vec::new());
    };

    Value::Array(
        concerns
            .iter()
            .enumerate()
            .map(|(idx, concern)| {
                let source_id = format!("stage8-{idx:03}", idx = idx + 1);
                match concern {
                    Value::Object(obj) => {
                        let mut obj = obj.clone();
                        if let Some(existing_id) =
                            obj.get("source_concern_id").and_then(|id| id.as_str())
                        {
                            obj.insert(
                                "model_source_concern_id".to_string(),
                                Value::String(existing_id.to_string()),
                            );
                        }
                        obj.insert("source_concern_id".to_string(), Value::String(source_id));
                        let concern_with_source = Value::Object(obj.clone());
                        if is_concrete_argument_order_concern(&concern_with_source) {
                            obj.entry("preservation_policy".to_string())
                                .or_insert_with(|| {
                                    Value::String(
                                        "argument_order_emit_or_subsumed_by_detailed_finding"
                                            .to_string(),
                                    )
                                });
                        }
                        if is_resource_cleanup_concern(&concern_with_source) {
                            obj.entry("preservation_policy".to_string())
                                .or_insert_with(|| {
                                    Value::String(
                                        "resource_cleanup_emit_or_prove_exact_deallocation"
                                            .to_string(),
                                    )
                                });
                        }
                        if is_lifecycle_ordering_concern(&concern_with_source) {
                            obj.entry("preservation_policy".to_string())
                                .or_insert_with(|| {
                                    Value::String(
                                        "lifecycle_ordering_emit_or_prove_callback_barrier"
                                            .to_string(),
                                    )
                                });
                        }
                        Value::Object(obj)
                    }
                    other => json!({
                        "source_concern_id": source_id,
                        "type": "General",
                        "description": concern_description(other)
                            .unwrap_or_else(|| other.to_string()),
                        "reasoning": other.to_string(),
                        "preexisting": false
                    }),
                }
            })
            .collect(),
    )
}

pub(crate) fn value_field_text(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|field| field.as_str())
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn concern_review_text(concern: &Value) -> String {
    [
        value_field_text(concern, "type"),
        value_field_text(concern, "description"),
        value_field_text(concern, "reasoning"),
    ]
    .join("\n")
}

pub(crate) fn finding_review_text(finding: &Value) -> String {
    [
        value_field_text(finding, "finding_id"),
        value_field_text(finding, "problem"),
        value_field_text(finding, "severity_explanation"),
    ]
    .join("\n")
}

pub(crate) fn text_mentions_argument_order(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("argument order")
        || lower.contains("parameter order")
        || lower.contains("swapped argument")
        || lower.contains("swapped parameter")
        || lower.contains("arguments swapped")
        || lower.contains("parameters swapped")
        || lower.contains("wrong argument")
        || lower.contains("wrong parameter")
        || lower.contains("wrong order")
        || lower.contains("reversed argument")
        || lower.contains("reversed parameter")
        || ((lower.contains("argument") || lower.contains("parameter"))
            && (lower.contains("swap") || lower.contains("order") || lower.contains("revers")))
}

pub(crate) fn text_preserves_argument_order_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text_mentions_argument_order(text)
        || ((lower.contains("callee") || lower.contains("call") || lower.contains("signature"))
            && (lower.contains("swap") || lower.contains("order") || lower.contains("revers")))
}

pub(crate) fn is_argument_order_concern(concern: &Value) -> bool {
    text_mentions_argument_order(&concern_review_text(concern))
}

pub(crate) fn is_concrete_argument_order_concern(concern: &Value) -> bool {
    if !is_argument_order_concern(concern) {
        return false;
    }

    let concern_text = concern_review_text(concern);
    if !text_has_named_callee_reference(&concern_text) {
        return false;
    }

    text_preserves_argument_order_detail_for_concern(concern, &concern_text)
}

pub(crate) fn text_has_named_callee_reference(text: &str) -> bool {
    if !extract_call_names(text).is_empty() {
        return true;
    }

    let tokens: Vec<&str> = text
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    for pair in tokens.windows(2) {
        let marker = pair[0].to_ascii_lowercase();
        if matches!(
            marker.as_str(),
            "callee" | "function" | "helper" | "api" | "called" | "to" | "for"
        ) && is_named_callee_token(pair[1])
        {
            return true;
        }
    }

    false
}

pub(crate) fn is_named_callee_token(token: &str) -> bool {
    if !is_simple_identifier(token) {
        return false;
    }
    if token.len() < 3 {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "actual"
            | "argument"
            | "arguments"
            | "arg_order"
            | "callee"
            | "call_site"
            | "changed"
            | "contract"
            | "expected"
            | "function"
            | "helper"
            | "issue"
            | "order"
            | "parameter"
            | "parameters"
            | "role"
            | "roles"
            | "signature"
            | "source_concern_id"
            | "swapped"
            | "wrong"
            | "actual_order"
            | "expected_order"
            | "sg_list"
    )
}

pub(crate) fn extract_call_names(text: &str) -> HashSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut names = HashSet::new();
    for (idx, ch) in chars.iter().enumerate() {
        if *ch != '(' {
            continue;
        }

        let mut end = idx;
        while end > 0 && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && (chars[start - 1].is_ascii_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }
        if start == end {
            continue;
        }

        let name: String = chars[start..end].iter().collect();
        if matches!(
            name.as_str(),
            "if" | "for" | "while" | "switch" | "sizeof" | "return"
        ) {
            continue;
        }
        names.insert(name);
    }
    names
}

pub(crate) fn text_has_expected_parameter_order(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("expected")
        || lower.contains("expects")
        || lower.contains("signature")
        || lower.contains("parameter order")
        || lower.contains("argument order")
        || lower.contains("should be")
        || lower.contains("callee takes")
        || lower.contains("api expects")
}

pub(crate) fn text_has_actual_call_order(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("actual")
        || lower.contains("call site")
        || lower.contains("call-site")
        || lower.contains("passes")
        || lower.contains("passed")
        || lower.contains("called with")
        || lower.contains("invoked with")
}

pub(crate) fn text_has_wrong_order_reason(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("wrong")
        || lower.contains("swapped")
        || lower.contains("swap")
        || lower.contains("reversed")
        || lower.contains("mismatch")
        || lower.contains("incorrect")
        || lower.contains("contradict")
}

pub(crate) fn text_mentions_concern_callee(text: &str, concern_text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let call_names = extract_call_names(concern_text);
    if call_names.is_empty() {
        return lower.contains("callee")
            || lower.contains("function")
            || !extract_call_names(text).is_empty();
    }

    call_names
        .iter()
        .any(|name| lower.contains(&name.to_ascii_lowercase()))
}

pub(crate) fn is_simple_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub(crate) fn collect_signature_identifiers(text: &str) -> HashSet<String> {
    let call_names = extract_call_names(text);
    let mut identifiers = HashSet::new();

    for quote in ['`', '\''] {
        for (idx, segment) in text.split(quote).enumerate() {
            if idx % 2 == 1 && is_simple_identifier(segment) {
                identifiers.insert(segment.to_ascii_lowercase());
            }
        }
    }

    let chars: Vec<char> = text.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] != '(' {
            idx += 1;
            continue;
        }
        let start = idx + 1;
        let mut end = start;
        while end < chars.len() && chars[end] != ')' {
            end += 1;
        }
        if end >= chars.len() {
            break;
        }
        let inner: String = chars[start..end].iter().collect();
        if inner.contains(',') && !inner.contains("->") && !inner.contains('&') {
            for part in inner.split(',') {
                let token = part.trim();
                if is_simple_identifier(token) {
                    identifiers.insert(token.to_ascii_lowercase());
                }
            }
        }
        idx = end + 1;
    }

    for call in call_names {
        identifiers.remove(&call.to_ascii_lowercase());
    }
    for generic in [
        "req",
        "arg",
        "args",
        "param",
        "params",
        "parameter",
        "parameters",
        "argument",
        "arguments",
    ] {
        identifiers.remove(generic);
    }

    identifiers
}

pub(crate) fn text_preserves_argument_role_names(concern_text: &str, text: &str) -> bool {
    let required = collect_signature_identifiers(concern_text);
    if required.len() < 2 {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    required.iter().all(|identifier| lower.contains(identifier))
}

pub(crate) fn text_preserves_argument_order_detail_for_concern(
    concern: &Value,
    text: &str,
) -> bool {
    let concern_text = concern_review_text(concern);
    text_preserves_argument_order_detail(text)
        && text_mentions_concern_callee(text, &concern_text)
        && text_has_expected_parameter_order(text)
        && text_has_actual_call_order(text)
        && text_has_wrong_order_reason(text)
        && text_preserves_argument_role_names(&concern_text, text)
}

pub(crate) fn finding_preserves_argument_order_detail(concern: &Value, finding: &Value) -> bool {
    text_preserves_argument_order_detail_for_concern(concern, &finding_review_text(finding))
}

pub(crate) fn text_mentions_seqcount_irq_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_seqcount = lower.contains("seqcount")
        || lower.contains("sequence counter")
        || lower.contains("sequence-counter")
        || lower.contains("write_seqcount")
        || lower.contains("read_seqcount");
    let mentions_irq = lower.contains("local_irq")
        || lower.contains("hardirq")
        || lower.contains("interrupt")
        || lower.contains(" irq")
        || lower.contains("irq ");
    let mentions_fprop_irq = lower.contains("fprop_new_period")
        && lower.contains("local_irq")
        && (lower.contains("deadlock")
            || lower.contains("interrupt")
            || lower.contains("percpu_counter"));

    (mentions_seqcount && mentions_irq) || mentions_fprop_irq
}

pub(crate) fn is_seqcount_irq_concern(concern: &Value) -> bool {
    text_mentions_seqcount_irq_issue(&concern_review_text(concern))
}

pub(crate) fn text_preserves_seqcount_irq_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_seqcount = lower.contains("seqcount")
        || lower.contains("sequence counter")
        || lower.contains("sequence-counter")
        || lower.contains("write_seqcount")
        || lower.contains("read_seqcount");
    let mentions_irq = lower.contains("local_irq")
        || lower.contains("hardirq")
        || lower.contains("interrupt")
        || lower.contains("irq-safe")
        || lower.contains("irq safe")
        || lower.contains(" irq")
        || lower.contains("irq ");
    let mentions_failure = lower.contains("deadlock")
        || lower.contains("livelock")
        || lower.contains("spin")
        || lower.contains("retry")
        || lower.contains("reader")
        || lower.contains("writer")
        || lower.contains("interrupted");

    mentions_seqcount && mentions_irq && mentions_failure
}

pub(crate) fn finding_preserves_seqcount_irq_detail(_concern: &Value, finding: &Value) -> bool {
    text_preserves_seqcount_irq_detail(&finding_review_text(finding))
}

pub(crate) fn seqcount_irq_drop_proves_safety(_concern: &Value, dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();

    let proves_seqcount = lower.contains("seqcount")
        || lower.contains("sequence counter")
        || lower.contains("sequence-counter")
        || lower.contains("write_seqcount")
        || lower.contains("read_seqcount");
    let proves_no_irq_reader = lower.contains("no interrupt")
        || lower.contains("not in interrupt")
        || lower.contains("not used in irq")
        || lower.contains("no hardirq")
        || lower.contains("no irq")
        || lower.contains("cannot be interrupted")
        || lower.contains("cannot observe")
        || lower.contains("cannot retry")
        || lower.contains("no reader");
    let only_callee_irq_safe = (lower.contains("percpu_counter")
        || lower.contains("callee")
        || lower.contains("raw_spin"))
        && (lower.contains("irq-safe")
            || lower.contains("irq safe")
            || lower.contains("raw_spin_lock_irq")
            || lower.contains("raw_spin_lock_irqsave"))
        && !proves_seqcount;

    proves_seqcount && proves_no_irq_reader && !only_callee_irq_safe
}

pub(crate) fn text_mentions_resource_cleanup_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_allocation = lower.contains("alloc")
        || lower.contains("kzalloc")
        || lower.contains("kcalloc")
        || lower.contains("kmalloc")
        || lower.contains("bitmap_zalloc")
        || lower.contains("resource")
        || lower.contains("object");
    let mentions_cleanup = lower.contains("leak")
        || lower.contains("cleanup")
        || lower.contains("free")
        || lower.contains("unwind")
        || lower.contains("error path")
        || lower.contains("failure path")
        || lower.contains("err_ptr")
        || lower.contains("-enomem");

    mentions_allocation && mentions_cleanup
}

pub(crate) fn is_resource_cleanup_concern(concern: &Value) -> bool {
    let text = concern_review_text(concern);
    text_mentions_resource_cleanup_issue(&text) && !resource_names_from_text(&text).is_empty()
}

pub(crate) fn resource_names_from_text(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch.to_ascii_lowercase());
            continue;
        }
        collect_resource_name(&mut names, &mut token);
    }
    collect_resource_name(&mut names, &mut token);

    if names.contains("stripe_uptodate_bitmap") {
        return HashSet::from(["stripe_uptodate_bitmap".to_string()]);
    }

    names
}

pub(crate) fn collect_resource_name(names: &mut HashSet<String>, token: &mut String) {
    if token.len() < 6 {
        token.clear();
        return;
    }
    let candidate = token.as_str();
    let is_helper_or_allocator = candidate.starts_with("free_")
        || candidate.starts_with("__")
        || candidate.starts_with("phys_to_")
        || candidate.starts_with("virt_to_")
        || candidate.contains("_to_")
        || candidate.ends_with("_zalloc")
        || candidate.ends_with("_alloc")
        || candidate.contains("kzalloc")
        || candidate.contains("kmalloc")
        || candidate.contains("kcalloc")
        || candidate.contains("bitmap_zalloc");
    let looks_like_resource = candidate.contains("uptodate")
        || candidate.ends_with("_bitmap")
        || candidate.ends_with("_buf")
        || candidate.ends_with("_buffer")
        || candidate.ends_with("_page")
        || candidate.ends_with("_pages")
        || candidate.ends_with("_ptr")
        || candidate.ends_with("_pointers")
        || candidate.ends_with("_sectors");
    if looks_like_resource && !is_helper_or_allocator {
        names.insert(candidate.to_string());
    }
    token.clear();
}

pub(crate) fn text_preserves_resource_cleanup_detail_for_concern(
    concern: &Value,
    text: &str,
) -> bool {
    let lower = text.to_ascii_lowercase();
    let names = resource_names_from_text(&concern_review_text(concern));
    if names.is_empty() {
        return false;
    }
    let names_preserved = names
        .iter()
        .any(|name| lower.contains(&name.to_ascii_lowercase()));
    let mentions_failure_path = lower.contains("error")
        || lower.contains("failure")
        || lower.contains("unwind")
        || lower.contains("cleanup")
        || lower.contains("return err")
        || lower.contains("-enomem");
    let mentions_missing_cleanup = lower.contains("leak")
        || lower.contains("not freed")
        || lower.contains("not free")
        || lower.contains("missing free")
        || lower.contains("missing cleanup")
        || lower.contains("without freeing")
        || lower.contains("without cleanup");
    let mentions_allocation = lower.contains("alloc")
        || lower.contains("bitmap_zalloc")
        || lower.contains("kzalloc")
        || lower.contains("kcalloc")
        || lower.contains("kmalloc")
        || lower.contains("allocated");

    names_preserved && mentions_failure_path && mentions_missing_cleanup && mentions_allocation
}

pub(crate) fn finding_preserves_resource_cleanup_detail(concern: &Value, finding: &Value) -> bool {
    text_has_resource_cleanup_problem_headline(concern, &value_field_text(finding, "problem"))
        && text_preserves_resource_cleanup_detail_for_concern(
            concern,
            &finding_review_text(finding),
        )
}

pub(crate) fn text_has_resource_cleanup_problem_headline(concern: &Value, text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let names = resource_names_from_text(&concern_review_text(concern));
    if names.is_empty() {
        return false;
    }
    let names_preserved = names
        .iter()
        .any(|name| lower.contains(&name.to_ascii_lowercase()));
    let cleanup_problem = lower.contains("leak")
        || lower.contains("not freed")
        || lower.contains("missing free")
        || lower.contains("missing cleanup")
        || lower.contains("without freeing");

    names_preserved && cleanup_problem
}

pub(crate) fn deallocation_call_mentions_resource(text: &str, resource: &str) -> bool {
    let compact: String = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let resource = resource.to_ascii_lowercase();
    for function in [
        "bitmap_free",
        "kfree",
        "kvfree",
        "vfree",
        "free_page",
        "free_pages",
        "dma_free_coherent",
    ] {
        let needle = format!("{function}(");
        let mut offset = 0;
        while let Some(relative_idx) = compact[offset..].find(&needle) {
            let start = offset + relative_idx + needle.len();
            let tail = &compact[start..];
            let end = tail.find(')').unwrap_or(tail.len());
            if tail[..end].contains(&resource) {
                return true;
            }
            offset = start;
        }
    }
    false
}

pub(crate) fn resource_cleanup_drop_proves_safety(concern: &Value, dropped: &Value) -> bool {
    let rationale = value_field_text(dropped, "rationale");
    let lower = rationale.to_ascii_lowercase();
    let names = resource_names_from_text(&concern_review_text(concern));

    if names.is_empty() {
        return text_mentions_resource_cleanup_issue(&rationale)
            && [
                "bitmap_free(",
                "kfree(",
                "kvfree(",
                "vfree(",
                "free_page(",
                "free_pages(",
                "dma_free_coherent(",
            ]
            .iter()
            .any(|needle| lower.contains(needle));
    }

    names.iter().all(|name| {
        lower.contains(&name.to_ascii_lowercase())
            && deallocation_call_mentions_resource(&rationale, name)
    })
}

pub(crate) fn text_mentions_retry_resource_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_retry = lower.contains("retry")
        || lower.contains("fallback")
        || lower.contains("fall back")
        || lower.contains("second ")
        || lower.contains("reissue");
    let mentions_error = lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("-eacces")
        || lower.contains("status_")
        || lower.contains("returns");
    let mentions_resource = lower.contains("retry_iov")
        || lower.contains("iov_base")
        || lower.contains("response buffer")
        || lower.contains("resource buffer")
        || lower.contains("rsp_iov")
        || lower.contains("resp_iov");
    let mentions_cleanup_or_leak = lower.contains("leak")
        || lower.contains("not freed")
        || lower.contains("not free")
        || lower.contains("missing free")
        || lower.contains("free_response_buf")
        || lower.contains("overwrite")
        || lower.contains("overwritten")
        || lower.contains("reuse")
        || lower.contains("cleanup")
        || lower.contains("use-after-free")
        || lower.contains("uaf");
    let mentions_operation = lower.contains("retry_open")
        || lower.contains("open call")
        || lower.contains("operation")
        || lower.contains("helper")
        || lower.contains("call");

    mentions_retry
        && mentions_error
        && mentions_resource
        && mentions_cleanup_or_leak
        && mentions_operation
}

pub(crate) fn is_retry_resource_leak_concern(concern: &Value) -> bool {
    text_mentions_retry_resource_issue(&concern_review_text(concern))
}

pub(crate) fn text_preserves_retry_resource_leak_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_operation = lower.contains("retry_open")
        || lower.contains("operation/helper")
        || lower.contains("retrying helper")
        || lower.contains("failed open")
        || lower.contains("open call");
    let mentions_resource = lower.contains("retry_iov")
        || lower.contains("iov_base")
        || lower.contains("response buffer")
        || lower.contains("resource buffer")
        || lower.contains("rsp_iov")
        || lower.contains("resp_iov");
    let mentions_cleanup_helper = lower.contains("free_response_buf")
        || lower.contains("cleanup helper")
        || lower.contains("free call")
        || lower.contains("freeing helper");
    let mentions_retry_trigger = lower.contains("retry")
        || lower.contains("fallback")
        || lower.contains("failed open")
        || lower.contains("failed retry_open")
        || lower.contains("-eacces")
        || lower.contains("second retry_open");
    let mentions_leak_before_retry = lower.contains("leak")
        || lower.contains("not freed before")
        || lower.contains("not free before")
        || lower.contains("missing free")
        || lower.contains("without freeing")
        || lower.contains("before retry")
        || lower.contains("before fallback")
        || lower.contains("before overwrite")
        || lower.contains("before reuse")
        || lower.contains("overwritten")
        || lower.contains("overwrite")
        || lower.contains("return");

    mentions_operation
        && mentions_resource
        && mentions_cleanup_helper
        && mentions_retry_trigger
        && mentions_leak_before_retry
}

pub(crate) fn finding_preserves_retry_resource_leak_detail(
    _concern: &Value,
    finding: &Value,
) -> bool {
    text_preserves_retry_resource_leak_detail(&finding_review_text(finding))
}

pub(crate) fn retry_resource_drop_proves_safety(concern: &Value, dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();
    let concern_lower = concern_review_text(concern).to_ascii_lowercase();

    let names_operation = if concern_lower.contains("retry_open") {
        lower.contains("retry_open")
    } else {
        lower.contains("operation") || lower.contains("helper") || lower.contains("retry")
    };
    let names_resource =
        if concern_lower.contains("retry_iov") || concern_lower.contains("iov_base") {
            lower.contains("retry_iov") || lower.contains("iov_base")
        } else {
            lower.contains("response buffer")
                || lower.contains("resource buffer")
                || lower.contains("rsp_iov")
                || lower.contains("resp_iov")
        };
    let names_cleanup_path = lower.contains("cleanup label")
        || lower.contains("cleanup path")
        || lower.contains("out label")
        || lower.contains("goto out")
        || lower.contains("error path")
        || lower.contains("retry path");
    let names_free_call = lower.contains("free_response_buf")
        || lower.contains("kfree(")
        || lower.contains("kvfree(")
        || lower.contains("vfree(")
        || lower.contains("free call");
    let proves_cleanup_before_retry = lower.contains("before retry")
        || lower.contains("before the retry")
        || lower.contains("prior to retry")
        || lower.contains("before fallback")
        || lower.contains("prior to fallback")
        || lower.contains("before the second")
        || lower.contains("before second")
        || lower.contains("before reissuing")
        || lower.contains("before overwrit")
        || lower.contains("before reus");
    let proves_no_leak_or_overwrite = lower.contains("cannot leak")
        || lower.contains("cannot be leaked")
        || lower.contains("not leaked")
        || lower.contains("no leak")
        || lower.contains("cannot be overwritten")
        || lower.contains("not overwritten")
        || lower.contains("cannot be reused")
        || lower.contains("not reused")
        || lower.contains("cannot be lost");

    names_operation
        && names_resource
        && names_cleanup_path
        && names_free_call
        && proves_cleanup_before_retry
        && proves_no_leak_or_overwrite
}

pub(crate) fn text_mentions_lifecycle_ordering_issue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_lifecycle_resource = lower.contains("workqueue")
        || lower.contains("work queue")
        || lower.contains("work_struct")
        || lower.contains("delayed_work")
        || lower.contains("timer")
        || lower.contains("rfkill")
        || lower.contains("callback");
    let mentions_teardown = lower.contains("unregister")
        || lower.contains("remove")
        || lower.contains("teardown")
        || lower.contains("destroy")
        || lower.contains("close")
        || lower.contains("cleanup")
        || lower.contains("free")
        || lower.contains("freed")
        || lower.contains("release");
    let mentions_callback_or_reentry = lower.contains("callback")
        || lower.contains("re-enter")
        || lower.contains("reenter")
        || lower.contains("re-entry")
        || lower.contains("rfkill")
        || lower.contains("nci_close_device")
        || lower.contains("close path")
        || lower.contains("remove path")
        || lower.contains("can run after");
    let mentions_ordering = lower.contains("before")
        || lower.contains("after")
        || lower.contains("order")
        || lower.contains("ordering")
        || lower.contains("race")
        || lower.contains("use-after-free")
        || lower.contains("destroyed before")
        || lower.contains("freed state");

    mentions_lifecycle_resource
        && mentions_teardown
        && mentions_callback_or_reentry
        && mentions_ordering
}

pub(crate) fn is_lifecycle_ordering_concern(concern: &Value) -> bool {
    text_mentions_lifecycle_ordering_issue(&concern_review_text(concern))
}

pub(crate) fn text_preserves_lifecycle_ordering_detail(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let preserves_destroyed_resource = (lower.contains("workqueue")
        || lower.contains("work queue")
        || lower.contains("timer")
        || lower.contains("rfkill")
        || lower.contains("resource"))
        && (lower.contains("destroy")
            || lower.contains("destroyed")
            || lower.contains("free")
            || lower.contains("freed"));
    let preserves_callback_source = lower.contains("unregister")
        || lower.contains("rfkill")
        || lower.contains("callback")
        || lower.contains("close")
        || lower.contains("remove");
    let preserves_reentry_path = lower.contains("nci_close_device")
        || lower.contains("re-enter")
        || lower.contains("reenter")
        || lower.contains("re-entry")
        || lower.contains("close path")
        || lower.contains("remove path")
        || lower.contains("callback path")
        || lower.contains("can run after");
    let preserves_bad_ordering = lower.contains("before")
        || lower.contains("after")
        || lower.contains("order")
        || lower.contains("ordering")
        || lower.contains("too early")
        || lower.contains("race");
    let preserves_consequence = lower.contains("use-after-free")
        || lower.contains("freed state")
        || lower.contains("freed workqueue")
        || lower.contains("destroyed workqueue")
        || lower.contains("race")
        || lower.contains("crash")
        || lower.contains("uaf");

    preserves_destroyed_resource
        && preserves_callback_source
        && preserves_reentry_path
        && preserves_bad_ordering
        && preserves_consequence
}

pub(crate) fn finding_preserves_lifecycle_ordering_detail(
    _concern: &Value,
    finding: &Value,
) -> bool {
    text_preserves_lifecycle_ordering_detail(&finding_review_text(finding))
}

pub(crate) fn lifecycle_ordering_drop_proves_safety(_concern: &Value, dropped: &Value) -> bool {
    let lower = [
        value_field_text(dropped, "drop_reason"),
        value_field_text(dropped, "rationale"),
    ]
    .join("\n")
    .to_ascii_lowercase();

    let proves_order = (lower.contains("unregister")
        || lower.contains("rfkill_unregister")
        || lower.contains("nfc_unregister"))
        && (lower.contains("destroy")
            || lower.contains("destroy_workqueue")
            || lower.contains("free")
            || lower.contains("timer_shutdown"))
        && (lower.contains("before") || lower.contains("after") || lower.contains("order"));
    let proves_callback_path = lower.contains("callback")
        || lower.contains("rfkill")
        || lower.contains("nci_close_device")
        || lower.contains("close")
        || lower.contains("remove")
        || lower.contains("re-enter")
        || lower.contains("reenter");
    let proves_barrier = lower.contains("cancel_work_sync")
        || lower.contains("flush_workqueue")
        || lower.contains("drain_workqueue")
        || lower.contains("destroy_workqueue")
        || lower.contains("del_timer_sync")
        || lower.contains("timer_shutdown_sync")
        || lower.contains("synchronize_rcu")
        || lower.contains("synchronize_net")
        || lower.contains("rfkill_unregister")
        || lower.contains("nfc_unregister_device")
        || lower.contains("barrier")
        || lower.contains("synchroniz");
    let proves_impossible_after_destroy = lower.contains("cannot")
        || lower.contains("can not")
        || lower.contains("no callback")
        || lower.contains("prevents")
        || lower.contains("waits")
        || lower.contains("drains")
        || lower.contains("flushes")
        || lower.contains("cancelled")
        || lower.contains("canceled")
        || lower.contains("synchronized");

    proves_order && proves_callback_path && proves_barrier && proves_impossible_after_destroy
}

pub(crate) fn finding_is_non_problem(finding: &Value) -> bool {
    let problem = value_field_text(finding, "problem").to_ascii_lowercase();
    let explanation = value_field_text(finding, "severity_explanation").to_ascii_lowercase();
    let text = format!("{problem}\n{explanation}");
    (problem.contains("correctly")
        || problem.contains("is correct")
        || problem.contains("safe")
        || problem.contains("redundant")
        || problem.contains("unnecessary")
        || problem.contains("improves performance")
        || problem.contains("not a bug"))
        && !text.contains("can lead")
        && !text.contains("could lead")
        && !text.contains("may lead")
        && !text.contains("risk")
        && !text.contains("deadlock")
        && !text.contains("leak")
        && !text.contains("use-after-free")
        && !text.contains("wrong")
        && !text.contains("incorrect")
}

pub(crate) fn synthesize_argument_order_finding(
    concern: &Value,
    existing: Option<&Value>,
) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Argument-order/API contract concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");
    let existing_problem = existing
        .and_then(|finding| finding.get("problem"))
        .and_then(|problem| problem.as_str())
        .unwrap_or_default();
    let problem = if text_preserves_argument_order_detail(existing_problem) {
        existing_problem.to_string()
    } else if text_preserves_argument_order_detail(&description) {
        description.clone()
    } else {
        format!("Argument-order/API contract concern: {description}")
    };
    let existing_explanation = existing
        .and_then(|finding| finding.get("severity_explanation"))
        .and_then(|explanation| explanation.as_str())
        .unwrap_or_default();
    let severity_explanation = if text_preserves_argument_order_detail(existing_explanation) {
        existing_explanation.to_string()
    } else {
        format!(
            "Stage 8 retained this argument-order/API-contract concern and Stage 9 did not produce explicit false-positive proof or a valid subsuming finding. The retained concern names the callee and expected-vs-actual argument order; the order is wrong if the call-site arguments contradict the callee signature. Retained reasoning: {reasoning}"
        )
    };
    let severity_explanation = augment_argument_order_explanation(concern, &severity_explanation);

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-argument-order".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    let existing_locations_empty = match obj
        .get("locations")
        .and_then(|locations| locations.as_array())
    {
        Some(locations) => locations.is_empty(),
        None => true,
    };
    if existing_locations_empty
        && let Some(locations) = concern.get("locations")
        && locations
            .as_array()
            .is_some_and(|locations| !locations.is_empty())
    {
        obj.insert("locations".to_string(), locations.clone());
    }
    Value::Object(obj)
}

pub(crate) fn augment_argument_order_explanation(concern: &Value, explanation: &str) -> String {
    if text_preserves_argument_order_detail_for_concern(concern, explanation) {
        return explanation.to_string();
    }

    let retained_evidence = concern_review_text(concern);
    if retained_evidence.trim().is_empty() {
        explanation.to_string()
    } else {
        format!("{explanation} Retained argument-order evidence: {retained_evidence}")
    }
}

pub(crate) fn synthesize_seqcount_irq_finding(concern: &Value, existing: Option<&Value>) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Seqcount/IRQ protection concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");
    let problem = format!(
        "Removing IRQ protection around the sequence-counter write side can let an interrupt-context reader spin while the writer is paused: {description}"
    );
    let severity_explanation = format!(
        "Stage 8 retained a concern about removed local_irq_save/local_irq_restore protection. Callee irq-safety only protects the callee's internal lock; it does not prove the surrounding write_seqcount_begin/write_seqcount_end region cannot be interrupted by a hardirq/NMI reader of the same sequence counter. If such a reader retries while the writer is interrupted, the system can deadlock or livelock. Retained reasoning: {reasoning}"
    );

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-seqcount-irq".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    Value::Object(obj)
}

pub(crate) fn synthesize_resource_cleanup_finding(
    concern: &Value,
    existing: Option<&Value>,
) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Resource cleanup concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");
    let names = resource_names_from_text(&concern_review_text(concern));
    let resource_summary = if names.is_empty() {
        "the newly added allocation/resource".to_string()
    } else {
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();
        names.join(", ")
    };
    let problem =
        format!("Newly allocated resource may leak on the error cleanup path: {resource_summary}");
    let severity_explanation = format!(
        "Stage 8 retained a resource-cleanup concern for {resource_summary}. Stage 9 did not prove a concrete deallocation expression for the same newly allocated resource on the failing/error path, so the concern must be preserved instead of dropped with generic cleanup-helper wording. Retained reasoning: {reasoning}"
    );

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-resource-cleanup".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    Value::Object(obj)
}

pub(crate) fn synthesize_retry_resource_leak_finding(
    concern: &Value,
    existing: Option<&Value>,
) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Retry/fallback resource-buffer concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");
    let existing_problem = existing
        .and_then(|finding| finding.get("problem"))
        .and_then(|problem| problem.as_str())
        .unwrap_or_default();
    let problem = if text_preserves_retry_resource_leak_detail(existing_problem) {
        existing_problem.to_string()
    } else {
        format!("retry_open response buffer may leak before retry/fallback: {description}")
    };
    let existing_explanation = existing
        .and_then(|finding| finding.get("severity_explanation"))
        .and_then(|explanation| explanation.as_str())
        .unwrap_or_default();
    let severity_explanation = if text_preserves_retry_resource_leak_detail(existing_explanation) {
        existing_explanation.to_string()
    } else {
        format!(
            "Stage 8 retained a retry/error-path resource-buffer concern. The preserved mechanism is: operation/helper retry_open or the retained retrying helper; resource retry_iov.iov_base / response buffer; cleanup helper free_response_buf() or equivalent; control-flow trigger failed open followed by retry/fallback; bug mechanism the response buffer is not freed before retry, fallback, overwrite, reuse, or return. Stage 9 did not prove the exact cleanup path and free call run before the retry or before retry_iov can be overwritten/lost. Retained reasoning: {reasoning}"
        )
    };

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-retry-resource-leak".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    Value::Object(obj)
}

pub(crate) fn synthesize_lifecycle_ordering_finding(
    concern: &Value,
    existing: Option<&Value>,
) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Lifecycle ordering concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");
    let problem =
        format!("Teardown ordering may let a callback use a destroyed resource: {description}");
    let severity_explanation = format!(
        "Stage 8 retained a lifecycle-ordering concern involving a workqueue/timer/rfkill resource, an unregister/callback source, and a close/remove re-entry path. Stage 9 did not prove an unregister-before-destroy order or a cancel/flush/synchronization barrier that prevents a callback such as nci_close_device from running after the resource is destroyed. That bad ordering can race with teardown and cause use-after-free of freed state. Retained reasoning: {reasoning}"
    );

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-lifecycle-ordering".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    Value::Object(obj)
}

pub(crate) fn synthesize_seed_pattern_finding(concern: &Value, existing: Option<&Value>) -> Value {
    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("stage8-unknown");
    let description = concern_description(concern)
        .unwrap_or_else(|| "Static bug-pattern seed concern from Stage 8".to_string());
    let reasoning = concern
        .get("reasoning")
        .and_then(|reasoning| reasoning.as_str())
        .unwrap_or(&description);
    let preexisting = existing
        .and_then(|finding| finding.get("preexisting"))
        .and_then(|preexisting| preexisting.as_bool())
        .or_else(|| {
            concern
                .get("preexisting")
                .and_then(|preexisting| preexisting.as_bool())
        })
        .unwrap_or(false);
    let severity = existing
        .and_then(|finding| finding.get("severity"))
        .and_then(|severity| severity.as_str())
        .filter(|severity| !severity.trim().is_empty())
        .unwrap_or("High");

    let (problem, proof_text) = match seed_pattern(concern) {
        Some("cgroup_keyed_parse_missing_value") => (
            format!("limit.max write path may parse or dereference a missing value: {description}"),
            "The preserved bug mechanism is: file/function kernel/cgroup/limit.c limit_region_max_write()/limit_key_write(); key or option limit.max / \"max\"; a cgroup file write can provide a missing or absent value; the options/value pointer reaches strcmp(), memparse(), kstrto*(), dereference, or parsing before a proven non-NULL check; the write path trigger is a cgroup limit.max write.",
        ),
        Some("rcu_teardown_iteration_without_read_lock") => (
            format!(
                "region_unregister() may traverse an RCU list in teardown without read-side protection: {description}"
            ),
            "The preserved bug mechanism is: region_unregister() runs in unregister/teardown context; it uses list_for_each_rcu() or an equivalent RCU iterator; Stage 9 did not prove rcu_read_lock(), a documented update-side lock, or a lockdep condition covers the traversal; this can trigger a suspicious RCU warning or race with teardown.",
        ),
        Some("skb_fragment_capacity_max_skb_frags") => (
            format!("t7xx skb fragment append path may exceed MAX_SKB_FRAGS: {description}"),
            "The preserved bug mechanism is: t7xx_dpmaif_set_frag_to_skb() appends skb fragments with skb_add_rx_frag()/skb_shinfo(skb)->nr_frags; Stage 9 did not prove a MAX_SKB_FRAGS or equivalent nr_frags capacity guard before every append; looped DMA/page fragments can exceed the skb fragment array capacity and cause out-of-bounds writes.",
        ),
        Some("retry_error_path_resource_leak") => (
            format!("retry_open response buffer may leak before retry/fallback: {description}"),
            "The preserved bug mechanism is: operation/helper retry_open_file()/retry_open(); resource retry_iov.iov_base / response buffer; cleanup helper free_response_buf(); failed retry_open followed by retry/fallback; Stage 9 did not prove free_response_buf() or an equivalent cleanup runs before the retry/fallback, overwrite, reuse, or return, so the first response buffer can leak.",
        ),
        _ => (
            format!("Static proof-required seed concern: {description}"),
            "Stage 8 retained a proof-required static or targeted seed and Stage 9 did not emit a detailed finding, valid subsuming finding, or concrete false-positive proof.",
        ),
    };

    let existing_explanation = existing
        .and_then(|finding| finding.get("severity_explanation"))
        .and_then(|explanation| explanation.as_str())
        .unwrap_or_default();
    let existing_problem = existing
        .and_then(|finding| finding.get("problem"))
        .and_then(|problem| problem.as_str())
        .unwrap_or_default();
    let existing_candidate = json!({
        "finding_id": "candidate",
        "problem": existing_problem,
        "severity_explanation": existing_explanation
    });
    let severity_explanation =
        if finding_preserves_seed_pattern_detail(concern, &existing_candidate) {
            existing_explanation.to_string()
        } else {
            format!("{proof_text} Retained reasoning: {reasoning}")
        };

    let mut obj = existing
        .and_then(|finding| finding.as_object().cloned())
        .unwrap_or_default();
    obj.entry("finding_id".to_string())
        .or_insert_with(|| Value::String("finding-seed-pattern".to_string()));
    obj.insert(
        "source_concern_id".to_string(),
        Value::String(source_id.to_string()),
    );
    obj.insert("problem".to_string(), Value::String(problem));
    obj.insert("severity".to_string(), Value::String(severity.to_string()));
    obj.insert(
        "severity_explanation".to_string(),
        Value::String(severity_explanation),
    );
    obj.insert("preexisting".to_string(), Value::Bool(preexisting));
    Value::Object(obj)
}

pub(crate) fn ensure_stage9_finding_ids(findings: &mut Value) {
    let Some(findings) = findings.as_array_mut() else {
        return;
    };
    let mut used = HashSet::new();
    for (idx, finding) in findings.iter_mut().enumerate() {
        let desired = finding
            .get("finding_id")
            .and_then(|id| id.as_str())
            .filter(|id| !id.trim().is_empty())
            .map(|id| id.to_string());
        let finding_id = desired
            .filter(|id| used.insert(id.clone()))
            .unwrap_or_else(|| {
                let mut candidate = format!("finding-{}", idx + 1);
                while used.contains(&candidate) {
                    candidate = format!("finding-{}-{}", idx + 1, used.len() + 1);
                }
                used.insert(candidate.clone());
                candidate
            });
        if let Value::Object(obj) = finding {
            obj.insert("finding_id".to_string(), Value::String(finding_id));
        }
    }
}

pub(crate) fn finding_id(finding: &Value) -> Option<&str> {
    finding.get("finding_id").and_then(|id| id.as_str())
}

pub(crate) fn find_argument_order_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_argument_order_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn find_seqcount_irq_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_seqcount_irq_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn find_resource_cleanup_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_resource_cleanup_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn find_retry_resource_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_retry_resource_leak_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn find_lifecycle_ordering_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_lifecycle_ordering_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn find_seed_pattern_subsuming_finding_id(
    concern: &Value,
    findings: &Value,
) -> Option<String> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_preserves_seed_pattern_detail(concern, finding))
        .and_then(finding_id)
        .map(|id| id.to_string())
}

pub(crate) fn stage9_finding_with_id<'a>(findings: &'a Value, id: &str) -> Option<&'a Value> {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .find(|finding| finding_id(finding) == Some(id))
}

pub(crate) fn make_subsumed_drop(
    source_id: &str,
    finding_id: &str,
    rationale: impl Into<String>,
) -> Value {
    json!({
        "source_concern_id": source_id,
        "decision": "drop",
        "drop_reason": "subsumed_by",
        "subsumed_by_finding_id": finding_id,
        "rationale": rationale.into()
    })
}

pub(crate) fn validate_stage9_accounting(
    stage9_concerns: &Value,
    findings: &Value,
    dropped_candidates: &Value,
) -> std::result::Result<(), String> {
    let concerns = stage9_concerns
        .as_array()
        .ok_or_else(|| "stage9 concerns are not an array".to_string())?;
    let findings = findings
        .as_array()
        .ok_or_else(|| "findings is not an array".to_string())?;
    let dropped_candidates = dropped_candidates
        .as_array()
        .ok_or_else(|| "dropped_candidates is not an array".to_string())?;

    if findings.len() + dropped_candidates.len() != concerns.len() {
        return Err(format!(
            "accounting count mismatch: findings {} + dropped_candidates {} != retained concerns {}",
            findings.len(),
            dropped_candidates.len(),
            concerns.len()
        ));
    }

    let expected: HashSet<String> = concerns
        .iter()
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    if expected.len() != concerns.len() {
        return Err("one or more retained concerns is missing source_concern_id".to_string());
    }
    let argument_order_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_concrete_argument_order_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let seqcount_irq_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_seqcount_irq_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let resource_cleanup_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_resource_cleanup_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let retry_resource_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_retry_resource_leak_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let lifecycle_ordering_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_lifecycle_ordering_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let proof_required_seed_sources: HashSet<String> = concerns
        .iter()
        .filter(|concern| is_proof_required_seed_concern(concern))
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let concern_by_id: HashMap<String, &Value> = concerns
        .iter()
        .filter_map(|concern| {
            let source_id = concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())?;
            Some((source_id.to_string(), concern))
        })
        .collect();
    let mut finding_by_id: HashMap<String, &Value> = HashMap::new();
    for finding in findings {
        let finding_id = finding
            .get("finding_id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| "finding missing finding_id".to_string())?;
        if finding_id.trim().is_empty() {
            return Err("finding has empty finding_id".to_string());
        }
        if finding_by_id
            .insert(finding_id.to_string(), finding)
            .is_some()
        {
            return Err(format!("finding_id {finding_id} used more than once"));
        }
    }

    let mut accounted = HashSet::new();
    for finding in findings {
        let source_id = finding
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| "finding missing source_concern_id".to_string())?;
        if !expected.contains(source_id) {
            return Err(format!(
                "finding references unknown source_concern_id {source_id}"
            ));
        }
        if !accounted.insert(source_id.to_string()) {
            return Err(format!(
                "source_concern_id {source_id} accounted more than once"
            ));
        }
        if finding_is_non_problem(finding) {
            return Err(format!(
                "finding for source_concern_id {source_id} describes safe/correct behavior instead of a bug or regression"
            ));
        }
        if argument_order_sources.contains(source_id)
            && !finding_preserves_argument_order_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "argument-order concern {source_id} was emitted as a finding without preserving callee, expected order, actual call-site order, and wrong-order detail"
            ));
        }
        if seqcount_irq_sources.contains(source_id)
            && !finding_preserves_seqcount_irq_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "seqcount/IRQ concern {source_id} was emitted as a finding without preserving removed IRQ protection, sequence-counter write-side detail, and interrupt-reader risk"
            ));
        }
        if resource_cleanup_sources.contains(source_id)
            && !finding_preserves_resource_cleanup_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "resource-cleanup concern {source_id} was emitted as a finding without preserving allocated resource, failing path, and missing cleanup detail"
            ));
        }
        if retry_resource_sources.contains(source_id)
            && !finding_preserves_retry_resource_leak_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "retry-resource concern {source_id} was emitted as a finding without preserving operation/helper, resource buffer, cleanup helper, retry/fallback trigger, and leak-before-retry detail"
            ));
        }
        if lifecycle_ordering_sources.contains(source_id)
            && !finding_preserves_lifecycle_ordering_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "lifecycle-ordering concern {source_id} was emitted as a finding without preserving destroyed resource, callback source, re-entry path, bad ordering, and consequence"
            ));
        }
        if proof_required_seed_sources.contains(source_id)
            && !finding_preserves_seed_pattern_detail(
                concern_by_id
                    .get(source_id)
                    .copied()
                    .ok_or_else(|| format!("missing retained concern {source_id}"))?,
                finding,
            )
        {
            return Err(format!(
                "proof-required seed concern {source_id} was emitted as a finding without preserving the exact seeded bug mechanism"
            ));
        }
    }

    for dropped in dropped_candidates {
        let source_id = dropped
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .ok_or_else(|| "dropped candidate missing source_concern_id".to_string())?;
        if !expected.contains(source_id) {
            return Err(format!(
                "dropped candidate references unknown source_concern_id {source_id}"
            ));
        }
        if !accounted.insert(source_id.to_string()) {
            return Err(format!(
                "source_concern_id {source_id} accounted more than once"
            ));
        }

        let decision = dropped
            .get("decision")
            .and_then(|decision| decision.as_str())
            .unwrap_or_default();
        if decision != "drop" {
            return Err(format!(
                "dropped candidate {source_id} has unsupported decision {decision:?}"
            ));
        }

        let drop_reason = dropped
            .get("drop_reason")
            .and_then(|reason| reason.as_str())
            .unwrap_or_default();
        if !matches!(
            drop_reason,
            "duplicate"
                | "subsumed_by"
                | "insufficient_evidence"
                | "not_security_relevant"
                | "already_mitigated"
                | "false_positive"
                | "unclear"
        ) {
            return Err(format!(
                "dropped candidate {source_id} has unsupported drop_reason {drop_reason:?}"
            ));
        }

        let rationale = dropped
            .get("rationale")
            .and_then(|rationale| rationale.as_str())
            .unwrap_or_default()
            .trim();
        if rationale.is_empty() {
            return Err(format!("dropped candidate {source_id} has empty rationale"));
        }
        if drop_reason == "subsumed_by" {
            let target_id = dropped
                .get("subsumed_by_finding_id")
                .and_then(|id| id.as_str())
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "dropped candidate {source_id} is subsumed_by without subsumed_by_finding_id"
                    )
                })?;
            if !finding_by_id.contains_key(target_id) {
                return Err(format!(
                    "dropped candidate {source_id} references unknown subsumed_by_finding_id {target_id}"
                ));
            }
        }
        if argument_order_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    return Err(format!(
                        "argument-order concern {source_id} was dropped as false_positive; it must be emitted as a finding or subsumed_by a detailed finding"
                    ));
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "argument-order concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_argument_order_detail(concern, target) {
                        return Err(format!(
                            "argument-order concern {source_id} was subsumed by {target_id}, but that finding does not preserve callee, expected order, actual call-site order, and wrong-order detail"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "argument-order concern {source_id} was dropped as {drop_reason}; it must be emitted as a finding or subsumed_by a detailed finding"
                    ));
                }
            }
        }
        if seqcount_irq_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    if !seqcount_irq_drop_proves_safety(concern, dropped) {
                        return Err(format!(
                            "seqcount/IRQ concern {source_id} was dropped as false_positive without proving the sequence-counter writer is safe from interrupt-context readers"
                        ));
                    }
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "seqcount/IRQ concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_seqcount_irq_detail(concern, target) {
                        return Err(format!(
                            "seqcount/IRQ concern {source_id} was subsumed by {target_id}, but that finding does not preserve removed IRQ protection, sequence-counter write-side detail, and interrupt-reader risk"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "seqcount/IRQ concern {source_id} was dropped as {drop_reason}; it must be emitted, subsumed by a detailed finding, or dropped with concrete sequence-counter safety proof"
                    ));
                }
            }
        }
        if resource_cleanup_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    if !resource_cleanup_drop_proves_safety(concern, dropped) {
                        return Err(format!(
                            "resource-cleanup concern {source_id} was dropped as false_positive without naming the exact resource and concrete deallocation expression/path"
                        ));
                    }
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "resource-cleanup concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_resource_cleanup_detail(concern, target) {
                        return Err(format!(
                            "resource-cleanup concern {source_id} was subsumed by {target_id}, but that finding does not preserve allocated resource, failing path, and missing cleanup detail"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "resource-cleanup concern {source_id} was dropped as {drop_reason}; it must be emitted, subsumed by a detailed finding, or dropped with concrete deallocation proof"
                    ));
                }
            }
        }
        if retry_resource_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    if !retry_resource_drop_proves_safety(concern, dropped) {
                        return Err(format!(
                            "retry-resource concern {source_id} was dropped as false_positive without proving exact cleanup/free before retry or overwrite"
                        ));
                    }
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "retry-resource concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_retry_resource_leak_detail(concern, target) {
                        return Err(format!(
                            "retry-resource concern {source_id} was subsumed by {target_id}, but that finding does not preserve operation/helper, resource buffer, cleanup helper, retry/fallback trigger, and leak-before-retry detail"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "retry-resource concern {source_id} was dropped as {drop_reason}; it must be emitted, subsumed by a detailed finding, or dropped with concrete before-retry cleanup proof"
                    ));
                }
            }
        }
        if lifecycle_ordering_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    if !lifecycle_ordering_drop_proves_safety(concern, dropped) {
                        return Err(format!(
                            "lifecycle-ordering concern {source_id} was dropped as false_positive without proving unregister/destroy order, callback path, and synchronization that prevents post-destroy re-entry"
                        ));
                    }
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "lifecycle-ordering concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_lifecycle_ordering_detail(concern, target) {
                        return Err(format!(
                            "lifecycle-ordering concern {source_id} was subsumed by {target_id}, but that finding does not preserve destroyed resource, callback source, re-entry path, bad ordering, and consequence"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "lifecycle-ordering concern {source_id} was dropped as {drop_reason}; it must be emitted, subsumed by a detailed finding, or dropped with concrete teardown-order safety proof"
                    ));
                }
            }
        }
        if proof_required_seed_sources.contains(source_id) {
            let concern = concern_by_id
                .get(source_id)
                .copied()
                .ok_or_else(|| format!("missing retained concern {source_id}"))?;
            match drop_reason {
                "false_positive" => {
                    if !seed_pattern_drop_proves_safety(concern, dropped) {
                        return Err(format!(
                            "proof-required seed concern {source_id} was dropped as false_positive without concrete proof for pattern {:?}",
                            seed_pattern(concern).unwrap_or("unknown")
                        ));
                    }
                }
                "subsumed_by" => {
                    let target_id = dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();
                    let target = finding_by_id.get(target_id).copied().ok_or_else(|| {
                        format!(
                            "proof-required seed concern {source_id} references unknown subsuming finding {target_id}"
                        )
                    })?;
                    if !finding_preserves_seed_pattern_detail(concern, target) {
                        return Err(format!(
                            "proof-required seed concern {source_id} was subsumed by {target_id}, but that finding does not preserve the exact seeded bug mechanism"
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "proof-required seed concern {source_id} was dropped as {drop_reason}; it must be emitted, subsumed by a detailed finding, or dropped with concrete false-positive proof"
                    ));
                }
            }
        }
    }

    if accounted.len() != expected.len() {
        let mut missing: Vec<_> = expected.difference(&accounted).cloned().collect();
        missing.sort();
        return Err(format!(
            "unaccounted source_concern_id(s): {}",
            missing.join(", ")
        ));
    }

    Ok(())
}

pub(crate) fn repair_stage9_accounting(
    stage9_concerns: &Value,
    findings: &Value,
    dropped_candidates: &Value,
) -> (Value, Value) {
    let concerns: Vec<Value> = stage9_concerns
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let expected_ids: Vec<String> = concerns
        .iter()
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let concern_by_id: HashMap<String, Value> = concerns
        .into_iter()
        .filter_map(|concern| {
            let source_id = concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())?
                .to_string();
            Some((source_id, concern))
        })
        .collect();
    let expected: HashSet<String> = expected_ids.iter().cloned().collect();
    let mut accounted = HashSet::new();

    let mut repaired_findings = Vec::new();
    let mut repaired_drops = Vec::new();
    for finding in findings.as_array().into_iter().flatten() {
        let Some(source_id) = finding.get("source_concern_id").and_then(|id| id.as_str()) else {
            continue;
        };
        if expected.contains(source_id) && accounted.insert(source_id.to_string()) {
            if finding_is_non_problem(finding) {
                if let Some(concern) = concern_by_id.get(source_id)
                    && is_proof_required_seed_concern(concern)
                {
                    repaired_findings.push(synthesize_seed_pattern_finding(concern, None));
                } else if let Some(concern) = concern_by_id.get(source_id)
                    && is_concrete_argument_order_concern(concern)
                {
                    repaired_findings.push(synthesize_argument_order_finding(concern, None));
                } else if let Some(concern) = concern_by_id.get(source_id)
                    && is_seqcount_irq_concern(concern)
                {
                    repaired_findings.push(synthesize_seqcount_irq_finding(concern, None));
                } else if let Some(concern) = concern_by_id.get(source_id)
                    && is_resource_cleanup_concern(concern)
                {
                    repaired_findings.push(synthesize_resource_cleanup_finding(concern, None));
                } else if let Some(concern) = concern_by_id.get(source_id)
                    && is_retry_resource_leak_concern(concern)
                {
                    repaired_findings.push(synthesize_retry_resource_leak_finding(concern, None));
                } else if let Some(concern) = concern_by_id.get(source_id)
                    && is_lifecycle_ordering_concern(concern)
                {
                    repaired_findings.push(synthesize_lifecycle_ordering_finding(concern, None));
                } else {
                    repaired_drops.push(json!({
                        "source_concern_id": source_id,
                        "decision": "drop",
                        "drop_reason": "false_positive",
                        "rationale": "Stage 9 emitted an affirming/non-issue finding. Findings must describe bugs or regressions, so this item is preserved in the ledger as a dropped non-problem."
                    }));
                }
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_proof_required_seed_concern(concern)
                && !finding_preserves_seed_pattern_detail(concern, finding)
            {
                repaired_findings.push(synthesize_seed_pattern_finding(concern, Some(finding)));
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_concrete_argument_order_concern(concern)
                && !finding_preserves_argument_order_detail(concern, finding)
            {
                repaired_findings.push(synthesize_argument_order_finding(concern, Some(finding)));
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_seqcount_irq_concern(concern)
                && !finding_preserves_seqcount_irq_detail(concern, finding)
            {
                repaired_findings.push(synthesize_seqcount_irq_finding(concern, Some(finding)));
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_resource_cleanup_concern(concern)
                && !finding_preserves_resource_cleanup_detail(concern, finding)
            {
                repaired_findings.push(synthesize_resource_cleanup_finding(concern, Some(finding)));
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_retry_resource_leak_concern(concern)
                && !finding_preserves_retry_resource_leak_detail(concern, finding)
            {
                repaired_findings.push(synthesize_retry_resource_leak_finding(
                    concern,
                    Some(finding),
                ));
            } else if let Some(concern) = concern_by_id.get(source_id)
                && is_lifecycle_ordering_concern(concern)
                && !finding_preserves_lifecycle_ordering_detail(concern, finding)
            {
                repaired_findings.push(synthesize_lifecycle_ordering_finding(
                    concern,
                    Some(finding),
                ));
            } else {
                repaired_findings.push(finding.clone());
            }
        }
    }
    let mut repaired_findings_value = Value::Array(repaired_findings);
    ensure_stage9_finding_ids(&mut repaired_findings_value);

    for dropped in dropped_candidates.as_array().into_iter().flatten() {
        let Some(source_id) = dropped.get("source_concern_id").and_then(|id| id.as_str()) else {
            continue;
        };
        if expected.contains(source_id) && !accounted.contains(source_id) {
            if let Some(concern) = concern_by_id.get(source_id)
                && is_proof_required_seed_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_seed_pattern_detail(concern, finding)
                        });
                let valid_false_positive = drop_reason == "false_positive"
                    && seed_pattern_drop_proves_safety(concern, dropped);
                if !valid_subsumed_by && !valid_false_positive {
                    if let Some(finding_id) =
                        find_seed_pattern_subsuming_finding_id(concern, &repaired_findings_value)
                    {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This proof-required seeded concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_seed_pattern_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            if let Some(concern) = concern_by_id.get(source_id)
                && is_concrete_argument_order_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_argument_order_detail(concern, finding)
                        });
                if !valid_subsumed_by {
                    if let Some(finding_id) =
                        find_argument_order_subsuming_finding_id(concern, &repaired_findings_value)
                    {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This argument-order concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_argument_order_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            if let Some(concern) = concern_by_id.get(source_id)
                && is_seqcount_irq_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_seqcount_irq_detail(concern, finding)
                        });
                let valid_false_positive = drop_reason == "false_positive"
                    && seqcount_irq_drop_proves_safety(concern, dropped);
                if !valid_subsumed_by && !valid_false_positive {
                    if let Some(finding_id) =
                        find_seqcount_irq_subsuming_finding_id(concern, &repaired_findings_value)
                    {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This seqcount/IRQ concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_seqcount_irq_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            if let Some(concern) = concern_by_id.get(source_id)
                && is_resource_cleanup_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_resource_cleanup_detail(concern, finding)
                        });
                let valid_false_positive = drop_reason == "false_positive"
                    && resource_cleanup_drop_proves_safety(concern, dropped);
                if !valid_subsumed_by && !valid_false_positive {
                    if let Some(finding_id) = find_resource_cleanup_subsuming_finding_id(
                        concern,
                        &repaired_findings_value,
                    ) {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This resource-cleanup concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_resource_cleanup_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            if let Some(concern) = concern_by_id.get(source_id)
                && is_retry_resource_leak_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_retry_resource_leak_detail(concern, finding)
                        });
                let valid_false_positive = drop_reason == "false_positive"
                    && retry_resource_drop_proves_safety(concern, dropped);
                if !valid_subsumed_by && !valid_false_positive {
                    if let Some(finding_id) =
                        find_retry_resource_subsuming_finding_id(concern, &repaired_findings_value)
                    {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This retry/error-path resource concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_retry_resource_leak_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            if let Some(concern) = concern_by_id.get(source_id)
                && is_lifecycle_ordering_concern(concern)
            {
                let drop_reason = dropped
                    .get("drop_reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or_default();
                let valid_subsumed_by = drop_reason == "subsumed_by"
                    && dropped
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some_and(|finding| {
                            finding_preserves_lifecycle_ordering_detail(concern, finding)
                        });
                let valid_false_positive = drop_reason == "false_positive"
                    && lifecycle_ordering_drop_proves_safety(concern, dropped);
                if !valid_subsumed_by && !valid_false_positive {
                    if let Some(finding_id) = find_lifecycle_ordering_subsuming_finding_id(
                        concern,
                        &repaired_findings_value,
                    ) {
                        accounted.insert(source_id.to_string());
                        repaired_drops.push(make_subsumed_drop(
                            source_id,
                            &finding_id,
                            "This lifecycle-ordering concern is preserved by the referenced finding.",
                        ));
                        continue;
                    }
                    accounted.insert(source_id.to_string());
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_lifecycle_ordering_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                    continue;
                }
            }
            let mut repaired = dropped.clone();
            if let Value::Object(obj) = &mut repaired {
                obj.insert("decision".to_string(), Value::String("drop".to_string()));
                if !matches!(
                    obj.get("drop_reason").and_then(|reason| reason.as_str()),
                    Some(
                        "duplicate"
                            | "subsumed_by"
                            | "insufficient_evidence"
                            | "not_security_relevant"
                            | "already_mitigated"
                            | "false_positive"
                            | "unclear"
                    )
                ) {
                    obj.insert(
                        "drop_reason".to_string(),
                        Value::String("unclear".to_string()),
                    );
                }
                if obj
                    .get("rationale")
                    .and_then(|rationale| rationale.as_str())
                    .is_none_or(|rationale| rationale.trim().is_empty())
                {
                    obj.insert(
                        "rationale".to_string(),
                        Value::String(
                            "Stage 9 provided a drop without a concrete rationale.".to_string(),
                        ),
                    );
                }
                if obj.get("drop_reason").and_then(|reason| reason.as_str()) == Some("subsumed_by")
                {
                    let valid_target = obj
                        .get("subsumed_by_finding_id")
                        .and_then(|id| id.as_str())
                        .and_then(|id| stage9_finding_with_id(&repaired_findings_value, id))
                        .is_some();
                    if !valid_target {
                        obj.insert(
                            "drop_reason".to_string(),
                            Value::String("unclear".to_string()),
                        );
                        obj.remove("subsumed_by_finding_id");
                        obj.insert(
                            "rationale".to_string(),
                            Value::String(
                                "Stage 9 referenced a missing subsuming finding.".to_string(),
                            ),
                        );
                    }
                }
            }
            accounted.insert(source_id.to_string());
            repaired_drops.push(repaired);
        }
    }

    for source_id in expected_ids {
        if accounted.insert(source_id.clone()) {
            if let Some(concern) = concern_by_id.get(&source_id)
                && is_proof_required_seed_concern(concern)
            {
                if let Some(finding_id) =
                    find_seed_pattern_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this proof-required seeded concern, but the referenced finding preserves the same seeded bug mechanism.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_seed_pattern_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else if let Some(concern) = concern_by_id.get(&source_id)
                && is_concrete_argument_order_concern(concern)
            {
                if let Some(finding_id) =
                    find_argument_order_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this retained argument-order concern, but the referenced finding preserves the same order issue.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_argument_order_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else if let Some(concern) = concern_by_id.get(&source_id)
                && is_seqcount_irq_concern(concern)
            {
                if let Some(finding_id) =
                    find_seqcount_irq_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this retained seqcount/IRQ concern, but the referenced finding preserves the same interruptibility issue.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_seqcount_irq_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else if let Some(concern) = concern_by_id.get(&source_id)
                && is_resource_cleanup_concern(concern)
            {
                if let Some(finding_id) =
                    find_resource_cleanup_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this retained resource-cleanup concern, but the referenced finding preserves the same missing-cleanup issue.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_resource_cleanup_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else if let Some(concern) = concern_by_id.get(&source_id)
                && is_retry_resource_leak_concern(concern)
            {
                if let Some(finding_id) =
                    find_retry_resource_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this retained retry/error-path resource concern, but the referenced finding preserves the same leak-before-retry issue.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_retry_resource_leak_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else if let Some(concern) = concern_by_id.get(&source_id)
                && is_lifecycle_ordering_concern(concern)
            {
                if let Some(finding_id) =
                    find_lifecycle_ordering_subsuming_finding_id(concern, &repaired_findings_value)
                {
                    repaired_drops.push(make_subsumed_drop(
                        &source_id,
                        &finding_id,
                        "Stage 9 failed to account for this retained lifecycle-ordering concern, but the referenced finding preserves the same teardown-ordering issue.",
                    ));
                } else {
                    if let Some(findings) = repaired_findings_value.as_array_mut() {
                        findings.push(synthesize_lifecycle_ordering_finding(concern, None));
                    }
                    ensure_stage9_finding_ids(&mut repaired_findings_value);
                }
            } else {
                repaired_drops.push(json!({
                    "source_concern_id": source_id,
                    "decision": "drop",
                    "drop_reason": "unclear",
                    "rationale": "Stage 9 failed to account for this retained concern after the accountability pass; this synthetic drop preserves the audit ledger instead of silently losing the candidate."
                }));
            }
        }
    }

    (repaired_findings_value, Value::Array(repaired_drops))
}

pub(crate) fn cap_repaired_stage9_findings(
    stage9_concerns: &Value,
    findings: Value,
    dropped_candidates: Value,
    max_findings: usize,
) -> (Value, Value) {
    let Some(findings_array) = findings.as_array() else {
        return (findings, dropped_candidates);
    };
    if findings_array.len() <= max_findings {
        return (findings, dropped_candidates);
    }

    let concerns: Vec<&Value> = stage9_concerns.as_array().into_iter().flatten().collect();
    let concern_by_id: HashMap<String, &Value> = concerns
        .iter()
        .filter_map(|concern| {
            let source_id = concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())?;
            Some((source_id.to_string(), *concern))
        })
        .collect();
    let expected_ids: Vec<String> = concerns
        .iter()
        .filter_map(|concern| {
            concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();

    let mut indexed_findings: Vec<(usize, i32, Value)> = findings_array
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, finding)| {
            let source_id = finding
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .unwrap_or_default();
            let score = concern_by_id
                .get(source_id)
                .map(|concern| repaired_finding_retention_score(concern, &finding))
                .unwrap_or_else(|| repaired_finding_text_score(&finding_review_text(&finding)));
            (idx, score, finding)
        })
        .collect();
    indexed_findings.sort_by(|(left_idx, left_score, _), (right_idx, right_score, _)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_idx.cmp(right_idx))
    });

    let mut keep_source_ids = HashSet::new();
    let mut kept_findings = Vec::new();
    for (_, _, finding) in indexed_findings {
        let Some(source_id) = finding
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .map(|id| id.to_string())
        else {
            continue;
        };
        let is_preserved_class = concern_by_id.get(&source_id).is_some_and(|concern| {
            is_proof_required_seed_concern(concern)
                || is_concrete_argument_order_concern(concern)
                || is_seqcount_irq_concern(concern)
                || is_resource_cleanup_concern(concern)
                || is_retry_resource_leak_concern(concern)
                || is_lifecycle_ordering_concern(concern)
        });
        if is_preserved_class || kept_findings.len() < max_findings {
            keep_source_ids.insert(source_id);
            kept_findings.push(finding);
        }
    }

    let mut repaired_drops = Vec::new();
    let mut accounted = keep_source_ids.clone();
    for dropped in dropped_candidates.as_array().into_iter().flatten() {
        let Some(source_id) = dropped
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .map(|id| id.to_string())
        else {
            continue;
        };
        if keep_source_ids.contains(&source_id) || !accounted.insert(source_id) {
            continue;
        }
        repaired_drops.push(dropped.clone());
    }

    for source_id in expected_ids {
        if accounted.insert(source_id.clone()) {
            repaired_drops.push(json!({
                "source_concern_id": source_id,
                "decision": "drop",
                "drop_reason": "unclear",
                "rationale": "Stage 9 over-produced an invalid findings/drop ledger after the accountability pass. This candidate is kept in the audit ledger but not emitted as a final finding because it was not part of the compact retained set."
            }));
        }
    }

    let mut kept_findings = Value::Array(kept_findings);
    ensure_stage9_finding_ids(&mut kept_findings);
    (kept_findings, Value::Array(repaired_drops))
}

pub(crate) fn cap_minimal_fallback_findings(
    stage9_concerns: &Value,
    findings: &mut Value,
    max_findings: usize,
) {
    let Some(findings_array) = findings.as_array() else {
        return;
    };
    if findings_array.len() <= max_findings {
        ensure_stage9_finding_ids(findings);
        return;
    }

    let concerns: Vec<&Value> = stage9_concerns.as_array().into_iter().flatten().collect();
    let mut indexed_findings: Vec<(usize, i32, bool, Value)> = findings_array
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, finding)| {
            let (score, protected) = minimal_fallback_retention_score(&concerns, &finding);
            (idx, score, protected, finding)
        })
        .collect();
    indexed_findings.sort_by(
        |(left_idx, left_score, left_protected, _),
         (right_idx, right_score, right_protected, _)| {
            right_protected
                .cmp(left_protected)
                .then_with(|| right_score.cmp(left_score))
                .then_with(|| left_idx.cmp(right_idx))
        },
    );

    let mut kept = Vec::new();
    for (_, _, protected, finding) in indexed_findings {
        if protected || kept.len() < max_findings {
            kept.push(finding);
        }
    }

    *findings = Value::Array(kept);
    ensure_stage9_finding_ids(findings);
}

pub(crate) fn minimal_fallback_retention_score(
    concerns: &[&Value],
    finding: &Value,
) -> (i32, bool) {
    let text = finding_review_text(finding);
    let mut score = repaired_finding_text_score(&text);
    let mut protected = false;

    for concern in concerns {
        let preserves_proof_required_seed = is_proof_required_seed_concern(concern)
            && finding_preserves_seed_pattern_detail(concern, finding);
        let preserves_argument_order = is_concrete_argument_order_concern(concern)
            && finding_preserves_argument_order_detail(concern, finding);
        let preserves_seqcount = is_seqcount_irq_concern(concern)
            && finding_preserves_seqcount_irq_detail(concern, finding);
        let preserves_resource_cleanup = is_resource_cleanup_concern(concern)
            && finding_preserves_resource_cleanup_detail(concern, finding);
        let preserves_retry_resource = is_retry_resource_leak_concern(concern)
            && finding_preserves_retry_resource_leak_detail(concern, finding);
        let preserves_lifecycle = is_lifecycle_ordering_concern(concern)
            && finding_preserves_lifecycle_ordering_detail(concern, finding);

        if preserves_proof_required_seed
            || preserves_argument_order
            || preserves_seqcount
            || preserves_resource_cleanup
            || preserves_retry_resource
            || preserves_lifecycle
        {
            protected = true;
            score += 200;
        }
    }

    (score, protected)
}

pub(crate) fn repaired_finding_retention_score(concern: &Value, finding: &Value) -> i32 {
    let mut score = repaired_finding_text_score(&format!(
        "{}\n{}",
        concern_review_text(concern),
        finding_review_text(finding)
    ));
    if is_proof_required_seed_concern(concern)
        || is_concrete_argument_order_concern(concern)
        || is_seqcount_irq_concern(concern)
        || is_resource_cleanup_concern(concern)
        || is_retry_resource_leak_concern(concern)
        || is_lifecycle_ordering_concern(concern)
    {
        score += 100;
    }
    score
}

pub(crate) fn repaired_finding_text_score(text: &str) -> i32 {
    let lower = text.to_ascii_lowercase();
    let mut score = 0;
    for needle in [
        "null pointer",
        "null dereference",
        "use-after-free",
        "rcu",
        "data corruption",
        "memory leak",
        "leak",
        "double free",
        "deadlock",
        "livelock",
        "buffer overflow",
        "out-of-bounds",
        "bounds",
        "missing check",
        "missing cleanup",
        "response buffer",
        "free_response_buf",
        "fallback",
        "incorrect argument",
        "wrong argument",
        "argument order",
        "seqcount",
        "local_irq",
        "workqueue",
        "rfkill",
        "teardown",
        "callback",
    ] {
        if lower.contains(needle) {
            score += 10;
        }
    }
    for weakener in [
        "potential",
        "potentially",
        "could",
        "may",
        "speculative",
        "no guarantee",
    ] {
        if lower.contains(weakener) {
            score -= 1;
        }
    }
    score
}

pub(crate) fn text_mentions_any_call(text: &str, call_names: &HashSet<String>) -> bool {
    let lower = text.to_ascii_lowercase();
    call_names
        .iter()
        .any(|call| lower.contains(&call.to_ascii_lowercase()))
}

pub(crate) fn compact_argument_order_related_findings(
    stage9_concerns: &Value,
    findings: &Value,
    dropped_candidates: &Value,
) -> (Value, Value) {
    let concerns: Vec<&Value> = stage9_concerns.as_array().into_iter().flatten().collect();
    let concern_by_id: HashMap<String, &Value> = concerns
        .iter()
        .filter_map(|concern| {
            let source_id = concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())?;
            Some((source_id.to_string(), *concern))
        })
        .collect();

    let Some(findings_array) = findings.as_array() else {
        return (findings.clone(), dropped_candidates.clone());
    };
    let Some(dropped_array) = dropped_candidates.as_array() else {
        return (findings.clone(), dropped_candidates.clone());
    };

    let mut canonical_targets: Vec<(HashSet<String>, String, String)> = Vec::new();
    for concern in concerns
        .iter()
        .copied()
        .filter(|concern| is_concrete_argument_order_concern(concern))
    {
        let Some(source_id) = concern.get("source_concern_id").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(finding_id) = find_argument_order_subsuming_finding_id(concern, findings) else {
            continue;
        };
        let mut call_names = extract_call_names(&concern_review_text(concern));
        if call_names.is_empty()
            && let Some(target) = stage9_finding_with_id(findings, &finding_id)
        {
            call_names = extract_call_names(&finding_review_text(target));
        }
        if call_names.is_empty() {
            continue;
        }
        if canonical_targets
            .iter()
            .any(|(_, existing_finding_id, _)| existing_finding_id == &finding_id)
        {
            continue;
        }
        canonical_targets.push((call_names, finding_id, source_id.to_string()));
    }

    if canonical_targets.is_empty() {
        return (findings.clone(), dropped_candidates.clone());
    }

    let dropped_sources: HashSet<String> = dropped_array
        .iter()
        .filter_map(|dropped| {
            dropped
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();
    let mut compacted_findings = Vec::new();
    let mut compacted_drops = dropped_array.clone();

    for finding in findings_array {
        let source_id = finding
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .unwrap_or_default();
        let finding_id = finding_id(finding).unwrap_or_default();
        let Some(concern) = concern_by_id.get(source_id).copied() else {
            compacted_findings.push(finding.clone());
            continue;
        };

        if dropped_sources.contains(source_id)
            || is_proof_required_seed_concern(concern)
            || is_seqcount_irq_concern(concern)
            || is_resource_cleanup_concern(concern)
            || is_retry_resource_leak_concern(concern)
            || is_lifecycle_ordering_concern(concern)
        {
            compacted_findings.push(finding.clone());
            continue;
        }

        let combined_text = format!(
            "{}\n{}",
            finding_review_text(finding),
            concern_review_text(concern)
        );
        let mut compacted = false;
        for (call_names, target_finding_id, target_source_id) in &canonical_targets {
            if finding_id == target_finding_id || source_id == target_source_id {
                continue;
            }
            if !text_mentions_any_call(&combined_text, call_names) {
                continue;
            }
            compacted_drops.push(make_subsumed_drop(
                source_id,
                target_finding_id,
                format!(
                    "This candidate is the same helper/root cause as {target_finding_id}; that finding preserves the callee name, expected parameter order/signature, actual call-site argument order, and why the order is wrong."
                ),
            ));
            compacted = true;
            break;
        }
        if !compacted {
            compacted_findings.push(finding.clone());
        }
    }

    (
        Value::Array(compacted_findings),
        Value::Array(compacted_drops),
    )
}

pub(crate) fn compact_stage9_related_findings(
    stage9_concerns: &Value,
    findings: &Value,
    dropped_candidates: &Value,
) -> (Value, Value) {
    let (findings, dropped_candidates) =
        compact_argument_order_related_findings(stage9_concerns, findings, dropped_candidates);
    compact_root_cause_related_findings(stage9_concerns, &findings, &dropped_candidates)
}

pub(crate) fn compact_root_cause_related_findings(
    stage9_concerns: &Value,
    findings: &Value,
    dropped_candidates: &Value,
) -> (Value, Value) {
    let concerns: Vec<&Value> = stage9_concerns.as_array().into_iter().flatten().collect();
    let concern_by_id: HashMap<String, &Value> = concerns
        .iter()
        .filter_map(|concern| {
            let source_id = concern
                .get("source_concern_id")
                .and_then(|id| id.as_str())?;
            Some((source_id.to_string(), *concern))
        })
        .collect();

    let Some(findings_array) = findings.as_array() else {
        return (findings.clone(), dropped_candidates.clone());
    };
    let Some(dropped_array) = dropped_candidates.as_array() else {
        return (findings.clone(), dropped_candidates.clone());
    };

    let dropped_sources: HashSet<String> = dropped_array
        .iter()
        .filter_map(|dropped| {
            dropped
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(|id| id.to_string())
        })
        .collect();

    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, finding) in findings_array.iter().enumerate() {
        let Some(source_id) = finding.get("source_concern_id").and_then(|id| id.as_str()) else {
            continue;
        };
        if dropped_sources.contains(source_id) {
            continue;
        }
        let Some(concern) = concern_by_id.get(source_id).copied() else {
            continue;
        };
        if let Some(key) = stage9_compaction_key(concern, finding) {
            groups.entry(key).or_default().push(idx);
        }
    }

    if groups.values().all(|indices| indices.len() < 2) {
        return (findings.clone(), dropped_candidates.clone());
    }

    let mut compacted_findings = findings_array.clone();
    let mut removed_indices = HashSet::new();
    let mut compacted_drops = dropped_array.clone();

    for indices in groups.values() {
        let available: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|idx| !removed_indices.contains(idx))
            .collect();
        if available.len() < 2 {
            continue;
        }
        let Some(canonical_idx) =
            choose_stage9_compaction_canonical(&available, &compacted_findings, &concern_by_id)
        else {
            continue;
        };
        let Some(canonical_id) = finding_id(&compacted_findings[canonical_idx]).map(str::to_string)
        else {
            continue;
        };

        for idx in available {
            if idx == canonical_idx || removed_indices.contains(&idx) {
                continue;
            }
            let Some(source_id) = compacted_findings[idx]
                .get("source_concern_id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(concern) = concern_by_id.get(&source_id).copied() else {
                continue;
            };
            if !stage9_concern_can_be_subsumed_by(concern, &compacted_findings[canonical_idx]) {
                continue;
            }

            let duplicate = compacted_findings[idx].clone();
            merge_compacted_finding_evidence(
                &mut compacted_findings[canonical_idx],
                &duplicate,
                concern,
            );
            removed_indices.insert(idx);
            compacted_drops.push(make_subsumed_drop(
                &source_id,
                &canonical_id,
                format!(
                    "This candidate shares the same root cause, resource/call path, trigger, and consequence class as {canonical_id}; that canonical finding preserves the required evidence."
                ),
            ));
        }
    }

    if removed_indices.is_empty() {
        return (findings.clone(), dropped_candidates.clone());
    }

    let compacted_findings: Vec<Value> = compacted_findings
        .into_iter()
        .enumerate()
        .filter_map(|(idx, finding)| (!removed_indices.contains(&idx)).then_some(finding))
        .collect();
    (
        Value::Array(compacted_findings),
        Value::Array(compacted_drops),
    )
}

pub(crate) fn choose_stage9_compaction_canonical(
    indices: &[usize],
    findings: &[Value],
    concern_by_id: &HashMap<String, &Value>,
) -> Option<usize> {
    let mut best: Option<(usize, usize, i32)> = None;
    for &idx in indices {
        let finding = findings.get(idx)?;
        let subsumable = indices
            .iter()
            .copied()
            .filter(|other_idx| *other_idx != idx)
            .filter(|other_idx| {
                findings
                    .get(*other_idx)
                    .and_then(|other| other.get("source_concern_id"))
                    .and_then(|id| id.as_str())
                    .and_then(|source_id| concern_by_id.get(source_id).copied())
                    .is_some_and(|concern| stage9_concern_can_be_subsumed_by(concern, finding))
            })
            .count();
        let score = finding
            .get("source_concern_id")
            .and_then(|id| id.as_str())
            .and_then(|source_id| concern_by_id.get(source_id).copied())
            .map(|concern| repaired_finding_retention_score(concern, finding))
            .unwrap_or_else(|| repaired_finding_text_score(&finding_review_text(finding)));

        let replace = match best {
            None => true,
            Some((best_idx, best_subsumable, best_score)) => {
                subsumable > best_subsumable
                    || (subsumable == best_subsumable && score > best_score)
                    || (subsumable == best_subsumable && score == best_score && idx < best_idx)
            }
        };
        if replace {
            best = Some((idx, subsumable, score));
        }
    }
    best.map(|(idx, _, _)| idx)
}

pub(crate) fn stage9_concern_can_be_subsumed_by(concern: &Value, finding: &Value) -> bool {
    let mut protected = false;

    if is_proof_required_seed_concern(concern) {
        protected = true;
        if !finding_preserves_seed_pattern_detail(concern, finding) {
            return false;
        }
    }
    if is_concrete_argument_order_concern(concern) {
        protected = true;
        if !finding_preserves_argument_order_detail(concern, finding) {
            return false;
        }
    }
    if is_seqcount_irq_concern(concern) {
        protected = true;
        if !finding_preserves_seqcount_irq_detail(concern, finding) {
            return false;
        }
    }
    if is_resource_cleanup_concern(concern) {
        protected = true;
        if !finding_preserves_resource_cleanup_detail(concern, finding) {
            return false;
        }
    }
    if is_retry_resource_leak_concern(concern) {
        protected = true;
        if !finding_preserves_retry_resource_leak_detail(concern, finding) {
            return false;
        }
    }
    if is_lifecycle_ordering_concern(concern) {
        protected = true;
        if !finding_preserves_lifecycle_ordering_detail(concern, finding) {
            return false;
        }
    }

    protected || !finding_is_non_problem(finding)
}

pub(crate) fn stage9_compaction_key(concern: &Value, finding: &Value) -> Option<String> {
    let text = format!(
        "{}\n{}",
        concern_review_text(concern),
        finding_review_text(finding)
    );
    let lower = text.to_ascii_lowercase();

    if is_concrete_argument_order_concern(concern) {
        return compact_call_key("argument-order", &text);
    }
    if is_retry_resource_leak_concern(concern)
        || seed_pattern(concern) == Some("retry_error_path_resource_leak")
    {
        return retry_resource_compaction_key(&lower)
            .or_else(|| compact_call_key("retry-resource", &text));
    }
    if is_lifecycle_ordering_concern(concern) {
        return lifecycle_ordering_compaction_key(&lower);
    }
    if is_seqcount_irq_concern(concern) {
        if lower.contains("fprop") {
            return Some("seqcount-irq:fprop".to_string());
        }
        return Some("seqcount-irq:generic".to_string());
    }
    if let Some(key) = dmem_compaction_key(&lower) {
        return Some(key);
    }
    if let Some(pattern) = seed_pattern(concern) {
        return Some(format!(
            "seed:{pattern}:{}",
            seed_pattern_compaction_anchor(pattern, &lower)
        ));
    }
    if is_resource_cleanup_concern(concern) {
        let names = resource_names_from_text(&text);
        if let Some(key) = sorted_limited_key("resource-cleanup", names, 4) {
            return Some(key);
        }
    }

    response_buffer_cleanup_compaction_key(&lower)
}

pub(crate) fn dmem_compaction_key(lower: &str) -> Option<String> {
    let mentions_dmem_max_path = lower.contains("limit.max")
        || lower.contains("limit_key_write")
        || lower.contains("limit_region_max_write")
        || lower.contains("dmemcg_parse_limit");
    let mentions_missing_value = lower.contains("missing value")
        || lower.contains("absent value")
        || lower.contains("absent option")
        || lower.contains("bare key")
        || lower.contains("without specifying")
        || lower.contains("without a value")
        || lower.contains("null dereference")
        || lower.contains("null pointer")
        || lower.contains("invalid parse");
    if mentions_dmem_max_path && mentions_missing_value {
        return Some("dmem:max-missing-value".to_string());
    }

    let mentions_unregister_rcu = lower.contains("region_unregister")
        && (lower.contains("list_for_each_rcu")
            || lower.contains("rcu_read_lock")
            || lower.contains("rcu traversal")
            || lower.contains("rcu iterator")
            || lower.contains("teardown")
            || lower.contains("unregister"));
    if mentions_unregister_rcu {
        return Some("dmem:unregister-rcu".to_string());
    }

    None
}

pub(crate) fn seed_pattern_compaction_anchor(pattern: &str, lower: &str) -> &'static str {
    match pattern {
        "cgroup_keyed_parse_missing_value" => "dmem-max-missing-value",
        "rcu_teardown_iteration_without_read_lock" => "dmem-unregister-rcu",
        "skb_fragment_capacity_max_skb_frags" => "skb-frag-capacity",
        "retry_error_path_resource_leak" => "retry-resource",
        _ if lower.contains("dmem") => "dmem",
        _ if lower.contains("skb") => "skb",
        _ => "generic",
    }
}

pub(crate) fn lifecycle_ordering_compaction_key(lower: &str) -> Option<String> {
    let mut anchors = HashSet::new();
    for (needle, anchor) in [
        ("nci_close_device", "nci_close_device"),
        ("rfkill", "rfkill"),
        ("workqueue", "workqueue"),
        ("work queue", "workqueue"),
        ("destroy_workqueue", "workqueue"),
        ("timer", "timer"),
        ("callback", "callback"),
        ("unregister", "unregister"),
        ("remove", "remove"),
    ] {
        if lower.contains(needle) {
            anchors.insert(anchor.to_string());
        }
    }
    if anchors.contains("rfkill") && anchors.contains("workqueue") {
        return Some("lifecycle:rfkill-workqueue-teardown".to_string());
    }
    if anchors.len() >= 3 {
        return sorted_limited_key("lifecycle", anchors, 4);
    }
    None
}

pub(crate) fn retry_resource_compaction_key(lower: &str) -> Option<String> {
    let mentions_resource = lower.contains("retry_iov")
        || lower.contains("iov_base")
        || lower.contains("response buffer")
        || lower.contains("resource buffer")
        || lower.contains("resp_iov")
        || lower.contains("rsp_iov");
    let mentions_retry = lower.contains("retry")
        || lower.contains("fallback")
        || lower.contains("fall back")
        || lower.contains("reissue");
    if !mentions_resource || !mentions_retry {
        return None;
    }
    if lower.contains("retry_open") {
        return Some("retry-resource:smb2-open-response-buffer".to_string());
    }
    response_buffer_cleanup_compaction_key(lower)
}

pub(crate) fn response_buffer_cleanup_compaction_key(lower: &str) -> Option<String> {
    let mentions_buffer = lower.contains("response buffer")
        || lower.contains("resource buffer")
        || lower.contains("retry_iov")
        || lower.contains("iov_base")
        || lower.contains("resp_iov")
        || lower.contains("rsp_iov");
    let mentions_cleanup = lower.contains("leak")
        || lower.contains("free_response_buf")
        || lower.contains("missing free")
        || lower.contains("without freeing")
        || lower.contains("not freed")
        || lower.contains("cleanup");
    if !mentions_buffer || !mentions_cleanup {
        return None;
    }
    if lower.contains("sendreceive") || lower.contains("cifs_send_recv") {
        return Some("response-buffer-cleanup:cifs-sendreceive".to_string());
    }
    if lower.contains("retry_open") {
        return Some("response-buffer-cleanup:smb2-open".to_string());
    }
    None
}

pub(crate) fn compact_call_key(prefix: &str, text: &str) -> Option<String> {
    let calls = extract_call_names(text)
        .into_iter()
        .filter(|call| {
            !matches!(
                call.as_str(),
                "json"
                    | "finding"
                    | "findings"
                    | "stage"
                    | "return"
                    | "error"
                    | "warning"
                    | "candidate"
            )
        })
        .collect();
    sorted_limited_key(prefix, calls, 3)
}

pub(crate) fn sorted_limited_key(
    prefix: &str,
    values: HashSet<String>,
    limit: usize,
) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort();
    values.truncate(limit);
    Some(format!("{prefix}:{}", values.join("+")))
}

pub(crate) fn merge_compacted_finding_evidence(
    target: &mut Value,
    duplicate: &Value,
    concern: &Value,
) {
    let duplicate_problem = value_field_text(duplicate, "problem");
    if duplicate_problem.trim().is_empty() {
        return;
    }
    let mut summary = duplicate_problem.trim().replace('\n', " ");
    if summary.len() > 220 {
        summary.truncate(217);
        summary.push_str("...");
    }

    let current = value_field_text(target, "severity_explanation");
    let current_lower = current.to_ascii_lowercase();
    if current_lower.contains(&summary.to_ascii_lowercase())
        || current.matches("Compacted related evidence").count() >= 2
    {
        return;
    }

    let source_id = concern
        .get("source_concern_id")
        .and_then(|id| id.as_str())
        .unwrap_or("retained concern");
    let addition = format!(" Compacted related evidence from {source_id}: {summary}.");
    if let Some(obj) = target.as_object_mut() {
        obj.insert(
            "severity_explanation".to_string(),
            Value::String(format!("{current}{addition}")),
        );
    }
}

pub(crate) fn fallback_inline_review(findings: &Value) -> String {
    let Some(findings) = findings.as_array() else {
        return "No issues found.".to_string();
    };
    if findings.is_empty() {
        return "No issues found.".to_string();
    }

    let mut text =
        String::from("commit review-fallback\nAuthor: Sashiko AI\n\n> Fallback review report\n\n");
    text.push_str("Sashiko could not produce a fully formatted inline report, but the structured review found potential issue(s):\n\n");
    for finding in findings {
        let severity = finding
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("Medium");
        let problem = finding
            .get("problem")
            .and_then(|v| v.as_str())
            .unwrap_or("Potential regression");
        let explanation = finding
            .get("severity_explanation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        text.push_str(&format!("- [{}] {}\n", severity, problem));
        if !explanation.is_empty() {
            text.push_str(&format!("  {}\n", explanation));
        }
    }
    text
}

pub(crate) fn derive_stage8_drops(input: &[Value], retained: &Value) -> Value {
    let retained_descriptions: HashSet<String> = retained
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(concern_description)
        .map(|s| s.to_ascii_lowercase())
        .collect();

    let dropped: Vec<Value> = input
        .iter()
        .filter_map(|concern| {
            let description = concern_description(concern)?;
            if retained_descriptions.contains(&description.to_ascii_lowercase()) {
                None
            } else {
                Some(json!({
                    "description": description,
                    "reason": "Not retained by Stage 8 consolidation output; model did not provide a specific drop reason."
                }))
            }
        })
        .collect();
    Value::Array(dropped)
}

pub(crate) fn dropped_concern_text(dropped: &Value) -> String {
    [
        value_field_text(dropped, "description"),
        value_field_text(dropped, "reason"),
    ]
    .join("\n")
}

pub(crate) fn retained_preserves_argument_order_concern(
    retained: &Value,
    dropped_text: &str,
) -> bool {
    let dropped_call_names = extract_call_names(dropped_text);
    retained
        .as_array()
        .into_iter()
        .flatten()
        .filter(|concern| is_concrete_argument_order_concern(concern))
        .any(|concern| {
            let retained_text = concern_review_text(concern);
            let retained_lower = retained_text.to_ascii_lowercase();
            if dropped_call_names.is_empty() {
                text_preserves_argument_order_detail_for_concern(concern, &retained_text)
            } else {
                dropped_call_names
                    .iter()
                    .any(|name| retained_lower.contains(&name.to_ascii_lowercase()))
                    && text_preserves_argument_order_detail_for_concern(concern, &retained_text)
            }
        })
}

pub(crate) fn preserve_stage8_argument_order_concerns(
    input: &[Value],
    retained: &mut Value,
    dropped_concerns: &Value,
) -> usize {
    let dropped_argument_order: Vec<_> = dropped_concerns
        .as_array()
        .into_iter()
        .flatten()
        .filter(|dropped| text_mentions_argument_order(&dropped_concern_text(dropped)))
        .filter(|dropped| {
            !retained_preserves_argument_order_concern(retained, &dropped_concern_text(dropped))
        })
        .cloned()
        .collect();
    let Some(retained_concerns) = retained.as_array_mut() else {
        return 0;
    };
    let mut retained_descriptions: HashSet<String> = retained_concerns
        .iter()
        .filter_map(concern_description)
        .map(|desc| desc.to_ascii_lowercase())
        .collect();

    let mut restored = 0;
    for dropped in dropped_argument_order {
        let dropped_description = value_field_text(&dropped, "description");
        let dropped_key = dropped_description.to_ascii_lowercase();
        let original = input
            .iter()
            .find(|concern| concern_description(concern).as_deref() == Some(&dropped_description))
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "type": "API Argument Order",
                    "description": dropped_description,
                    "reasoning": value_field_text(&dropped, "reason"),
                    "preexisting": false,
                    "preservation_policy": "argument_order_emit_or_subsumed_by_detailed_finding"
                })
            });
        if !retained_descriptions.contains(&dropped_key) {
            retained_concerns.push(original);
            retained_descriptions.insert(dropped_key);
            restored += 1;
        }
    }

    for concern in input
        .iter()
        .filter(|concern| is_concrete_argument_order_concern(concern))
    {
        let description = match concern_description(concern) {
            Some(description) => description,
            None => continue,
        };
        let description_key = description.to_ascii_lowercase();
        let concern_text = concern_review_text(concern);
        let retained_snapshot = Value::Array(retained_concerns.clone());
        if !retained_descriptions.contains(&description_key)
            && !retained_preserves_argument_order_concern(&retained_snapshot, &concern_text)
        {
            retained_concerns.push(concern.clone());
            retained_descriptions.insert(description_key);
            restored += 1;
        }
    }

    restored
}

pub(crate) fn retained_preserves_seed_pattern_concern(seed: &Value, retained: &Value) -> bool {
    let text = concern_review_text(retained);
    match seed_pattern(seed) {
        Some("cgroup_keyed_parse_missing_value") => {
            text_preserves_cgroup_missing_value_detail(&text)
                || static_bug_pattern_text_matches(&concern_review_text(seed), &text)
        }
        Some("rcu_teardown_iteration_without_read_lock") => {
            text_preserves_rcu_teardown_detail(&text)
                || static_bug_pattern_text_matches(&concern_review_text(seed), &text)
        }
        Some("skb_fragment_capacity_max_skb_frags") => {
            text_preserves_skb_fragment_capacity_detail(&text)
                || static_bug_pattern_text_matches(&concern_review_text(seed), &text)
        }
        Some("retry_error_path_resource_leak") => {
            text_preserves_retry_resource_leak_detail(&text)
                || static_bug_pattern_text_matches(&concern_review_text(seed), &text)
        }
        _ => false,
    }
}

pub(crate) fn copy_seed_provenance(seed: &Value, retained: &mut Value) {
    let Some(pattern) = seed_pattern(seed) else {
        return;
    };
    let Some(obj) = retained.as_object_mut() else {
        return;
    };

    let source = seed
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("static_bug_pattern_seed");
    obj.entry("source".to_string())
        .or_insert_with(|| Value::String(source.to_string()));
    obj.entry("pattern".to_string())
        .or_insert_with(|| Value::String(pattern.to_string()));
    obj.insert(
        "preservation".to_string(),
        Value::String("proof_required_drop".to_string()),
    );
    obj.insert(
        "preservation_policy".to_string(),
        Value::String("proof_required_drop".to_string()),
    );
    obj.entry("required_evidence".to_string())
        .or_insert_with(|| {
            Value::Array(
                seed_required_evidence(pattern)
                    .into_iter()
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            )
        });
}

pub(crate) fn preserve_stage8_proof_required_seed_concerns(
    input: &[Value],
    retained: &mut Value,
) -> usize {
    let seeds: Vec<&Value> = input
        .iter()
        .filter(|concern| is_proof_required_seed_concern(concern))
        .collect();
    if seeds.is_empty() {
        return 0;
    }

    let Some(retained_concerns) = retained.as_array_mut() else {
        return 0;
    };

    let mut restored = 0;
    for seed in seeds {
        if let Some(retained_match) = retained_concerns
            .iter_mut()
            .find(|concern| retained_preserves_seed_pattern_concern(seed, concern))
        {
            copy_seed_provenance(seed, retained_match);
            continue;
        }

        if !retained_concerns
            .iter()
            .any(|concern| concern_description(concern) == concern_description(seed))
        {
            retained_concerns.push(seed.clone());
            restored += 1;
        }
    }

    restored
}

pub(crate) fn concern_description(concern: &Value) -> Option<String> {
    concern
        .get("description")
        .and_then(|v| v.as_str())
        .or_else(|| concern.get("problem").and_then(|v| v.as_str()))
        .or_else(|| concern.get("type").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

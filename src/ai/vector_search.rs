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

//! Fast, localized vector-space comparison and deduplication for pre-existing bugs.
//!
//! Extracts weighted sparse feature vectors from bug descriptions, affected source
//! files, directory hierarchies, subsystem identifiers, and code symbols to perform
//! sub-millisecond candidate retrieval before LLM verification.

use crate::db::PreexistingBug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Minimum similarity threshold for candidate match consideration.
pub const DEFAULT_SIMILARITY_THRESHOLD: f32 = 0.15;

/// Default top N candidates to retrieve for LLM deduplication.
pub const DEFAULT_TOP_CANDIDATES: usize = 20;

/// Normalized sparse vector representation of a bug's semantic and localized features.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BugVector {
    /// Normalized feature weights mapped by token.
    pub features: HashMap<String, f32>,
}

impl BugVector {
    /// Creates a new empty bug vector.
    pub fn new() -> Self {
        Self {
            features: HashMap::new(),
        }
    }

    /// Serializes the vector to a JSON string for database persistence.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.features).unwrap_or_else(|_| "{}".to_string())
    }

    /// Deserializes the vector from a JSON string.
    pub fn from_json(json_str: &str) -> Option<Self> {
        let features: HashMap<String, f32> = serde_json::from_str(json_str).ok()?;
        Some(Self { features })
    }

    /// Computes the cosine similarity between two normalized bug vectors.
    ///
    /// Since the vectors are pre-normalized to unit length (L2 norm = 1),
    /// cosine similarity is simply the dot product of common features.
    pub fn cosine_similarity(&self, other: &BugVector) -> f32 {
        let (smaller, larger) = if self.features.len() < other.features.len() {
            (&self.features, &other.features)
        } else {
            (&other.features, &self.features)
        };

        let mut dot_product = 0.0f32;
        for (term, weight) in smaller {
            if let Some(other_weight) = larger.get(term) {
                dot_product += weight * other_weight;
            }
        }

        dot_product.clamp(0.0, 1.0)
    }
}

impl Default for BugVector {
    fn default() -> Self {
        Self::new()
    }
}

/// A ranked candidate match from vector similarity search.
#[derive(Debug, Clone)]
pub struct CandidateMatch {
    /// The matched known pre-existing bug.
    pub bug: PreexistingBug,
    /// The computed similarity score (0.0 to 1.0).
    pub similarity: f32,
}

/// Generates a normalized sparse feature vector for a bug given its problem description,
/// locations, affected source files, and subsystem.
pub fn extract_bug_vector(
    problem: &str,
    subsystem: Option<&str>,
    source_files: &[String],
    locations: Option<&serde_json::Value>,
) -> BugVector {
    let mut raw_weights: HashMap<String, f32> = HashMap::new();

    // 1. Problem description tokens and error keywords (Weight: 1.0 - 2.5)
    for token in tokenize_text(problem) {
        let weight = get_keyword_weight(&token);
        *raw_weights.entry(format!("tok:{}", token)).or_insert(0.0) += weight;
    }

    // 2. Subsystem features (Weight: 4.0)
    if let Some(sub) = subsystem {
        let cleaned = sub.trim().to_lowercase();
        if !cleaned.is_empty() {
            *raw_weights.entry(format!("sub:{}", cleaned)).or_insert(0.0) += 4.0;
        }
    }

    // 3. Source files and directory hierarchies (Weight: 2.0 - 6.0)
    for file_path in source_files {
        add_file_path_features(&mut raw_weights, file_path);
    }

    // 4. Locations JSON (functions, symbols, files) (Weight: 3.0 - 5.0)
    if let Some(locs) = locations
        && let Some(arr) = locs.as_array()
    {
        for loc in arr {
            if let Some(file) = loc.get("file").and_then(|v| v.as_str()) {
                add_file_path_features(&mut raw_weights, file);
            }
            if let Some(symbol) = loc.get("function_or_symbol").and_then(|v| v.as_str()) {
                let sym_clean = symbol.trim().to_lowercase();
                if !sym_clean.is_empty() {
                    *raw_weights
                        .entry(format!("sym:{}", sym_clean))
                        .or_insert(0.0) += 5.0;
                    for part in tokenize_identifier(&sym_clean) {
                        *raw_weights
                            .entry(format!("sym_part:{}", part))
                            .or_insert(0.0) += 2.0;
                    }
                }
            }
        }
    }

    // L2 Normalize the vector
    let l2_norm: f32 = raw_weights.values().map(|w| w * w).sum::<f32>().sqrt();

    let mut normalized_features = HashMap::new();
    if l2_norm > 0.0 {
        for (term, weight) in raw_weights {
            normalized_features.insert(term, weight / l2_norm);
        }
    }

    BugVector {
        features: normalized_features,
    }
}

/// Adds hierarchical path components to feature weights.
fn add_file_path_features(weights: &mut HashMap<String, f32>, file_path: &str) {
    let clean_path = file_path.trim().trim_start_matches("./").to_lowercase();
    if clean_path.is_empty() {
        return;
    }

    // Full exact path (highest localization weight)
    *weights.entry(format!("file:{}", clean_path)).or_insert(0.0) += 6.0;

    let parts: Vec<&str> = clean_path.split('/').collect();
    if let Some(&filename) = parts.last() {
        *weights.entry(format!("fname:{}", filename)).or_insert(0.0) += 4.0;
    }

    // Hierarchical directory prefixes (e.g. drivers/net/ethernet/intel/)
    let mut current_dir = String::new();
    for (i, &part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            break; // Skip file name itself
        }
        if !current_dir.is_empty() {
            current_dir.push('/');
        }
        current_dir.push_str(part);
        *weights.entry(format!("dir:{}", current_dir)).or_insert(0.0) += 2.5;
    }
}

/// Tokenizes free-form text into normalized words and identifiers.
fn tokenize_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = raw.trim().to_lowercase();
        if trimmed.len() >= 2 && !is_stop_word(&trimmed) {
            tokens.push(trimmed.clone());
            tokens.extend(tokenize_identifier(&trimmed));
        }
    }
    tokens
}

/// Splits snake_case or compound identifiers into sub-tokens.
fn tokenize_identifier(ident: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for part in ident.split('_') {
        let part_clean = part.trim().to_lowercase();
        if part_clean.len() >= 2 && !is_stop_word(&part_clean) && part_clean != ident {
            parts.push(part_clean);
        }
    }
    parts
}

/// Returns higher weights for critical vulnerability and error domain terms.
fn get_keyword_weight(word: &str) -> f32 {
    match word {
        "deadlock" | "uaf" | "overflow" | "underflow" | "dereference" | "leak" | "corruption"
        | "race" | "uninitialized" | "double_free" | "out-of-bounds" | "bounds" | "oob"
        | "spinlock" | "mutex" | "rcu" | "refcount" | "atomic" => 3.0,
        "null" | "pointer" | "free" | "alloc" | "kfree" | "kmalloc" | "kzalloc" | "lock"
        | "unlock" | "error" | "panic" | "hang" | "fault" | "crash" => 2.0,
        _ => 1.0,
    }
}

/// Common English stop words to exclude from feature vectors.
fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "a"
            | "an"
            | "and"
            | "or"
            | "but"
            | "is"
            | "are"
            | "was"
            | "were"
            | "in"
            | "on"
            | "at"
            | "by"
            | "for"
            | "with"
            | "about"
            | "against"
            | "between"
            | "into"
            | "through"
            | "during"
            | "before"
            | "after"
            | "above"
            | "below"
            | "to"
            | "from"
            | "up"
            | "down"
            | "of"
            | "off"
            | "over"
            | "under"
            | "this"
            | "that"
            | "these"
            | "those"
            | "it"
            | "its"
            | "as"
            | "if"
            | "when"
            | "where"
            | "why"
            | "how"
            | "all"
            | "any"
            | "both"
            | "each"
            | "few"
            | "more"
            | "most"
            | "other"
            | "some"
            | "such"
            | "no"
            | "nor"
            | "not"
            | "only"
            | "own"
            | "same"
            | "so"
            | "than"
            | "too"
            | "very"
            | "can"
            | "will"
            | "just"
            | "should"
            | "now"
    )
}

/// Searches a collection of known pre-existing bugs and returns the top matching candidates.
pub fn find_top_candidates(
    query_vector: &BugVector,
    known_bugs: &[PreexistingBug],
    top_n: usize,
    threshold: f32,
) -> Vec<CandidateMatch> {
    let mut matches = Vec::new();

    for bug in known_bugs {
        let bug_vec = if let Some(ref v_json) = bug.vector_json
            && let Some(v) = BugVector::from_json(v_json)
        {
            v
        } else {
            let files = bug.source_files.clone().unwrap_or_default();
            extract_bug_vector(
                &bug.problem,
                bug.subsystem.as_deref(),
                &files,
                bug.locations.as_ref(),
            )
        };

        let sim = query_vector.cosine_similarity(&bug_vec);
        if sim >= threshold {
            matches.push(CandidateMatch {
                bug: bug.clone(),
                similarity: sim,
            });
        }
    }

    // Sort descending by similarity score
    matches.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if matches.len() > top_n {
        matches.truncate(top_n);
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Severity;
    use serde_json::json;

    #[test]
    fn test_extract_vector_and_similarity() {
        let vec1 = extract_bug_vector(
            "Null pointer dereference in e1000_clean_rx_irq",
            Some("net"),
            &["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            Some(&json!([{
                "file": "drivers/net/ethernet/intel/e1000/e1000_main.c",
                "function_or_symbol": "e1000_clean_rx_irq"
            }])),
        );

        // Identical vector should have similarity 1.0
        let self_sim = vec1.cosine_similarity(&vec1);
        assert!((self_sim - 1.0).abs() < 1e-4);

        // Highly related bug (same file and function, slightly different description)
        let vec2 = extract_bug_vector(
            "Possible NULL dereference when rx ring buffer is empty in e1000_clean_rx_irq",
            Some("net"),
            &["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            Some(&json!([{
                "file": "drivers/net/ethernet/intel/e1000/e1000_main.c",
                "function_or_symbol": "e1000_clean_rx_irq"
            }])),
        );

        let sim_related = vec1.cosine_similarity(&vec2);
        assert!(
            sim_related > 0.6,
            "Related bugs should have high similarity, got {}",
            sim_related
        );

        // Completely unrelated bug (different subsystem, file, problem)
        let vec3 = extract_bug_vector(
            "Deadlock in ext4_evict_inode during journaling commit",
            Some("fs"),
            &["fs/ext4/inode.c".to_string()],
            Some(&json!([{
                "file": "fs/ext4/inode.c",
                "function_or_symbol": "ext4_evict_inode"
            }])),
        );

        let sim_unrelated = vec1.cosine_similarity(&vec3);
        assert!(
            sim_unrelated < 0.1,
            "Unrelated bugs should have very low similarity, got {}",
            sim_unrelated
        );
    }

    #[test]
    fn test_vector_json_roundtrip() {
        let vec = extract_bug_vector(
            "Memory leak in foo_init",
            Some("kernel"),
            &["kernel/foo.c".to_string()],
            None,
        );

        let json_str = vec.to_json();
        let deserialized = BugVector::from_json(&json_str).expect("Should deserialize");
        assert_eq!(vec, deserialized);
    }

    #[test]
    fn test_find_top_candidates_ranking() {
        let known_bug_1 = PreexistingBug {
            id: 1,
            slug: "pb-1".to_string(),
            problem: "Null pointer dereference in e1000_clean_rx_irq".to_string(),
            severity: Severity::High,
            severity_explanation: None,
            locations: Some(
                json!([{ "file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "function_or_symbol": "e1000_clean_rx_irq" }]),
            ),
            subsystem: Some("net".to_string()),
            source_files: Some(vec![
                "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
            ]),
            inline_review: "review 1".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            created_at: 100,
        };

        let known_bug_2 = PreexistingBug {
            id: 2,
            slug: "pb-2".to_string(),
            problem: "Memory leak in e1000_probe".to_string(),
            severity: Severity::High,
            severity_explanation: None,
            locations: Some(
                json!([{ "file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "function_or_symbol": "e1000_probe" }]),
            ),
            subsystem: Some("net".to_string()),
            source_files: Some(vec![
                "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
            ]),
            inline_review: "review 2".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            created_at: 200,
        };

        let known_bug_3 = PreexistingBug {
            id: 3,
            slug: "pb-3".to_string(),
            problem: "Unrelated deadlock in fs/btrfs/super.c".to_string(),
            severity: Severity::Critical,
            severity_explanation: None,
            locations: Some(json!([{ "file": "fs/btrfs/super.c" }])),
            subsystem: Some("fs".to_string()),
            source_files: Some(vec!["fs/btrfs/super.c".to_string()]),
            inline_review: "review 3".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            created_at: 300,
        };

        let query = extract_bug_vector(
            "e1000_clean_rx_irq causes null pointer exception",
            Some("net"),
            &["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            Some(
                &json!([{ "file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "function_or_symbol": "e1000_clean_rx_irq" }]),
            ),
        );

        let candidates = vec![
            known_bug_1.clone(),
            known_bug_2.clone(),
            known_bug_3.clone(),
        ];
        let matches = find_top_candidates(&query, &candidates, 20, 0.15);

        assert!(!matches.is_empty());
        assert_eq!(
            matches[0].bug.id, 1,
            "Top match should be bug 1 (exact function and file match)"
        );
        assert!(
            matches.iter().all(|m| m.bug.id != 3),
            "Unrelated bug 3 should not match"
        );
    }
}

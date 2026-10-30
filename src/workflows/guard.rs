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

//! Security and normalization helpers shared across review workflows.

/// Strips known prompt-bundle directory prefixes from a guide or prompt path
/// returned by an LLM (e.g. `"review-prompts/kernel/subsystem/locking.md"` or
/// `"subsystem/locking.md"` -> `"locking.md"` for pre-screen guide names).
pub fn normalize_guide_name(name: &str) -> &str {
    let mut s = name.trim();
    for prefix in [
        "review-prompts/kernel/",
        "review-prompts/sashiko/",
        "review-prompts/",
        "third_party/prompts/kernel/",
        "projects/sashiko/prompts/",
        "kernel/",
        "sashiko/",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    for prefix in ["subsystem/", "patterns/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    s
}

/// Strips known outer repository or bundle prefixes from a relative prompt path
/// while preserving a valid `"subsystem/..."` or `"patterns/..."` subdirectory
/// component (e.g. `"review-prompts/kernel/subsystem/locking.md"` ->
/// `"subsystem/locking.md"`).
pub fn normalize_prompt_relpath(path: &str) -> &str {
    let s = path.trim();
    for prefix in [
        "review-prompts/kernel/",
        "review-prompts/sashiko/",
        "review-prompts/",
        "third_party/prompts/kernel/",
        "projects/sashiko/prompts/",
        "kernel/",
        "sashiko/",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest;
        }
    }
    s
}

/// Verifies that a guide name selected by an LLM (e.g. during pre-screening) is
/// a plain filename without path separators or parent directory components.
///
/// Selected guide names are joined against the prompt bundle directory and
/// inlined into stage system prompts. Since patches from untrusted sources can
/// influence LLM selection, any name containing path traversal characters is
/// rejected.
pub fn sanitize_guide_name(name: &str) -> bool {
    let plain =
        !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..");
    if !plain {
        tracing::warn!("Ignoring prescreen guide with a path in its name: {name}");
    }
    plain
}

/// Verifies that a relative prompt path (from `def.guides` or
/// `selected_guides`) is a safe `.md` path within the prompt bundle without
/// parent-directory traversal or absolute components.
pub fn sanitize_prompt_relpath(path: &str) -> bool {
    let trimmed = normalize_prompt_relpath(path);
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || !trimmed.ends_with(".md")
    {
        return false;
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    match parts.as_slice() {
        [file] => !file.is_empty(),
        [dir, file] => matches!(*dir, "subsystem" | "patterns") && !file.is_empty(),
        _ => false,
    }
}

/// Normalizes a stage name returned by an LLM planner or CLI flag to its
/// canonical lowercase kebab-case identifier without leading `stage-` prefix.
pub fn normalize_stage_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase().replace('_', "-");
    if let Some(stripped) = lower.strip_prefix("stage-") {
        stripped.to_string()
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_guide_name_strips_bundle_prefixes() {
        assert_eq!(
            normalize_guide_name("review-prompts/kernel/subsystem/networking-core.md"),
            "networking-core.md"
        );
        assert_eq!(normalize_guide_name("subsystem/locking.md"), "locking.md");
        assert_eq!(
            normalize_guide_name("patterns/rust-async.md"),
            "rust-async.md"
        );
        assert_eq!(normalize_guide_name("locking.md"), "locking.md");
    }

    #[test]
    fn test_sanitize_guide_name_accepts_plain_filename() {
        assert!(sanitize_guide_name("locking.md"));
        assert!(sanitize_guide_name("workflow-engine.md"));
        assert!(sanitize_guide_name("rust-async.md"));
    }

    #[test]
    fn test_sanitize_guide_name_rejects_traversal_and_paths() {
        assert!(!sanitize_guide_name(""));
        assert!(!sanitize_guide_name("../etc/passwd"));
        assert!(!sanitize_guide_name("subsystem/locking.md"));
        assert!(!sanitize_guide_name("subsystem\\locking.md"));
        assert!(!sanitize_guide_name(".."));
    }

    #[test]
    fn test_sanitize_prompt_relpath() {
        assert!(sanitize_prompt_relpath("technical-patterns.md"));
        assert!(sanitize_prompt_relpath("callstack.md"));
        assert!(sanitize_prompt_relpath("subsystem/locking.md"));
        assert!(sanitize_prompt_relpath("patterns/rust-async.md"));
        assert!(!sanitize_prompt_relpath(""));
        assert!(!sanitize_prompt_relpath("../secret.md"));
        assert!(!sanitize_prompt_relpath("/etc/passwd.md"));
        assert!(!sanitize_prompt_relpath("other/file.md"));
        assert!(!sanitize_prompt_relpath("subsystem/nested/file.md"));
        assert!(!sanitize_prompt_relpath("locking.txt"));
    }

    #[test]
    fn test_normalize_stage_name() {
        assert_eq!(
            normalize_stage_name("Stage_Execution_Flow"),
            "execution-flow"
        );
        assert_eq!(normalize_stage_name("stage-llm-pipeline"), "llm-pipeline");
        assert_eq!(normalize_stage_name("  CONCURRENCY "), "concurrency");
    }
}

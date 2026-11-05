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

#[cfg(test)]
mod tests {
    use crate::toolbox::ToolBox;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::runtime::Runtime;

    /// The number of lines in a page git_show returns for a line range.
    ///
    /// Not str::lines: the page is its lines joined with '\n', so one that
    /// ends on a blank line ends in '\n', and lines() does not count a blank
    /// last line. These tests page through this repository's own README, so
    /// which line is blank changes with every edit to it.
    fn page_line_count(page: &str) -> usize {
        page.split('\n').count()
    }

    fn get_test_paths() -> PathBuf {
        // Use current repo as the test repo
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
    }

    #[test]
    fn test_virtualize_ref() {
        let mut toolbox = ToolBox::new(PathBuf::from("."));

        // Without virtual HEAD, should return original
        assert_eq!(toolbox.virtualize_ref("HEAD"), "HEAD");
        assert_eq!(toolbox.virtualize_ref("HEAD~1"), "HEAD~1");
        assert_eq!(toolbox.virtualize_ref("origin/HEAD"), "origin/HEAD");

        // Set virtual HEAD
        toolbox.set_virtual_head("abc123e".to_string());

        // Replacements
        assert_eq!(toolbox.virtualize_ref("HEAD"), "abc123e");
        assert_eq!(toolbox.virtualize_ref("HEAD~1"), "abc123e~1");
        assert_eq!(toolbox.virtualize_ref("HEAD^"), "abc123e^");
        assert_eq!(
            toolbox.virtualize_ref("baseline..HEAD"),
            "baseline..abc123e"
        );
        assert_eq!(
            toolbox.virtualize_ref("HEAD..baseline"),
            "abc123e..baseline"
        );
        assert_eq!(toolbox.virtualize_ref("HEAD:file.c"), "abc123e:file.c");

        // Non-replacements
        assert_eq!(toolbox.virtualize_ref("origin/HEAD"), "origin/HEAD");
        assert_eq!(toolbox.virtualize_ref("origin/HEAD~1"), "origin/HEAD~1");
        assert_eq!(
            toolbox.virtualize_ref("refs/remotes/origin/HEAD"),
            "refs/remotes/origin/HEAD"
        );
        assert_eq!(toolbox.virtualize_ref("FOREHEAD"), "FOREHEAD");
        assert_eq!(toolbox.virtualize_ref("my-HEAD-branch"), "my-HEAD-branch");
        assert_eq!(toolbox.virtualize_ref("HEAD-fixes"), "HEAD-fixes");
    }

    #[test]
    fn test_git_ls_linux() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "revision": "HEAD", "path": "." });
        let result = rt.block_on(toolbox.call("git_ls", args)).unwrap();
        let text = result.as_str().unwrap();
        let entries: Vec<&str> = text.lines().collect();

        assert!(entries.contains(&"README.md"));
        assert!(entries.contains(&"Cargo.toml"));
        assert!(entries.contains(&"src/"));
    }

    #[test]
    fn test_read_files_linux_readme() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "README.md", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let content = result.as_str().unwrap();

        assert!(content.starts_with("=== README.md (lines 1-5 of "));
        assert!(content.contains("\n1: "));
        assert!(content.contains("\n5: "));
        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_log() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "range": "HEAD", "limit": 1 });
        let result = rt.block_on(toolbox.call("git_log", args)).unwrap();
        let output = result.as_str().unwrap();

        assert!(output.contains("commit"));
        assert!(output.contains("Author:"));
    }

    #[test]
    fn test_git_find_files_and_git_diff_plain_text() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let find_res = rt
            .block_on(toolbox.call(
                "git_find_files",
                json!({ "revision": "HEAD", "pattern": "*.rs", "path": "src/toolbox" }),
            ))
            .unwrap();
        let find_text = find_res.as_str().unwrap();
        assert!(find_text.starts_with("Found "));
        assert!(find_text.contains("src/toolbox/mod.rs"));

        let empty_diff = rt
            .block_on(toolbox.call(
                "git_diff",
                json!({ "base_revision": "HEAD", "target_revision": "HEAD" }),
            ))
            .unwrap();
        assert_eq!(empty_diff.as_str().unwrap(), "No differences found.");
    }

    #[test]
    fn test_git_show_head() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result.as_str().unwrap();

        assert!(content.contains("commit"));
        assert!(content.contains("Author:"));
    }

    fn setup_test_repo() -> (tempfile::TempDir, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["commit", "--allow-empty", "-m", "Initial commit"]);
        run_git(&["commit", "--allow-empty", "-m", "Second commit"]);

        (temp_dir, repo_path)
    }

    #[test]
    fn test_git_show_virtual_head() {
        let (_temp_dir, repo_path) = setup_test_repo();
        let mut toolbox = ToolBox::new(repo_path.clone());
        let rt = Runtime::new().unwrap();

        // Resolve actual HEAD~1 SHA
        let output = crate::git_cmd::in_dir(&repo_path)
            .args(["rev-parse", "HEAD~1"])
            .output()
            .unwrap();
        let head_minus_1 = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Set virtual HEAD to HEAD~1
        toolbox.set_virtual_head(head_minus_1.clone());

        // Call git_show with "HEAD"
        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result.as_str().unwrap();

        // The content should match the commit info of HEAD~1 (which is head_minus_1)
        assert!(content.contains(&head_minus_1));

        // It should NOT contain the current HEAD SHA
        let output_current = crate::git_cmd::in_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let current_head = String::from_utf8(output_current.stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(!content.contains(&current_head));
    }
    #[test]
    fn test_git_show_file_full() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD:README.md" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result.as_str().unwrap();

        assert!(content.starts_with("=== HEAD:README.md (lines 1-"));
        assert!(content.contains("\n1: "));
        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_show_file_range() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 1,
            "end_line": 5
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result.as_str().unwrap();
        let (header, body) = content.split_once('\n').unwrap();

        assert!(header.starts_with("=== HEAD:README.md (lines 1-5 of "));
        let lines_count = page_line_count(body);
        assert_eq!(lines_count, 5);
        assert!(body.starts_with("1: "));
    }

    #[test]
    fn test_git_show_file_default_limit() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 10
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result.as_str().unwrap();
        let (header, body) = content.split_once('\n').unwrap();

        assert!(header.starts_with("=== HEAD:README.md (lines 10-110 of "));
        let lines_count = page_line_count(body);
        assert_eq!(lines_count, 101); // 10 to 110 inclusive is 101 lines
        assert!(body.starts_with("10: "));
    }

    #[test]
    fn test_git_show_raw_caching() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        // Clear/initialize cache check
        assert!(toolbox.cache.read().unwrap().is_empty());

        // 1. Read subrange lines 1-5
        let args1 = json!({
            "object": "HEAD:README.md",
            "start_line": 1,
            "end_line": 5
        });
        let result1 = rt.block_on(toolbox.call("git_show", args1)).unwrap();
        assert!(
            result1
                .as_str()
                .unwrap()
                .starts_with("=== HEAD:README.md (lines 1-5 of ")
        );

        // Verify raw cache was populated
        let raw_key = "git_show_raw:HEAD:README.md:false:None";
        {
            let cache = toolbox.cache.read().unwrap();
            assert!(cache.contains_key(raw_key), "Raw key should be cached");
            assert!(
                cache
                    .get(raw_key)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("Sashiko")
            );
        }

        // 2. Read different subrange lines 10-15 of the same file
        let args2 = json!({
            "object": "HEAD:README.md",
            "start_line": 10,
            "end_line": 15
        });
        let result2 = rt.block_on(toolbox.call("git_show", args2)).unwrap();
        assert!(
            result2
                .as_str()
                .unwrap()
                .starts_with("=== HEAD:README.md (lines 10-15 of ")
        );

        // Verify that no extra git_show raw keys were created
        {
            let cache = toolbox.cache.read().unwrap();
            // There should be exactly 3 keys:
            // 1. git_show:{"end_line":5,"object":"HEAD:README.md","start_line":1}
            // 2. git_show_raw:HEAD:README.md:false:None
            // 3. git_show:{"end_line":15,"object":"HEAD:README.md","start_line":10}
            assert_eq!(cache.len(), 3);
        }
    }

    #[test]
    fn test_git_blame_readme() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args =
            json!({ "revision": "HEAD", "path": "README.md", "start_line": 1, "end_line": 3 });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        let content = result.as_str().unwrap();

        assert!(content.starts_with("=== README.md blame (lines 1-3 of 3) ==="));
        assert_eq!(content.lines().count(), 4);
    }

    #[test]
    fn test_git_blame_truncation() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "path": "src/worker/prompts.rs",
            "start_line": 1,
            "end_line": 3000
        });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        let text = result.as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();

        let header = lines[0];
        let footer = *lines.last().unwrap();
        assert!(header.starts_with("=== src/worker/prompts.rs blame (lines 1-"));
        assert!(footer.starts_with("[Truncated: "));

        // Parse end_idx from "=== src/worker/prompts.rs blame (lines 1-<end_idx> of ..."
        let after_prefix = header
            .strip_prefix("=== src/worker/prompts.rs blame (lines 1-")
            .unwrap();
        let end_idx: usize = after_prefix
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();

        // Total lines = 1 header + end_idx blame lines + 1 Truncator warning + 1 continuation footer.
        assert!(lines.len() < 2400, "Blame was not truncated!");
        assert_eq!(lines.len(), end_idx + 3);
        assert!(footer.contains(&format!("start_line={}", end_idx + 1)));
    }

    #[test]
    fn test_git_grep_relative_path() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        // Search for "Sashiko" which should be in README.md
        let args = json!({
            "revision": "HEAD",
            "pattern": "Sashiko",
            "path": "README.md"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        let content = result.as_str().unwrap();

        assert!(!content.is_empty());
        for line in content.lines() {
            assert!(
                !line.starts_with('/'),
                "Line starts with absolute path: {}",
                line
            );
        }

        assert!(content.contains("README.md") || content.contains("./README.md"));
    }

    #[test]
    fn test_git_grep_hyphen_pattern() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        // Search for a pattern starting with a hyphen (e.g. "-ETIMEDOUT" or "--")
        let args = json!({
            "revision": "HEAD",
            "pattern": "-ETIMEDOUT",
            "path": "src/"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        let text = result.as_str().unwrap();
        assert!(!text.starts_with("Error:"), "unexpected grep error: {text}");
    }

    #[test]
    fn test_git_grep_function_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        std::fs::write(
            repo_path.join("sample.c"),
            "int helper(void)\n{\n    return 1;\n}\n\nint target_caller(int arg)\n{\n    int x = arg + 1;\n    if (x > 10)\n        return special_sink(x);\n    return 0;\n}\n",
        )
        .unwrap();
        run_git(&["add", "sample.c"]);
        run_git(&["commit", "-m", "Add sample.c"]);

        let toolbox = ToolBox::new(repo_path);
        let rt = Runtime::new().unwrap();

        let res = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({
                    "revision": "HEAD",
                    "pattern": "special_sink",
                    "function_context": true
                }),
            ))
            .unwrap();
        let text = res.as_str().unwrap();
        assert!(text.contains("[file: sample.c]"));
        assert!(text.contains("6=int target_caller(int arg)"));
        assert!(text.contains("10:        return special_sink(x);"));
        assert!(!text.contains("int helper(void)"));

        // Verify normalize_args cache-key parity for default function_context=false
        let before = toolbox.cache.read().unwrap().len();
        let _ = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({ "revision": "HEAD", "pattern": "special_sink" }),
            ))
            .unwrap();
        let after_one = toolbox.cache.read().unwrap().len();
        assert_eq!(after_one, before + 1);
        let _ = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({
                    "revision": "HEAD",
                    "pattern": "special_sink",
                    "context_lines": 0,
                    "function_context": false,
                    "count_only": false,
                    "is_literal": false
                }),
            ))
            .unwrap();
        assert_eq!(toolbox.cache.read().unwrap().len(), after_one);
    }

    #[test]
    fn test_git_grep_invalid_revision() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "--invalid-option",
            "pattern": "foo"
        });

        let result = rt.block_on(toolbox.call("git_grep", args));
        assert!(result.is_err());
    }

    #[test]
    fn test_git_read_files_truncation() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "Cargo.lock" }
            ]
        });

        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let text = result.as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();

        let header = lines[0];
        let footer = *lines.last().unwrap();
        assert!(header.starts_with("=== Cargo.lock (lines 1-"));
        assert!(footer.starts_with("[Truncated: lines 1-"));

        let after_prefix = header.strip_prefix("=== Cargo.lock (lines 1-").unwrap();
        let end_idx: usize = after_prefix
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();

        // 1 header + end_idx numbered lines + 1 Truncator warning + 1 continuation footer.
        assert!(lines.len() < 3200, "Content was not truncated!");
        assert_eq!(lines.len(), end_idx + 3);
        assert!(lines[1].starts_with("1: "));
        assert!(lines[end_idx].starts_with(&format!("{end_idx}: ")));
        assert!(footer.contains(&format!("start_line={}", end_idx + 1)));
    }

    #[test]
    fn test_git_show_truncation() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:Cargo.lock",
            "start_line": 1,
            "end_line": 4000
        });

        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let text = result.as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();

        let header = lines[0];
        let footer = *lines.last().unwrap();
        assert!(header.starts_with("=== HEAD:Cargo.lock (lines 1-"));
        assert!(footer.starts_with("[Truncated: lines 1-"));

        let after_prefix = header
            .strip_prefix("=== HEAD:Cargo.lock (lines 1-")
            .unwrap();
        let end_idx: usize = after_prefix
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();

        assert!(lines.len() < 3200, "Content was not truncated!");
        assert_eq!(lines.len(), end_idx + 3);
        assert!(footer.contains(&format!("start_line={}", end_idx + 1)));
    }

    #[test]
    fn test_git_read_files_empty_file_with_range() {
        // Regression: reading a file that is empty at the revision with an
        // explicit line range must not panic. total_lines is 0 for such a
        // file, and the bounds clamp used to invert (lower 1 > upper 0).
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo_path.join("empty.txt"), "").unwrap();
        run_git(&["add", "empty.txt"]);
        run_git(&["commit", "-m", "Add empty file"]);

        let toolbox = ToolBox::new(repo_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "empty.txt", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "=== empty.txt (empty, 0 lines) ==="
        );
    }

    #[test]
    fn test_git_read_files_with_float_line_ranges() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                {
                    "path": "README.md",
                    "start_line": 3.0,
                    "end_line": 6.0
                }
            ]
        });

        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let text = result.as_str().unwrap();
        assert!(text.starts_with("=== README.md (lines 3-6 of "));
        assert!(text.contains("\n3: "));
        assert!(text.contains("\n6: "));
    }

    #[test]
    fn test_git_tools_reject_invalid_paths() {
        let linux_path = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        for bad_path in ["../outside", "/etc/passwd"] {
            // 1. git_blame
            let res = rt.block_on(
                toolbox.call("git_blame", json!({ "revision": "HEAD", "path": bad_path })),
            );
            assert!(res.is_err(), "git_blame should reject {bad_path}");

            // 2. git_diff
            let res = rt.block_on(toolbox.call(
                "git_diff",
                json!({
                    "base_revision": "HEAD~1",
                    "target_revision": "HEAD",
                    "paths": [bad_path]
                }),
            ));
            assert!(res.is_err(), "git_diff should reject {bad_path}");

            // 3. git_find_files
            let res = rt.block_on(toolbox.call(
                "git_find_files",
                json!({
                    "revision": "HEAD",
                    "pattern": "*",
                    "path": bad_path
                }),
            ));
            assert!(res.is_err(), "git_find_files should reject {bad_path}");

            // 4. git_grep
            let res = rt.block_on(toolbox.call(
                "git_grep",
                json!({
                    "revision": "HEAD",
                    "pattern": "Sashiko",
                    "path": bad_path
                }),
            ));
            assert!(res.is_err(), "git_grep should reject {bad_path}");

            // 5. git_ls
            let res = rt
                .block_on(toolbox.call("git_ls", json!({ "revision": "HEAD", "path": bad_path })));
            assert!(res.is_err(), "git_ls should reject {bad_path}");

            // 6. git_read_files
            let res = rt
                .block_on(toolbox.call(
                    "git_read_files",
                    json!({
                        "revision": "HEAD",
                        "files": [{ "path": bad_path }]
                    }),
                ))
                .unwrap();
            let file_text = res.as_str().unwrap();
            assert!(
                file_text.contains(": ERROR:"),
                "git_read_files should report error for {bad_path}: {file_text}"
            );

            // 7a. git_show with object rev:path
            let res = rt.block_on(
                toolbox.call("git_show", json!({ "object": format!("HEAD:{bad_path}") })),
            );
            assert!(res.is_err(), "git_show object should reject {bad_path}");

            // 7b. git_show with paths filter
            let res = rt.block_on(
                toolbox.call("git_show", json!({ "object": "HEAD", "paths": [bad_path] })),
            );
            assert!(res.is_err(), "git_show paths should reject {bad_path}");

            // 8a. git_log with paths filter
            let res = rt
                .block_on(toolbox.call("git_log", json!({ "range": "HEAD", "paths": [bad_path] })));
            assert!(res.is_err(), "git_log paths should reject {bad_path}");

            // 8b. git_log with line_range path
            let res = rt.block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "line_range": { "path": bad_path, "start_line": 1, "end_line": 5 }
                }),
            ));
            assert!(res.is_err(), "git_log line_range should reject {bad_path}");
        }
    }

    #[test]
    fn test_git_tools_historical_deleted_paths() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);

        let nested_dir = repo_path.join("deleted_dir/sub");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(
            nested_dir.join("file.c"),
            "int old_func(void) { return 42; }\n",
        )
        .unwrap();
        run_git(&["add", "deleted_dir/sub/file.c"]);
        run_git(&["commit", "-m", "Add nested file"]);

        run_git(&["rm", "-r", "deleted_dir"]);
        std::fs::write(repo_path.join("keep.c"), "int keep(void) { return 0; }\n").unwrap();
        run_git(&["add", "keep.c"]);
        run_git(&["commit", "-m", "Remove nested dir"]);

        assert!(!repo_path.join("deleted_dir").exists());

        let toolbox = ToolBox::new(repo_path);
        let rt = Runtime::new().unwrap();

        // git_read_files on historical deleted path in HEAD~1
        let read_res = rt
            .block_on(toolbox.call(
                "git_read_files",
                json!({
                    "revision": "HEAD~1",
                    "files": [{ "path": "deleted_dir/sub/file.c" }]
                }),
            ))
            .unwrap();
        assert!(read_res.as_str().unwrap().contains("1: int old_func"));

        // git_show on historical deleted path in HEAD~1
        let show_res = rt
            .block_on(toolbox.call(
                "git_show",
                json!({ "object": "HEAD~1:deleted_dir/sub/file.c" }),
            ))
            .unwrap();
        assert!(show_res.as_str().unwrap().contains("1: int old_func"));

        // git_log on historical deleted path finds both addition and deletion commits
        let log_res = rt
            .block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "paths": ["deleted_dir/sub/file.c"],
                    "oneline": true
                }),
            ))
            .unwrap();
        let log_text = log_res.as_str().unwrap();
        assert!(log_text.contains("Add nested file"));
        assert!(log_text.contains("Remove nested dir"));
    }

    #[test]
    fn test_git_log_paths_line_range_and_pickaxe() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);

        std::fs::write(
            repo_path.join("alpha.c"),
            "int compute_val(int x)\n{\n    return x + 1;\n}\n",
        )
        .unwrap();
        std::fs::write(repo_path.join("beta.c"), "int beta(void) { return 0; }\n").unwrap();
        run_git(&["add", "alpha.c", "beta.c"]);
        run_git(&["commit", "-m", "Add alpha and beta"]);

        std::fs::write(
            repo_path.join("alpha.c"),
            "int compute_val(int x)\n{\n    if (x < 0)\n        return -EINVAL;\n    return x + 2;\n}\n",
        )
        .unwrap();
        run_git(&["add", "alpha.c"]);
        run_git(&["commit", "-m", "Handle negative input in compute_val"]);

        std::fs::write(repo_path.join("beta.c"), "int beta(void) { return 99; }\n").unwrap();
        run_git(&["add", "beta.c"]);
        run_git(&["commit", "-m", "Update beta return value"]);

        let toolbox = ToolBox::new(repo_path);
        let rt = Runtime::new().unwrap();

        // 1. Path filtering isolates alpha.c commits from beta-only commits
        let by_path = rt
            .block_on(toolbox.call(
                "git_log",
                json!({ "range": "HEAD", "paths": ["alpha.c"], "oneline": true }),
            ))
            .unwrap();
        let by_path_text = by_path.as_str().unwrap();
        assert!(by_path_text.contains("Handle negative input in compute_val"));
        assert!(!by_path_text.contains("Update beta return value"));

        // 2. Pickaxe (-S) with a leading hyphen ("-EINVAL") must not be parsed as a git flag
        let pickaxe_s = rt
            .block_on(toolbox.call(
                "git_log",
                json!({ "range": "HEAD", "pickaxe": "-EINVAL", "oneline": true }),
            ))
            .unwrap();
        let pickaxe_s_text = pickaxe_s.as_str().unwrap();
        assert!(pickaxe_s_text.contains("Handle negative input in compute_val"));
        assert!(!pickaxe_s_text.contains("Add alpha and beta"));

        // 3. Regex pickaxe (-G)
        let pickaxe_g = rt
            .block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "pickaxe": "return [0-9]{2};",
                    "pickaxe_regex": true,
                    "oneline": true
                }),
            ))
            .unwrap();
        assert!(
            pickaxe_g
                .as_str()
                .unwrap()
                .contains("Update beta return value")
        );

        // 4. Line evolution (-L start,end:path) and redundant matching paths tolerance
        let line_hist = rt
            .block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "paths": ["alpha.c"],
                    "line_range": { "path": "alpha.c", "start_line": 1, "end_line": 6 }
                }),
            ))
            .unwrap();
        let line_hist_text = line_hist.as_str().unwrap();
        assert!(line_hist_text.contains("Handle negative input in compute_val"));
        assert!(line_hist_text.contains("-    return x + 1;"));

        // 5. Symbol evolution (-L :symbol:path) on C top-level function
        let sym_hist = rt
            .block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "line_range": { "path": "alpha.c", "symbol": "compute_val" }
                }),
            ))
            .unwrap();
        assert!(
            sym_hist
                .as_str()
                .unwrap()
                .contains("Handle negative input in compute_val")
        );

        // 6. Conflicting paths alongside line_range and unsafe symbol injection are rejected
        assert!(
            rt.block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "paths": ["beta.c"],
                    "line_range": { "path": "alpha.c", "start_line": 1, "end_line": 2 }
                }),
            ))
            .is_err()
        );
        assert!(
            rt.block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "line_range": { "path": "alpha.c", "symbol": "bad:inject" }
                }),
            ))
            .is_err()
        );

        // 7. normalize_args canonicalizes omitted vs explicit default parameters to one cache key
        let count_before = toolbox.cache.read().unwrap().len();
        let _ = rt
            .block_on(toolbox.call("git_log", json!({ "range": "HEAD" })))
            .unwrap();
        let count_after_first = toolbox.cache.read().unwrap().len();
        assert_eq!(count_after_first, count_before + 1);
        let _ = rt
            .block_on(toolbox.call(
                "git_log",
                json!({
                    "range": "HEAD",
                    "limit": 10,
                    "paths": [],
                    "pickaxe": "",
                    "pickaxe_regex": false,
                    "oneline": false
                }),
            ))
            .unwrap();
        assert_eq!(toolbox.cache.read().unwrap().len(), count_after_first);
    }

    #[test]
    fn test_tool_declarations_come_in_a_stable_order() {
        // The declarations go into every request, so their order is part of
        // the response cache key and of the prompt prefix a provider can
        // cache. They must not depend on the registry's hash map order,
        // which is random for every new registry.
        let repo = get_test_paths();
        let names = |tb: &ToolBox| -> Vec<String> {
            tb.get_declarations_generic()
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        let first = names(&ToolBox::new(repo.clone()));
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "declarations are not in name order");
        assert_eq!(first, names(&ToolBox::new(repo)));
    }
}

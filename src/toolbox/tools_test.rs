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

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        // Use current repo as the test repo
        let linux_path = root.clone();
        let prompts_path = root.join("third_party/prompts/kernel");
        (linux_path, prompts_path)
    }

    #[test]
    fn test_virtualize_ref() {
        let mut toolbox = ToolBox::new(PathBuf::from("."), None);

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
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "revision": "HEAD", "path": "." });
        let result = rt.block_on(toolbox.call("git_ls", args)).unwrap();
        let entries = result["entries"].as_array().unwrap();

        assert!(entries.iter().any(|e| e["name"] == "README.md"));
        assert!(entries.iter().any(|e| e["name"] == "Cargo.toml"));
    }

    #[test]
    fn test_read_files_linux_readme() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "README.md", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let content = results[0]["content"].as_str().unwrap();

        assert!(!content.is_empty());
        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_log() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "range": "HEAD", "limit": 1 });
        let result = rt.block_on(toolbox.call("git_log", args)).unwrap();
        let output = result["output"].as_str().unwrap();

        assert!(output.contains("commit"));
        assert!(output.contains("Author:"));
    }

    #[test]
    fn test_git_show_head() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

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
        let mut toolbox = ToolBox::new(repo_path.clone(), None);
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
        let content = result["content"].as_str().unwrap();

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
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD:README.md" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_show_file_range() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 1,
            "end_line": 5
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();
        let end_line = result["end_line"].as_u64().unwrap();
        let start_line = result["start_line"].as_u64().unwrap();

        assert_eq!(start_line, 1);
        assert_eq!(end_line, 5);
        let lines_count = page_line_count(content);
        assert_eq!(lines_count, 5);
    }

    #[test]
    fn test_git_show_file_default_limit() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 10
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();
        let end_line = result["end_line"].as_u64().unwrap();
        let start_line = result["start_line"].as_u64().unwrap();

        assert_eq!(start_line, 10);
        assert_eq!(end_line, 110);
        let lines_count = page_line_count(content);
        assert_eq!(lines_count, 101); // 10 to 110 inclusive is 101 lines
    }

    #[test]
    fn test_git_show_raw_caching() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
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
        assert_eq!(result1["start_line"].as_u64().unwrap(), 1);
        assert_eq!(result1["end_line"].as_u64().unwrap(), 5);

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
        assert_eq!(result2["start_line"].as_u64().unwrap(), 10);
        assert_eq!(result2["end_line"].as_u64().unwrap(), 15);

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
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args =
            json!({ "revision": "HEAD", "path": "README.md", "start_line": 1, "end_line": 3 });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        // Typical git blame output starts with hash or (
        // e.g. ^1da177e4c3f (Linus Torvalds 2005-04-16 15:20:36 -0700 1) Linux kernel release 2.6.xx
    }

    #[test]
    fn test_git_blame_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "path": "src/worker/prompts.rs",
            "start_line": 1,
            "end_line": 3000
        });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        assert_eq!(result["truncated"].as_bool(), Some(true));

        let content = result["content"].as_str().unwrap();
        let returned_items = result["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("git_blame returned_items metadata: {}", returned_items);
        println!("git_blame actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 2400, "Blame was not truncated!");
        // returned_items should match the actual lines returned excluding the warning line.
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );

        // Verify end_index calculation is start_index + returned_items - 1
        let start_index = result["metadata"]["start_index"].as_u64().unwrap();
        let end_index = result["metadata"]["end_index"].as_u64().unwrap();
        assert_eq!(end_index, start_index + returned_items as u64 - 1);

        // Verify next_page_hint suggests end_index + 1
        let hint = result["next_page_hint"].as_str().unwrap();
        let expected_next_start = end_index + 1;
        assert!(hint.contains(&format!("start_line={}", expected_next_start)));
    }

    #[test]
    fn test_git_grep_relative_path() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Search for "Sashiko" which should be in README.md
        let args = json!({
            "revision": "HEAD",
            "pattern": "Sashiko",
            "path": "README.md"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        // Verify path is relative (does not start with /)
        // Check that no line starts with /
        for line in content.lines() {
            assert!(
                !line.starts_with("/"),
                "Line starts with absolute path: {}",
                line
            );
        }

        // Check if README.md matches are found (it might not be the first match)
        assert!(content.contains("README.md") || content.contains("./README.md"));
    }

    #[test]
    fn test_git_grep_hyphen_pattern() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Search for a pattern starting with a hyphen (e.g. "-ETIMEDOUT" or "--")
        let args = json!({
            "revision": "HEAD",
            "pattern": "-ETIMEDOUT",
            "path": "src/"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        // Should succeed without error even if no matches found
        assert!(
            result.get("content").is_some()
                || result.get("matches").is_some()
                || result.get("message").is_some()
        );
    }

    #[test]
    fn test_git_grep_invalid_revision() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "--invalid-option",
            "pattern": "foo"
        });

        let result = rt.block_on(toolbox.call("git_grep", args));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_prompt() {
        let (linux_path, prompts_path) = get_test_paths();
        // Enable prompt tool by passing path
        let toolbox = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()));
        let rt = Runtime::new().unwrap();

        // Ensure we have a dummy prompt file to read
        // The real review-prompts might not be populated in test env, check first
        // Or assume technical-patterns.md exists as per repo structure.
        // But tests might run in clean env. Let's create a dummy one if we can or check existence.
        // Since we are running in the actual repo, review-prompts should exist.

        let args = json!({ "name": "technical-patterns.md" });
        if prompts_path.join("technical-patterns.md").exists() {
            let result = rt
                .block_on(toolbox.call("read_prompt", args.clone()))
                .expect("Failed to call read_prompt");
            assert!(result.get("content").is_some());
        } else {
            // If file doesn't exist (e.g. CI), skip assertion on content but check tool availability
            println!("Skipping read_prompt content check: technical-patterns.md not found");
        }

        // Test disabled tool
        let toolbox_disabled = ToolBox::new(linux_path, None);
        let result = rt.block_on(toolbox_disabled.call("read_prompt", args));
        assert!(result.is_err());
    }

    /// read_prompt resolves the requested name against its base, so the base
    /// has to be the prompt directory. Handed a file, every call fails while
    /// canonicalizing, which is how a caller passing `<prompts>/tool.md` went
    /// unnoticed: the tool is optional, so its errors look like the model
    /// simply choosing not to use it.
    #[test]
    fn test_read_prompt_base_is_the_prompt_directory() {
        let (linux_path, prompts_path) = get_test_paths();
        let rt = Runtime::new().unwrap();
        // A guide the workflow already loads by name, so it cannot go missing
        // without breaking the review itself. The nested path also covers the
        // subdirectory case, which is most of what the tool is asked for.
        let args = json!({ "name": "subsystem/locking.md" });

        let toolbox = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()));
        let result = rt
            .block_on(toolbox.call("read_prompt", args.clone()))
            .expect("a name under the prompt directory must resolve");
        assert!(!result["content"].as_str().unwrap_or_default().is_empty());

        // A file as the base cannot resolve anything.
        let wrong = ToolBox::new(linux_path, Some(prompts_path.join("tool.md")));
        assert!(rt.block_on(wrong.call("read_prompt", args)).is_err());
    }

    #[test]
    fn test_git_read_files_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "Cargo.lock" }
            ]
        });

        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let res = &results[0];
        assert_eq!(res["truncated"].as_bool(), Some(true));

        let content = res["content"].as_str().unwrap();
        let returned_items = res["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("returned_items metadata: {}", returned_items);
        println!("actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 3200, "Content was not truncated!");
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );
    }

    #[test]
    fn test_git_show_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:Cargo.lock",
            "start_line": 1,
            "end_line": 4000
        });

        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        assert_eq!(result["truncated"].as_bool(), Some(true));

        let content = result["content"].as_str().unwrap();
        let returned_items = result["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("git_show returned_items metadata: {}", returned_items);
        println!("git_show actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 3200, "Content was not truncated!");
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "git_show returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );
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

        let toolbox = ToolBox::new(repo_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "empty.txt", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let res = &results[0];
        assert!(res.get("error").is_none(), "unexpected error: {res:?}");
        assert_eq!(res["content"].as_str().unwrap(), "");
        assert_eq!(res["total_lines"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_git_read_files_with_float_line_ranges() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
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
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["start_line"].as_u64().unwrap(), 3);
        assert_eq!(results[0]["end_line"].as_u64().unwrap(), 6);
    }

    #[test]
    fn test_git_tools_reject_invalid_paths() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
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
            let file_res = &res["results"][0];
            assert!(
                file_res["error"].is_string() && file_res.get("content").is_none(),
                "git_read_files should report error for {bad_path}: {file_res:?}"
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

        let toolbox = ToolBox::new(repo_path, None);
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
        assert!(
            read_res["results"][0]["content"]
                .as_str()
                .unwrap()
                .contains("old_func")
        );

        // git_show on historical deleted path in HEAD~1
        let show_res = rt
            .block_on(toolbox.call(
                "git_show",
                json!({ "object": "HEAD~1:deleted_dir/sub/file.c" }),
            ))
            .unwrap();
        assert!(show_res["content"].as_str().unwrap().contains("old_func"));
    }

    #[test]
    fn test_tool_declarations_come_in_a_stable_order() {
        // The declarations go into every request, so their order is part of
        // the response cache key and of the prompt prefix a provider can
        // cache. They must not depend on the registry's hash map order,
        // which is random for every new registry.
        let (repo, prompts) = get_test_paths();
        let names = |tb: &ToolBox| -> Vec<String> {
            tb.get_declarations_generic()
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        let first = names(&ToolBox::new(repo.clone(), Some(prompts.clone())));
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "declarations are not in name order");
        assert_eq!(first, names(&ToolBox::new(repo, Some(prompts))));
    }
}

#[test]
fn test_mcp_stage_names_match_numbered_instances() {
    use super::stage_matches;
    assert!(stage_matches("hardware", "hardware"));
    assert!(stage_matches("post-verification-2", "post-verification"));
    assert!(!stage_matches("post-verification-x", "post-verification"));
    assert!(!stage_matches("post-verification-", "post-verification"));
    assert!(!stage_matches("hardware2", "hardware"));
    assert!(!stage_matches("verification", "post-verification"));
}

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
    use crate::worker::tools::ToolBox;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::runtime::Runtime;

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        // Use current repo as the test repo
        let linux_path = root.clone();
        let prompts_path = root.join("third_party/prompts/kernel");
        (linux_path, prompts_path)
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
        let lines_count = content.lines().count();
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
        let lines_count = content.lines().count();
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

    #[test]
    fn test_git_read_files_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "src/worker/prompts.rs" }
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

        // We expect actual_lines to match returned_items, or at least be extremely close (including warning lines).
        // Currently, returned_items is slice.len() (2448), but content only has allowed_lines (~800) lines!
        println!("returned_items metadata: {}", returned_items);
        println!("actual returned content lines: {}", actual_lines);

        assert!(
            actual_lines < 2400,
            "Content was not truncated! (should be around 800 lines)"
        );
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
            "object": "HEAD:src/worker/prompts.rs",
            "start_line": 1,
            "end_line": 3000
        });

        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        assert_eq!(result["truncated"].as_bool(), Some(true));

        let content = result["content"].as_str().unwrap();
        let returned_items = result["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("git_show returned_items metadata: {}", returned_items);
        println!("git_show actual returned content lines: {}", actual_lines);

        assert!(
            actual_lines < 2400,
            "Content was not truncated! (should be around 800 lines)"
        );
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "git_show returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );
    }
}

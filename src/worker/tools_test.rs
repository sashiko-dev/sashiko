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
        let linux_path = root.join("third_party/linux");
        let prompts_path = root.join("third_party/review-prompts/kernel");
        (linux_path, prompts_path)
    }

    #[test]
    fn test_list_dir_linux() {
        crate::setup_test_tracing();
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "path": "." });
        let result = rt.block_on(toolbox.call("list_dir", args)).unwrap();
        let entries = result["entries"].as_array().unwrap();

        assert!(entries.iter().any(|e| e["name"] == "README"));
        assert!(entries.iter().any(|e| e["name"] == "Makefile"));
    }

    #[test]
    fn test_read_files_linux_readme() {
        crate::setup_test_tracing();
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "files": [
                { "path": "README", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let content = results[0]["content"].as_str().unwrap();

        assert!(!content.is_empty());
        assert!(content.contains("Linux kernel"));
    }

    #[test]
    fn test_git_show_head() {
        crate::setup_test_tracing();
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
    fn test_git_blame_readme() {
        crate::setup_test_tracing();
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "path": "README", "start_line": 1, "end_line": 3 });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        // Typical git blame output starts with hash or (
        // e.g. ^1da177e4c3f (Linus Torvalds 2005-04-16 15:20:36 -0700 1) Linux kernel release 2.6.xx
    }

    #[test]
    fn test_write_file() {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        crate::setup_test_tracing();
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_path = temp_dir.path().to_path_buf();
        let _prompts_path = root.join("third_party/review-prompts/kernel");
        let toolbox = ToolBox::new(worktree_path.clone(), None);

        let rt = Runtime::new().unwrap();

        let filename = "review-inline.txt";
        let content = "commit 123456\nAuthor: Test User\n\nSubject";
        let args = json!({ "path": filename, "content": content });

        let result = rt.block_on(toolbox.call("write_file", args)).unwrap();
        assert_eq!(result["status"], "success");

        let written_content = std::fs::read_to_string(worktree_path.join(filename)).unwrap();
        assert_eq!(written_content, content);
    }

    #[test]
    fn test_write_file_invalid_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_path = temp_dir.path().to_path_buf();
        let toolbox = ToolBox::new(worktree_path, None);

        let rt = Runtime::new().unwrap();

        let filename = "review-inline.txt";
        let content = "Hello, world!";
        let args = json!({ "path": filename, "content": content });

        let result = rt.block_on(toolbox.call("write_file", args));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Please check `inline-template.md` for the correct output format.")
        );
    }

    #[test]
    fn test_write_file_overwrite() {
        crate::setup_test_tracing();
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_path = temp_dir.path().to_path_buf();
        let toolbox = ToolBox::new(worktree_path.clone(), None);

        let rt = Runtime::new().unwrap();

        let filename = "review-inline.txt";
        let initial_content = "commit 111111\nAuthor: User1\n\nOld content";
        std::fs::write(worktree_path.join(filename), initial_content).unwrap();

        let new_content = "commit 222222\nAuthor: User2\n\nNew overwritten content";
        let args = json!({ "path": filename, "content": new_content });

        let result = rt.block_on(toolbox.call("write_file", args)).unwrap();
        assert_eq!(result["status"], "success");

        let written_content = std::fs::read_to_string(worktree_path.join(filename)).unwrap();
        assert_eq!(written_content, new_content);
    }

    #[test]
    fn test_write_file_forbidden() {
        crate::setup_test_tracing();
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_path = temp_dir.path().to_path_buf();
        let toolbox = ToolBox::new(worktree_path, None);

        let rt = Runtime::new().unwrap();

        let filename = "forbidden.txt";
        let content = "This should fail";
        let args = json!({ "path": filename, "content": content });

        let result = rt.block_on(toolbox.call("write_file", args));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Permission denied")
        );
    }

    #[test]
    fn test_search_file_content_relative_path() {
        crate::setup_test_tracing();
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Search for "Linux kernel" which should be in README
        let args = json!({
            "pattern": "Linux kernel",
            "path": "."
        });

        let result = rt
            .block_on(toolbox.call("search_file_content", args))
            .unwrap();
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

        // Check if README matches are found (it might not be the first match)
        assert!(content.contains("README") || content.contains("./README"));
    }

    #[test]
    fn test_read_prompt() {
        let (linux_path, prompts_path) = get_test_paths();
        // Enable prompt tool by passing path
        let toolbox = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()));
        let rt = Runtime::new().unwrap();

        // Ensure we have a dummy prompt file to read
        // The real review-prompts might not be populated in test env, check first
        // Or assume review-core.md exists as per repo structure.
        // But tests might run in clean env. Let's create a dummy one if we can or check existence.
        // Since we are running in the actual repo, review-prompts should exist.

        let args = json!({ "name": "review-core.md" });
        if prompts_path.join("review-core.md").exists() {
            let result = rt
                .block_on(toolbox.call("read_prompt", args.clone()))
                .unwrap();
            let content = result["content"].as_str().unwrap();
            assert!(content.contains("Protocol"));
        } else {
            // If file doesn't exist (e.g. CI), skip assertion on content but check tool availability
            tracing::warn!("Skipping read_prompt content check: review-core.md not found");
        }

        // Test disabled tool
        let toolbox_disabled = ToolBox::new(linux_path, None);
        let result = rt.block_on(toolbox_disabled.call("read_prompt", args));
        assert!(result.is_err());
    }
}

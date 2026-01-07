#[cfg(test)]
mod tests {
    use crate::worker::tools::ToolBox;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::runtime::Runtime;

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let linux_path = root.join("linux");
        let prompts_path = root.join("review-prompts");
        (linux_path, prompts_path)
    }

    #[test]
    fn test_list_dir_linux() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "path": "." });
        let result = rt.block_on(toolbox.call("list_dir", args)).unwrap();
        let entries = result["entries"].as_array().unwrap();

        assert!(entries.iter().any(|e| e["name"] == "README"));
        assert!(entries.iter().any(|e| e["name"] == "Makefile"));
    }

    #[test]
    fn test_read_file_linux_readme() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "path": "README", "start_line": 1, "end_line": 5 });
        let result = rt.block_on(toolbox.call("read_file", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        assert!(content.contains("Linux kernel"));
    }

    #[test]
    fn test_git_show_head() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("commit"));
        assert!(content.contains("Author:"));
    }

    #[test]
    fn test_git_blame_readme() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path);
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
        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_path = temp_dir.path().to_path_buf();
        let _prompts_path = root.join("review-prompts");
        let toolbox = ToolBox::new(worktree_path.clone());

        let rt = Runtime::new().unwrap();

        let filename = "test-write.txt";
        let content = "Hello, world!";
        let args = json!({ "path": filename, "content": content });

        let result = rt.block_on(toolbox.call("write_file", args)).unwrap();
        assert_eq!(result["status"], "success");

        let written_content = std::fs::read_to_string(worktree_path.join(filename)).unwrap();
        assert_eq!(written_content, content);
    }
}

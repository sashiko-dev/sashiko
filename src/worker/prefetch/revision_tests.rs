use super::*;
use std::fs;

struct Fixture {
    repo: tempfile::TempDir,
    base: String,
    target: String,
    later: String,
    diff: String,
    later_diff: String,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit(repo: &Path) -> String {
    git(repo, &["add", "--all"]);
    git(
        repo,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"],
    );
    git(repo, &["rev-parse", "HEAD"])
}

impl Fixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "-q"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@example.com"]);
        fs::create_dir(path.join("include")).unwrap();
        fs::write(
            path.join("driver.c"),
            "int helper(int value);\n\nint consume(struct Packet *packet)\n{\n    return helper(10);\n}\n",
        ).unwrap();
        fs::write(
            path.join("include/packet.h"),
            "struct Packet {\n    int value;\n};\n",
        )
        .unwrap();
        // A colon in a filename exercises revision-prefix and filename parsing.
        fs::write(
            path.join("helper:impl.c"),
            "int helper(int value)\n{\n    return value + 100;\n}\n",
        )
        .unwrap();
        fs::write(path.join("deleted.c"), "int deleted(void) { return 9; }\n").unwrap();
        fs::write(path.join("old.c"), "int renamed(void) { return 11; }\n").unwrap();
        let base = commit(path);

        fs::write(
            path.join("driver.c"),
            "int helper(int value);\n\nint consume(struct Packet *packet)\n{\n    return packet->value + helper(10);\n}\n",
        ).unwrap();
        fs::write(path.join("added.c"), "int added(void) { return 42; }\n").unwrap();
        fs::remove_file(path.join("deleted.c")).unwrap();
        fs::rename(path.join("old.c"), path.join("renamed.c")).unwrap();
        let target = commit(path);
        let diff = git(path, &["show", "--format=", &target]);

        fs::write(
            path.join("driver.c"),
            "int helper(int value);\n\nint consume(struct Packet *packet)\n{\n    if (!packet) return 0;\n    return packet->later + helper(10);\n}\n",
        ).unwrap();
        fs::write(
            path.join("include/packet.h"),
            "struct Packet {\n    int later;\n};\n",
        )
        .unwrap();
        fs::write(
            path.join("helper:impl.c"),
            "int helper(int value)\n{\n    return value + 200;\n}\n",
        )
        .unwrap();
        let later = commit(path);
        let later_diff = git(path, &["show", "--format=", &later]);
        Self {
            repo,
            base,
            target,
            later,
            diff,
            later_diff,
        }
    }

    async fn context(&self) -> String {
        prefetch_context(self.repo.path(), &self.target, &self.diff)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn target_context_is_independent_of_checkout_and_dirty_files() {
    let fixture = Fixture::new();
    let path = fixture.repo.path();
    let expected = fixture.context().await;
    assert!(expected.starts_with(&format!("Source revision: {}\n", fixture.target)));
    assert!(expected.contains("return packet->value + helper(10);"));
    assert!(expected.contains("int value;"));
    assert!(expected.contains("return value + 100;"));
    assert!(expected.contains("int added(void)"));
    assert!(!expected.contains("if (!packet)"));
    assert!(!expected.contains("int later;"));
    assert!(!expected.contains("return value + 200;"));
    assert!(!expected.contains("int deleted(void)"));

    for revision in [&fixture.base, &fixture.target, &fixture.later] {
        git(path, &["checkout", "-q", "--detach", revision]);
        assert_eq!(fixture.context().await, expected, "checkout {revision}");
    }

    git(path, &["checkout", "-q", "--detach", &fixture.target]);
    fs::write(
        path.join("driver.c"),
        "int unrelated(void) { return 777; }\n",
    )
    .unwrap();
    fs::write(
        path.join("helper:impl.c"),
        "int helper(int n) { return 999; }\n",
    )
    .unwrap();
    git(path, &["add", "driver.c", "helper:impl.c"]);
    fs::remove_file(path.join("include/packet.h")).unwrap();
    fs::remove_file(path.join("added.c")).unwrap();
    assert_eq!(fixture.context().await, expected, "dirty files and index");
    fs::remove_file(path.join("driver.c")).unwrap();
    fs::remove_file(path.join("helper:impl.c")).unwrap();
    assert_eq!(fixture.context().await, expected, "missing working files");
}

#[tokio::test]
async fn concurrent_revisions_keep_separate_source_and_provenance() {
    let fixture = Fixture::new();
    let path = fixture.repo.path();
    let head = git(path, &["rev-parse", "HEAD"]);
    let status = git(path, &["status", "--porcelain"]);
    let (target, later) = tokio::join!(
        prefetch_context(path, &fixture.target, &fixture.diff),
        prefetch_context(path, &fixture.later, &fixture.later_diff),
    );
    let target = target.unwrap();
    let later = later.unwrap();
    assert!(target.contains("return value + 100;"));
    assert!(!target.contains("if (!packet)"));
    assert!(later.contains("if (!packet)"));
    assert!(later.contains("return value + 200;"));
    assert!(target.starts_with(&format!("Source revision: {}\n", fixture.target)));
    assert!(later.starts_with(&format!("Source revision: {}\n", fixture.later)));
    assert_eq!(git(path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(path, &["status", "--porcelain"]), status);
}

#[tokio::test]
async fn absent_objects_fail_without_using_working_files() {
    let fixture = Fixture::new();
    let path = fixture.repo.path();
    for invalid in ["HEAD", "unknown", "--all", &"0".repeat(40)] {
        assert!(
            prefetch_context(path, invalid, &fixture.diff)
                .await
                .is_err()
        );
    }
    // added.c exists locally at the later commit, but not in the baseline.
    let diff = "+++ b/added.c\n@@ -0,0 +1 @@\n+int added(void) { return 42; }\n";
    assert!(prefetch_context(path, &fixture.base, diff).await.is_err());
}

#[tokio::test]
async fn deleted_hunks_do_not_extend_the_previous_files_ranges() {
    let fixture = Fixture::new();
    let diff = "diff --git a/driver.c b/driver.c\n+++ b/driver.c\n@@ -1 +1 @@\n-int old;\n+int helper(int value);\ndiff --git a/deleted.c b/deleted.c\n+++ /dev/null\n@@ -1,9 +1,2 @@\n-old\n";
    let ranges = parse_diff_ranges(diff);
    assert_eq!(ranges["driver.c"], vec![(0, 0)]);
    let deleted_only = "diff --git a/deleted.c b/deleted.c\n+++ /dev/null\n@@ -1 +0,0 @@\n-int deleted(void) { return 9; }\n";
    assert!(
        prefetch_context(fixture.repo.path(), &fixture.target, deleted_only)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn renamed_postimage_is_read_even_when_checkout_has_old_name() {
    let fixture = Fixture::new();
    let path = fixture.repo.path();
    git(path, &["checkout", "-q", "--detach", &fixture.base]);
    // Model a rename with an edited hunk; pure renames require no source extraction.
    let diff = "diff --git a/old.c b/renamed.c\n--- a/old.c\n+++ b/renamed.c\n@@ -1 +1 @@\n-int renamed(void) { return 10; }\n+int renamed(void) { return 11; }\n";
    let context = prefetch_context(path, &fixture.target, diff).await.unwrap();
    assert!(context.contains("--- renamed.c:1 (renamed) ---"));
    assert!(context.contains("return 11;"));
    assert!(!path.join("renamed.c").exists());
}

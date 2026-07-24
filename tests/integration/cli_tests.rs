use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn get_bin_path(name: &str) -> PathBuf {
    if let Ok(path) = env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }

    // Fallback for local run
    let path = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    // We can't easily know if the binary was built with release or debug from here
    // if we are running under cargo test. But we can check which one exists.
    let executable = format!("{name}{}", env::consts::EXE_SUFFIX);
    let release_path = path.join("release").join(&executable);
    let debug_path = path.join("debug").join(&executable);

    if release_path.exists() {
        release_path
    } else if debug_path.exists() {
        debug_path
    } else {
        panic!("Could not find {name} binary in target/release or target/debug. Path: {path:?}");
    }
}

fn test_repo_with_root_commit() -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("failed to create temporary repository");
    let repo_path = temp_dir.path();

    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test User"],
    ] {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()
            .expect("failed to execute git");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::write(repo_path.join("root.txt"), "root commit\n").expect("failed to write test file");
    for args in [vec!["add", "root.txt"], vec!["commit", "-m", "root commit"]] {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()
            .expect("failed to execute git");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    temp_dir
}

fn run_local_review(repo_path: &std::path::Path) -> std::process::Output {
    Command::new(get_bin_path("sashiko-cli"))
        .args(["--color", "never", "local", "HEAD", "--repo"])
        .arg(repo_path)
        .args(["--no-ai", "--force-local"])
        .output()
        .expect("failed to execute sashiko-cli")
}

#[test]
fn test_local_review_accepts_root_commit() {
    let temp_dir = test_repo_with_root_commit();
    let output = run_local_review(temp_dir.path());

    assert!(
        output.status.success(),
        "root commit review failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("(pre-applied)"),
        "root commit was not reviewed through the direct-commit path\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_local_review_still_applies_non_root_commit() {
    let temp_dir = test_repo_with_root_commit();
    let repo_path = temp_dir.path();
    fs::write(repo_path.join("root.txt"), "second commit\n").expect("failed to update test file");
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["commit", "-am", "second commit"])
        .output()
        .expect("failed to execute git");
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_local_review(repo_path);
    assert!(
        output.status.success(),
        "non-root commit review failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("(git-am)") || stdout.contains("(checkout)"),
        "non-root commit did not use normal patch application\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("(pre-applied)"),
        "non-root commit incorrectly used the root-commit path\nstdout:\n{stdout}"
    );
}

#[test]
fn test_review_subcommand_hides_info_logs() {
    let bin_path = get_bin_path("sashiko");

    let output = Command::new(&bin_path)
        .args(["review", "HEAD", "--no-ai"])
        .output()
        .expect("Failed to execute sashiko binary");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("INFO"),
        "stderr contains INFO logs: {}",
        stderr
    );
    assert!(
        !stderr.contains("Skipping AI review"),
        "stderr contains info log message: {}",
        stderr
    );
    assert!(
        stderr.contains("Reviewing: HEAD"),
        "stderr missing 'Reviewing: HEAD': {}",
        stderr
    );
}

#[test]
fn test_review_subcommand_shows_info_logs_with_debug() {
    let bin_path = get_bin_path("sashiko");

    let output = Command::new(&bin_path)
        .args(["--debug", "review", "HEAD", "--no-ai"])
        .output()
        .expect("Failed to execute sashiko binary");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("INFO"),
        "stderr missing INFO logs in debug mode: {}",
        stderr
    );
    assert!(
        stderr.contains("Skipping AI review"),
        "stderr missing info log message in debug mode: {}",
        stderr
    );
}

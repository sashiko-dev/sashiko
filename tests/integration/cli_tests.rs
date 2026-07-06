use std::env;
use std::path::PathBuf;
use std::process::Command;

fn get_bin_path() -> PathBuf {
    if let Ok(path) = env::var("CARGO_BIN_EXE_sashiko") {
        return PathBuf::from(path);
    }

    // Fallback for local run
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    // We can't easily know if the binary was built with release or debug from here
    // if we are running under cargo test. But we can check which one exists.
    let release_path = path.join("release").join("sashiko");
    let debug_path = path.join("debug").join("sashiko");

    if release_path.exists() {
        release_path
    } else if debug_path.exists() {
        debug_path
    } else {
        panic!(
            "Could not find sashiko binary in target/release or target/debug. Path: {:?}",
            path
        );
    }
}

#[test]
fn test_review_subcommand_hides_info_logs() {
    let bin_path = get_bin_path();

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
    let bin_path = get_bin_path();

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

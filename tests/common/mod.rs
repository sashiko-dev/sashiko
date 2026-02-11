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
#![allow(dead_code)]

use sashiko::settings::Settings;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::{TempDir, tempdir};

pub mod trace_replayer;

#[allow(dead_code)]
pub struct MockEnv {
    pub base_dir: TempDir,
    pub remote_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub git_config: PathBuf,
    pub settings_file: PathBuf,
    pub settings: Settings,
}

impl MockEnv {
    pub async fn setup() -> Self {
        let base_dir = tempdir().expect("Failed to create temp dir");
        let base_path = base_dir.path().to_path_buf();

        let remote_dir = base_path.join("mock-remote");
        let cache_dir = base_path.join("cache");
        let worktree_dir = base_path.join("worktrees");
        let archives_dir = base_path.join("archives");
        let prompts_base_dir = base_path.join("prompts");
        let prompts_dir = prompts_base_dir.join("kernel");
        let static_dir = std::env::current_dir().unwrap().join("static");
        let db_path = base_path.join("test.db");
        let git_config = base_path.join("gitconfig");
        let settings_file = base_path.join("Settings.toml");

        fs::create_dir_all(&remote_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::create_dir_all(&archives_dir).unwrap();
        fs::create_dir_all(&prompts_dir).unwrap();

        // 1. Initialize Mock Remote
        Self::git_exec(&remote_dir, &["init", "-q", "-b", "master"]);
        Self::git_exec(&remote_dir, &["config", "user.email", "test@example.com"]);
        Self::git_exec(&remote_dir, &["config", "user.name", "Test User"]);

        fs::write(remote_dir.join("main.c"), "int main() { return 0; }\n").unwrap();
        Self::git_exec(&remote_dir, &["add", "main.c"]);

        // Use fixed dates for determinism
        let envs = [
            ("GIT_AUTHOR_DATE", "2026-02-06T12:00:00Z"),
            ("GIT_COMMITTER_DATE", "2026-02-06T12:00:00Z"),
        ];
        Self::git_exec_with_env(
            &remote_dir,
            &["commit", "-q", "-m", "Initial commit"],
            &envs,
        );

        // 2. Initialize Cache (Bare)
        Self::git_exec(&cache_dir, &["init", "-q", "--bare", "-b", "master"]);

        // 3. Generate Git Config
        let git_config_content = r#"[safe]
    directory = *
    bareRepository = all
[user]
    name = Sashiko Test
    email = sashiko@test.local
"#;
        fs::write(&git_config, git_config_content).unwrap();

        // 4. Find a free port
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to random port");
            listener.local_addr().unwrap().port()
        };

        // 5. Create Settings
        let settings_content = format!(
            r#"
log_level = "info"

[database]
url = "{}"
token = ""

[mailing_lists]
track = ["linux-kernel"]

[nntp]
server = "localhost"
port = 119

[ai]
provider = "gemini"
model = "gemini-pro"
max_input_tokens = 200000
max_interactions = 100
temperature = 0.0
explicit_prompts_caching = false

[server]
host = "127.0.0.1"
port = {}
static_dir = "{}"

[git]
repository_path = "{}"
archives_dir = "{}"

[review]
worktree_dir = "{}"
prompts_dir = "{}"
concurrency = 1
timeout_seconds = 300
max_retries = 3
poll_interval = 1
"#,
            db_path.to_str().unwrap(),
            port,
            static_dir.to_str().unwrap(),
            cache_dir.to_str().unwrap(),
            archives_dir.to_str().unwrap(),
            worktree_dir.to_str().unwrap(),
            prompts_dir.to_str().unwrap()
        );
        fs::write(&settings_file, settings_content).unwrap();

        // We deserialize it just to have it in the struct if needed
        let settings: Settings = Config::builder()
            .add_source(config::File::from(settings_file.clone()))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();

        Self {
            base_dir,
            remote_dir,
            prompts_dir,
            git_config,
            settings_file,
            settings,
        }
    }

    fn git_exec(dir: &PathBuf, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("Failed to execute git");
        assert!(
            output.status.success(),
            "Git command failed: {:?}\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_exec_with_env(dir: &PathBuf, args: &[&str], envs: &[(&str, &str)]) {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(args);
        for (key, val) in envs {
            cmd.env(key, val);
        }
        let output = cmd.output().expect("Failed to execute git");
        assert!(
            output.status.success(),
            "Git command failed: {:?}\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub fn get_remote_url(&self) -> String {
        format!("file://{}", self.remote_dir.to_str().unwrap())
    }

    pub fn get_head_sha(&self) -> String {
        let output = Command::new("git")
            .current_dir(&self.remote_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("Failed to get HEAD SHA");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

#[allow(dead_code)]
pub struct SashikoProcess {
    child: Child,
    port: u16,
    pub logs: Arc<Mutex<String>>,
}

impl SashikoProcess {
    pub fn spawn(env: &MockEnv, binary_path: &str, extra_envs: Vec<(String, String)>) -> Self {
        let mut cmd = Command::new(binary_path);
        cmd.current_dir(env.base_dir.path())
            .arg("--api")
            .arg("--debug")
            .env("GIT_CONFIG_GLOBAL", &env.git_config)
            .env("SASHIKO_CONFIG", &env.settings_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (key, val) in extra_envs {
            cmd.env(key, val);
        }

        let mut child = cmd.spawn().expect("Failed to spawn sashiko process");
        let logs = Arc::new(Mutex::new(String::new()));

        let stdout = child.stdout.take().expect("Failed to open stdout");
        let stderr = child.stderr.take().expect("Failed to open stderr");
        let logs_clone = logs.clone();

        thread::spawn(move || {
            let mut reader = stdout.chain(stderr);
            let mut buf = [0; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let s = String::from_utf8_lossy(&buf[..n]);
                let mut l = logs_clone.lock().unwrap();
                l.push_str(&s);
            }
        });

        Self {
            child,
            port: env.settings.server.port,
            logs,
        }
    }

    pub async fn wait_ready(&self) {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/api/patchsets", self.port);
        let start = Instant::now();
        let timeout = Duration::from_secs(10);

        while start.elapsed() < timeout {
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        panic!("Sashiko process failed to become ready at {}", url);
    }
}

impl Drop for SashikoProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let logs = self.logs.lock().unwrap();
        if !logs.is_empty() {
            println!(
                "\n--- Sashiko Process Logs ---\n{}\n----------------------------\n",
                logs
            );
        }
    }
}

use config::Config;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;

pub fn setup_tracing() {
    sashiko::setup_test_tracing();
}

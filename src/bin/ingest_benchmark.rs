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

use clap::Parser;
use reqwest::Client;
use sashiko::settings::Settings;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the benchmark file
    #[arg(short, long)]
    file: String,

    /// Override the default port (reads from settings by default)
    #[arg(short, long)]
    port: Option<u16>,

    /// Override the default repo URL (default: kernel.org linux.git)
    #[arg(short, long)]
    repo: Option<String>,
}

#[derive(Deserialize)]
struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    commit: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SubmitRequest {
    Remote { sha: String, repo: String },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Load settings to get the default port
    let settings = Settings::new()?;
    let port = args.port.unwrap_or(settings.server.port);

    let file = File::open(&args.file)?;
    let reader = BufReader::new(file);
    let entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)?;

    let client = Client::new();
    let repo_url = args.repo.as_deref()
        .unwrap_or("https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git");

    println!("Found {} entries to process", entries.len());

    let target_url = format!("http://127.0.0.1:{}/api/submit", port);

    for entry in entries {
        println!("Processing commit: {}", entry.commit);

        // Submit to API
        let payload = SubmitRequest::Remote {
            sha: entry.commit.clone(),
            repo: repo_url.to_string(),
        };

        let res = client.post(&target_url).json(&payload).send().await;

        match res {
            Ok(response) => {
                if response.status().is_success() {
                    println!("Successfully submitted {}", entry.commit);
                } else {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    eprintln!(
                        "Failed to submit {}: Status {} Body: {}",
                        entry.commit, status, text
                    );
                }
            }
            Err(e) => {
                eprintln!("Failed to send request for {}: {}", entry.commit, e);
            }
        }
    }

    Ok(())
}

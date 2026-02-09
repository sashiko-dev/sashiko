use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use reqwest::Client;

#[derive(Deserialize)]
struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    commit: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SubmitRequest {
    Remote {
        sha: String,
        repo: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open("benchmark.json")?;
    let reader = BufReader::new(file);
    let entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)?;

    let client = Client::new();
    let repo_url = "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";

    println!("Found {} entries to process", entries.len());

    for entry in entries {
        println!("Processing commit: {}", entry.commit);

        // Submit to API
        let payload = SubmitRequest::Remote {
            sha: entry.commit.clone(),
            repo: repo_url.to_string(),
        };

        let res = client.post("http://127.0.0.1:8080/api/submit")
            .json(&payload)
            .send()
            .await;

        match res {
            Ok(response) => {
                if response.status().is_success() {
                    println!("Successfully submitted {}", entry.commit);
                } else {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    eprintln!("Failed to submit {}: Status {} Body: {}", entry.commit, status, text);
                }
            }
            Err(e) => {
                eprintln!("Failed to send request for {}: {}", entry.commit, e);
            }
        }
    }

    Ok(())
}

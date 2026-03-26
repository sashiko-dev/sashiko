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

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::Client;
use sashiko::api::{PatchsetsResponse, SubmitRequest, SubmitResponse};
use serde_json::Value;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Parser)]
#[command(name = "sashiko-cli")]
#[command(about = "CLI tool for interacting with Sashiko", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Override server URL (default: from settings or http://127.0.0.1:8080)
    #[arg(
        long,
        global = true,
        env = "SASHIKO_SERVER",
        default_value = "http://127.0.0.1:8080"
    )]
    server: Option<String>,

    /// Output format (text, json)
    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit a patch or range for review
    Submit {
        /// Revision range, commit SHA, or path to mbox file.
        /// Defaults to "HEAD" if in a git repo, or reads from stdin if piped.
        #[arg(value_name = "INPUT")]
        input: Option<String>,

        /// Explicitly set type (overrides auto-detection)
        #[arg(long, value_enum)]
        r#type: Option<SubmitType>,

        /// Override repository path (defaults to settings)
        #[arg(long, short = 'r')]
        repo: Option<PathBuf>,

        /// Baseline commit (for mbox injection only)
        #[arg(long)]
        baseline: Option<String>,

        /// Skip review for patches matching subject pattern (with wildcards, e.g. mm:*)
        #[arg(long, value_name = "PATTERN")]
        skip_subject: Option<Vec<String>>,

        /// Only review patches matching subject pattern (with wildcards, e.g. *PRODKERNEL*)
        #[arg(long, value_name = "PATTERN")]
        only_subject: Option<Vec<String>>,
    },
    /// Show server status and statistics
    Status,
    /// List patchsets or reviews
    List {
        /// Filter query (e.g. "pending", "failed", "linux-mm")
        #[arg(value_name = "FILTER")]
        filter: Option<String>,

        /// Page number
        #[arg(long, default_value_t = 1)]
        page: usize,

        /// Items per page
        #[arg(long, default_value_t = 20)]
        per_page: usize,
    },
    /// Show details of a patchset or review
    Show {
        /// ID of the patchset or "latest"
        #[arg(default_value = "latest")]
        id: String,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum SubmitType {
    /// Submit a raw mbox file (or - for stdin)
    Mbox,
    /// Submit a single remote commit
    Remote,
    /// Submit a range of remote commits
    Range,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let base_url = cli.server.unwrap();

    let client = Client::new();

    if let Err(e) = run_command(cli.command, &client, &base_url, cli.format).await {
        print_colored(Color::Red, "Error: ");
        println!("{}", e);

        // Provide helpful hints for common errors
        if let Some(req_err) = e.downcast_ref::<reqwest::Error>() {
            if req_err.is_connect() {
                println!("\nHint: Is the Sashiko server running at {}?", base_url);
                println!("      You can start it with `cargo run --bin sashiko`");
            } else if let Some(status) = req_err.status() {
                if status == reqwest::StatusCode::NOT_FOUND {
                    println!("\nHint: The requested resource was not found.");
                } else if status == reqwest::StatusCode::BAD_REQUEST {
                    println!("\nHint: The request was invalid. Check your arguments.");
                }
            }
        }
        std::process::exit(1);
    }

    Ok(())
}

async fn run_command(
    command: Commands,
    client: &Client,
    base_url: &str,
    format: OutputFormat,
) -> Result<()> {
    match command {
        Commands::Submit {
            input,
            r#type,
            repo,
            baseline,
            skip_subject,
            only_subject,
        } => {
            handle_submit(
                client,
                base_url,
                input,
                r#type,
                repo,
                baseline,
                skip_subject,
                only_subject,
                format,
            )
            .await
        }
        Commands::Status => handle_status(client, base_url, format).await,
        Commands::List {
            filter,
            page,
            per_page,
        } => handle_list(client, base_url, page, per_page, filter, format).await,
        Commands::Show { id } => handle_show(client, base_url, id, format).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_submit(
    client: &Client,
    base_url: &str,
    input: Option<String>,
    explicit_type: Option<SubmitType>,
    repo: Option<PathBuf>,
    baseline: Option<String>,
    skip_subjects: Option<Vec<String>>,
    only_subjects: Option<Vec<String>>,
    format: OutputFormat,
) -> Result<()> {
    let url = format!("{}/api/submit", base_url);

    // DWIM Detection Logic
    let (submission_type, target) = if let Some(t) = explicit_type {
        (t, input.unwrap_or_else(|| "HEAD".to_string()))
    } else {
        // Auto-detect based on input
        if let Some(s) = input {
            if s == "-" {
                (SubmitType::Mbox, s)
            } else if s.contains("..") {
                (SubmitType::Range, s)
            } else if PathBuf::from(&s).exists() {
                // If it's a file, assume mbox. If it's a dir, maybe repo?
                // For safety, if it looks like a commit (hex), prefer Remote unless file exists.
                // But filenames can look like anything.
                // Sashiko deals with mbox files primarily.
                let p = PathBuf::from(&s);
                if p.is_file() {
                    (SubmitType::Mbox, s)
                } else {
                    // Not a file, assume commit ref
                    (SubmitType::Remote, s)
                }
            } else {
                // Not a file on disk (or we can't see it). Assume commit ref.
                (SubmitType::Remote, s)
            }
        } else {
            // No input provided.
            // Check if stdin is piped
            if !std::io::stdin().is_terminal() {
                (SubmitType::Mbox, "-".to_string())
            } else {
                // Default to HEAD
                (SubmitType::Remote, "HEAD".to_string())
            }
        }
    };

    let payload = match submission_type {
        SubmitType::Mbox => {
            let content = if target == "-" {
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .context("Failed to read from stdin")?;
                buffer
            } else {
                std::fs::read_to_string(&target)
                    .with_context(|| format!("Failed to read file {:?}", target))?
            };
            SubmitRequest::Inject {
                raw: content,
                base_commit: baseline,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
            }
        }
        SubmitType::Remote => {
            let repo_path = repo.map(|p| p.to_string_lossy().to_string());

            SubmitRequest::Remote {
                sha: target,
                repo: repo_path,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
            }
        }
        SubmitType::Range => {
            let repo_path = repo.map(|p| p.to_string_lossy().to_string());

            SubmitRequest::RemoteRange {
                sha: target,
                repo: repo_path,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
            }
        }
    };

    let resp = client.post(&url).json(&payload).send().await?;

    if resp.status().is_success() {
        let result: SubmitResponse = resp.json().await?;
        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            OutputFormat::Text => {
                print_colored(Color::Green, "Success: ");
                println!("Submission accepted. ID: {}", result.id);
            }
        }
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Submission failed ({}): {}", status, text));
    }

    Ok(())
}

async fn handle_status(client: &Client, base_url: &str, format: OutputFormat) -> Result<()> {
    let url = format!("{}/api/stats", base_url);
    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        let stats: Value = resp.json().await?;

        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&stats)?),
            OutputFormat::Text => {
                print_colored(Color::Cyan, "Server Status:\n");
                println!(
                    "  Version:   {}",
                    stats["version"].as_str().unwrap_or("unknown")
                );
                println!("  Messages:  {}", stats["messages"]);
                println!("  Patchsets: {}", stats["patchsets"]);

                if let Some(breakdown) = stats.get("breakdown") {
                    println!("\nQueue Breakdown:");
                    let items = [
                        ("Pending", "pending"),
                        ("In Review", "reviewing"),
                        ("Reviewed", "reviewed"),
                        ("Failed", "failed"),
                        ("Apply Failed", "failed_to_apply"),
                        ("Incomplete", "incomplete"),
                    ];

                    let zero = serde_json::json!(0);
                    for (label, key) in items {
                        let val = breakdown.get(key).unwrap_or(&zero);
                        println!("  {:<15} {}", label, val);
                    }
                }
            }
        }
    } else {
        return Err(anyhow::anyhow!("Failed to get status: {}", resp.status()));
    }

    Ok(())
}

async fn handle_list(
    client: &Client,
    base_url: &str,
    page: usize,
    per_page: usize,
    filter: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    let mut url = format!(
        "{}/api/patchsets?page={}&per_page={}",
        base_url, page, per_page
    );
    if let Some(q) = filter {
        url.push_str(&format!("&q={}", q));
    }

    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        let data: PatchsetsResponse = resp.json().await?;

        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&data)?),
            OutputFormat::Text => {
                if data.items.is_empty() {
                    println!("No items found.");
                    return Ok(());
                }

                println!(
                    "{:<10} {:<18} {:<50} {:<20}",
                    "ID", "Status", "Subject", "Date"
                );
                println!("{:-<10} {:-<18} {:-<50} {:-<20}", "", "", "", "");

                for item in data.items {
                    let status_str = item.status.as_deref().unwrap_or("Unknown");
                    let status_color = match status_str {
                        "Reviewed" => Color::Green,
                        "Failed" | "Error" | "Failed To Apply" => Color::Red,
                        "Pending" | "In Review" => Color::Yellow,
                        _ => Color::White,
                    };

                    print!("{:<10} ", item.id);
                    print_colored(status_color, &format!("{:<18}", status_str));

                    let subject = item.subject.unwrap_or_else(|| "(no subject)".to_string());
                    let subject_display = if subject.len() > 48 {
                        format!("{}...", &subject[..45])
                    } else {
                        subject
                    };

                    let date_display = if let Some(ts) = item.date {
                        format_timestamp(ts)
                    } else {
                        "-".to_string()
                    };

                    println!(" {:<50} {}", subject_display, date_display);
                }

                println!(
                    "\nPage {} of {} (Total: {})",
                    data.page,
                    data.total.div_ceil(data.per_page),
                    data.total
                );
            }
        }
    } else {
        return Err(anyhow::anyhow!(
            "Failed to list patchsets: {}",
            resp.status()
        ));
    }

    Ok(())
}

async fn handle_show(
    client: &Client,
    base_url: &str,
    mut id: String,
    format: OutputFormat,
) -> Result<()> {
    if id == "latest" {
        // Fetch list to find latest
        let list_url = format!("{}/api/patchsets?page=1&per_page=1", base_url);
        let resp = client.get(&list_url).send().await?;
        if resp.status().is_success() {
            let data: PatchsetsResponse = resp.json().await?;
            if let Some(latest) = data.items.first() {
                id = latest.id.to_string();
            } else {
                return Err(anyhow::anyhow!("No patchsets found"));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Failed to find latest patchset: {}",
                resp.status()
            ));
        }
    }

    let url = format!("{}/api/patch?id={}", base_url, id);
    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        let mut details: Value = resp.json().await?;
        let status = details["status"].as_str().unwrap_or("").to_string();

        // Fetch review if available
        let mut review_data = None;
        if status == "Reviewed" || status == "Failed" || status == "Failed To Apply" {
            let review_url = format!("{}/api/review?patchset_id={}", base_url, id);
            let review_resp = client.get(&review_url).send().await?;

            if review_resp.status().is_success() {
                review_data = Some(review_resp.json::<Value>().await?);
            }
        }

        match format {
            OutputFormat::Json => {
                if let Some(r) = review_data {
                    details["review"] = r;
                }
                println!("{}", serde_json::to_string_pretty(&details)?);
            }
            OutputFormat::Text => {
                print_colored(Color::Cyan, "Patchset Details:\n");
                println!("  ID:        {}", details["id"]);
                println!("  Subject:   {}", details["subject"].as_str().unwrap_or(""));
                println!("  Author:    {}", details["author"].as_str().unwrap_or(""));
                println!("  Status:    {}", details["status"].as_str().unwrap_or(""));

                if let Some(ts) = details["date"].as_i64() {
                    println!("  Date:      {}", format_timestamp(ts));
                }

                if let Some(reason) = details.get("failed_reason").and_then(|r| r.as_str()) {
                    print_colored(Color::Red, "\nFailure Reason: ");
                    println!("{}", reason);
                }

                if let Some(patches) = details.get("patches").and_then(|p| p.as_array()) {
                    println!("\nPatches ({}):", patches.len());
                    for patch in patches {
                        let idx = patch["part_index"].as_i64().unwrap_or(0);
                        let status = patch["status"].as_str().unwrap_or("");
                        let apply_err = patch["apply_error"].as_str();

                        print!("  [{}] {}", idx, patch["subject"].as_str().unwrap_or(""));
                        if !status.is_empty() && status != "Pending" {
                            print!(" (");
                            let color = if status == "Failed" {
                                Color::Red
                            } else {
                                Color::Green
                            };
                            print_colored(color, status);
                            print!(")");
                        }
                        println!();

                        if let Some(err) = apply_err {
                            print_colored(Color::Red, "      Error: ");
                            println!("{}", err.trim());
                        }
                    }
                }

                if let Some(review) = review_data {
                    println!("\nReview Summary:");
                    if let Some(verdict) = review.get("verdict").and_then(|v| v.as_str()) {
                        let color = match verdict {
                            "LGTM" => Color::Green,
                            "Request Changes" => Color::Red,
                            _ => Color::Yellow,
                        };
                        print!("  Verdict: ");
                        print_colored(color, verdict);
                        println!();
                    }

                    if let Some(model) = review.get("model").and_then(|m| m.as_str()) {
                        println!("  Model:   {}", model);
                    }

                    if let Some(summary) = review.get("summary").and_then(|s| s.as_str()) {
                        println!("\n{}", summary);
                    }

                    if let Some(logs) = review.get("logs").and_then(|l| l.as_str()) {
                        println!("\nLogs:\n{}", logs);
                    }
                } else if let Some(logs) = details.get("baseline_logs").and_then(|l| l.as_str()) {
                    // Fallback to baseline logs if review is missing (e.g. Failed To Apply during baseline prep)
                    if status == "Failed To Apply" {
                        println!("\nBaseline Logs:\n{}", logs);
                    }
                }
            }
        }
    } else {
        return Err(anyhow::anyhow!(
            "Failed to show patchset: {}",
            resp.status()
        ));
    }

    Ok(())
}

fn print_colored(color: Color, text: &str) {
    let mut stdout = StandardStream::stdout(ColorChoice::Auto);
    stdout
        .set_color(ColorSpec::new().set_fg(Some(color)))
        .unwrap();
    write!(&mut stdout, "{}", text).unwrap();
    stdout.reset().unwrap();
}

fn format_timestamp(ts: i64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    match Utc.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => {
            let local_dt: DateTime<Local> = DateTime::from(dt);
            local_dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        _ => ts.to_string(),
    }
}

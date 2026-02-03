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

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info};

pub struct NntpClient {
    stream: BufReader<TcpStream>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct GroupInfo {
    pub number: u64,
    pub low: u64,
    pub high: u64,
    pub name: String,
}

impl NntpClient {
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        info!("Connecting to NNTP server at {}", addr);
        let stream = TcpStream::connect(addr).await?;
        let mut reader = BufReader::new(stream);

        let mut buf = Vec::new();
        reader.read_until(b'\n', &mut buf).await?;
        let response = String::from_utf8_lossy(&buf).trim().to_string();

        if !response.starts_with("200") && !response.starts_with("201") {
            return Err(anyhow!("Unexpected welcome message: {}", response));
        }

        debug!("Connected: {}", response);
        Ok(Self { stream: reader })
    }

    async fn send_command(&mut self, command: &str) -> Result<()> {
        self.stream.write_all(command.as_bytes()).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        self.stream.read_until(b'\n', &mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).trim().to_string())
    }

    pub async fn group(&mut self, group_name: &str) -> Result<GroupInfo> {
        self.send_command(&format!("GROUP {}", group_name)).await?;
        let response = self.read_response().await?;

        if !response.starts_with("211") {
            return Err(anyhow!(
                "Failed to select group {}: {}",
                group_name,
                response
            ));
        }

        let parts: Vec<&str> = response.split_whitespace().collect();
        if parts.len() < 5 {
            return Err(anyhow!("Invalid GROUP response format: {}", response));
        }

        Ok(GroupInfo {
            number: parts[1].parse().unwrap_or(0),
            low: parts[2].parse().unwrap_or(0),
            high: parts[3].parse().unwrap_or(0),
            name: parts[4].to_string(),
        })
    }

    pub async fn article(&mut self, id: &str) -> Result<Vec<String>> {
        self.send_command(&format!("ARTICLE {}", id)).await?;
        let response = self.read_response().await?;

        if !response.starts_with("220") {
            return Err(anyhow!("Failed to retrieve article {}: {}", id, response));
        }

        let mut lines = Vec::new();
        loop {
            let mut buf = Vec::new();
            let n = self.stream.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                break; // EOF
            }

            // Convert to string (lossy)
            let line_raw = String::from_utf8_lossy(&buf);
            let line = line_raw.trim_end(); // remove \r\n

            if line == "." {
                break;
            }
            // Dot-unstuffing
            let content = if line.starts_with("..") {
                line[1..].to_string()
            } else {
                line.to_string()
            };
            lines.push(content);
        }

        Ok(lines)
    }

    pub async fn quit(&mut self) -> Result<()> {
        self.send_command("QUIT").await?;
        let response = self.read_response().await?;

        if !response.starts_with("205") {
            debug!("QUIT response was not 205: {}", response);
        }
        Ok(())
    }
}

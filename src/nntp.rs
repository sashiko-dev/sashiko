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
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct NntpClient {
    stream: BufReader<TcpStream>,
    timeout: Duration,
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
        let stream = timeout(DEFAULT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("Connection timed out"))??;
        let mut reader = BufReader::new(stream);

        let mut buf = Vec::new();
        timeout(DEFAULT_TIMEOUT, reader.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| anyhow!("Timeout reading welcome message"))??;
        let response = String::from_utf8_lossy(&buf).trim().to_string();

        if !response.starts_with("200") && !response.starts_with("201") {
            return Err(anyhow!("Unexpected welcome message: {}", response));
        }

        debug!("Connected: {}", response);
        Ok(Self {
            stream: reader,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    async fn read_line_with_timeout(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        timeout(self.timeout, self.stream.read_until(b'\n', buf))
            .await
            .map_err(|_| anyhow!("Read timed out"))?
            .map_err(|e| e.into())
    }

    async fn write_all_with_timeout(&mut self, bytes: &[u8]) -> Result<()> {
        timeout(self.timeout, self.stream.write_all(bytes))
            .await
            .map_err(|_| anyhow!("Write timed out"))?
            .map_err(|e| e.into())
    }

    async fn send_command(&mut self, command: &str) -> Result<()> {
        self.write_all_with_timeout(command.as_bytes()).await?;
        self.write_all_with_timeout(b"\r\n").await?;
        timeout(self.timeout, self.stream.flush())
            .await
            .map_err(|_| anyhow!("Flush timed out"))??;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        self.read_line_with_timeout(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).trim().to_string())
    }

    pub async fn list(&mut self) -> Result<Vec<String>> {
        self.send_command("LIST").await?;
        let response = self.read_response().await?;

        if !response.starts_with("215") {
            return Err(anyhow!("Failed to retrieve list: {}", response));
        }

        let mut groups = Vec::new();
        loop {
            let mut buf = Vec::new();
            let n = self.read_line_with_timeout(&mut buf).await?;
            if n == 0 {
                break; // EOF
            }

            let line_raw = String::from_utf8_lossy(&buf);
            let line = line_raw.trim_end_matches(['\r', '\n']);

            if line == "." {
                break;
            }

            if let Some(group) = line.split_whitespace().next() {
                groups.push(group.to_string());
            }
        }

        Ok(groups)
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

        if parts[4] != group_name {
            return Err(anyhow!(
                "Mismatched GROUP response: expected {}, got {}",
                group_name,
                parts[4]
            ));
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
            let n = self.read_line_with_timeout(&mut buf).await?;
            if n == 0 {
                break; // EOF
            }

            // Convert to string (lossy)
            let line_raw = String::from_utf8_lossy(&buf);
            let line = line_raw.trim_end_matches(['\r', '\n']);

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn article_preserves_payload_whitespace_and_framing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);

            stream
                .get_mut()
                .write_all(b"200 mock server ready\r\n")
                .await
                .unwrap();

            let mut command = Vec::new();
            stream.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"ARTICLE <issue-320@example.test>\r\n");

            stream
                .get_mut()
                .write_all(
                    b"220 article follows\r\n\
normal text\r\n\
trailing space \r\n\
trailing tab\t\r\n\
\x20\r\n\
\r\n\
..payload\r\n\
+added \t\r\n\
-removed \r\n\
\x20context \t\r\n\
lf-only \t\n\
.\r\n",
                )
                .await
                .unwrap();
        });

        let mut client = NntpClient::connect("127.0.0.1", address.port())
            .await
            .unwrap();
        let article_result = client.article("<issue-320@example.test>").await;
        server.await.unwrap();
        let lines = article_result.unwrap();

        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0].as_bytes(), b"normal text");
        assert_eq!(lines[1].as_bytes(), b"trailing space ");
        assert_eq!(lines[2].as_bytes(), b"trailing tab\t");
        assert_eq!(lines[3].as_bytes(), b" ");
        assert_eq!(lines[4].as_bytes(), b"");
        assert_eq!(lines[5].as_bytes(), b".payload");
        assert_eq!(lines[6].as_bytes(), b"+added \t");
        assert_eq!(lines[7].as_bytes(), b"-removed ");
        assert_eq!(lines[8].as_bytes(), b" context \t");
        assert_eq!(lines[9].as_bytes(), b"lf-only \t");
        assert!(!lines.iter().any(|line| line == "."));

        let reconstructed = lines.join("\n");
        assert_eq!(
            reconstructed.as_bytes(),
            b"normal text\ntrailing space \ntrailing tab\t\n \n\n.payload\n+added \t\n-removed \n context \t\nlf-only \t"
        );
    }

    #[tokio::test]
    async fn group_validates_matching_group_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);

            stream
                .get_mut()
                .write_all(b"200 mock server ready\r\n")
                .await
                .unwrap();

            let mut command = Vec::new();
            stream.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"GROUP org.kernel.vger.linux-nfs\r\n");

            // Server mistakenly sends response for linux-mm
            stream
                .get_mut()
                .write_all(b"211 500 1 500 org.kvack.linux-mm\r\n")
                .await
                .unwrap();
        });

        let mut client = NntpClient::connect("127.0.0.1", address.port())
            .await
            .unwrap();
        let err = client.group("org.kernel.vger.linux-nfs").await.unwrap_err();
        server.await.unwrap();

        assert!(err.to_string().contains(
            "Mismatched GROUP response: expected org.kernel.vger.linux-nfs, got org.kvack.linux-mm"
        ));
    }
}

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

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Request, RequestBuilder, StatusCode, Url};
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::warn;

const LORE_BASE_URL: &str = "https://lore.kernel.org/all/";
const MAX_MBOX_DOWNLOAD: usize = 10 * 1024 * 1024;
const MAX_MBOX_DECOMPRESSED: usize = 50 * 1024 * 1024;
const LORE_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

static LORE_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

pub fn split_mbox(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut emails = Vec::new();
    let mut current_email = Vec::new();

    for line in raw.split_inclusive(|&b| b == b'\n') {
        if is_mbox_separator(line) {
            if !current_email.is_empty() {
                emails.push(std::mem::take(&mut current_email));
            }
            // Skip the "From " line.
        } else {
            current_email.extend_from_slice(line);
        }
    }

    if !current_email.is_empty() {
        emails.push(current_email);
    }

    emails
}

pub fn is_mbox_separator(line: &[u8]) -> bool {
    if !line.starts_with(b"From ") {
        return false;
    }
    // Mbox separator lines normally contain a timestamp. Requiring an
    // HH:MM:SS-like value avoids splitting on ordinary body text.
    line.iter().filter(|&&b| b == b':').count() >= 2
}

#[derive(Clone)]
pub(crate) struct LoreMboxClient {
    client: &'static Client,
    base_url: Url,
    retry_delays: [Duration; 2],
}

struct LoreFetchFailure {
    error: anyhow::Error,
    retryable: bool,
}

impl LoreMboxClient {
    pub(crate) fn new() -> Result<Self> {
        Self::with_base_url(LORE_BASE_URL)
    }

    fn with_base_url(base_url: &str) -> Result<Self> {
        Self::with_base_url_and_retry_delays(base_url, LORE_RETRY_DELAYS)
    }

    fn with_base_url_and_retry_delays(base_url: &str, retry_delays: [Duration; 2]) -> Result<Self> {
        let base_url = Url::parse(base_url).context("invalid lore base URL")?;
        Ok(Self {
            client: shared_http_client()?,
            base_url,
            retry_delays,
        })
    }

    pub(crate) async fn search_patch_id(&self, patch_id: &str) -> Result<Vec<u8>> {
        if patch_id.len() != 40 || !patch_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!("invalid prerequisite patch ID: {patch_id}"));
        }

        let mut url = self.base_url.clone();
        url.query_pairs_mut()
            .append_pair("x", "m")
            .append_pair("q", &format!("patchid:{patch_id}"))
            .append_pair("t", "1");
        self.fetch(
            self.client.post(url).body(""),
            &format!("search result for patch ID {patch_id}"),
        )
        .await
    }

    pub(crate) async fn fetch_thread(&self, message_id: &str) -> Result<Vec<u8>> {
        let url = self
            .base_url
            .join(&format!("{message_id}/t.mbox.gz"))
            .context("invalid lore thread URL")?;
        self.fetch(
            self.client.get(url),
            &format!("thread for message ID {message_id}"),
        )
        .await
    }

    async fn fetch(&self, request: RequestBuilder, description: &str) -> Result<Vec<u8>> {
        let request = request
            .build()
            .with_context(|| format!("failed to build lore request for {description}"))?;

        // Lore requires POST for search exports, but both supported operations
        // are read-only and safe to retry.
        let mut attempt = 0;
        let compressed = loop {
            let request = request
                .try_clone()
                .ok_or_else(|| anyhow!("lore request for {description} cannot be retried"))?;
            match self.fetch_once(request, description).await {
                Ok(compressed) => break compressed,
                Err(failure) => {
                    if failure.retryable
                        && let Some(delay) = self.retry_delays.get(attempt).copied()
                    {
                        let remaining_delays = self.retry_delays.len() - attempt - 1;
                        attempt += 1;
                        warn!(
                            "Lore request for {} failed: {}; retrying in {:.1}s ({} retries remaining)",
                            description,
                            failure.error,
                            delay.as_secs_f64(),
                            remaining_delays
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(failure.error);
                }
            }
        };

        tokio::task::spawn_blocking(move || decompress_mbox(&compressed))
            .await
            .context("lore mbox decompression task failed")?
    }

    async fn fetch_once(
        &self,
        request: Request,
        description: &str,
    ) -> std::result::Result<Vec<u8>, LoreFetchFailure> {
        let mut response = self.client.execute(request).await.map_err(|error| {
            let retryable =
                error.is_timeout() || error.is_connect() || error.is_body() || error.is_request();
            LoreFetchFailure {
                error: anyhow::Error::new(error)
                    .context(format!("failed to fetch lore {description}")),
                retryable,
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(LoreFetchFailure {
                error: anyhow!("lore {description} returned HTTP {status}"),
                retryable: is_retryable_status(status),
            });
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_MBOX_DOWNLOAD as u64)
        {
            return Err(LoreFetchFailure {
                error: anyhow!(
                    "lore {description} exceeds the {MAX_MBOX_DOWNLOAD} byte download limit"
                ),
                retryable: false,
            });
        }

        let mut compressed = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(|error| {
                let retryable = error.is_timeout() || error.is_connect() || error.is_body();
                LoreFetchFailure {
                    error: anyhow::Error::new(error)
                        .context(format!("failed to download lore {description}")),
                    retryable,
                }
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if compressed.len() + chunk.len() > MAX_MBOX_DOWNLOAD {
                return Err(LoreFetchFailure {
                    error: anyhow!(
                        "lore {description} exceeds the {MAX_MBOX_DOWNLOAD} byte download limit"
                    ),
                    retryable: false,
                });
            }
            compressed.extend_from_slice(&chunk);
        }

        Ok(compressed)
    }
}

fn shared_http_client() -> Result<&'static Client> {
    if let Some(client) = LORE_HTTP_CLIENT.get() {
        return Ok(client);
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build lore HTTP client")?;
    Ok(LORE_HTTP_CLIENT.get_or_init(|| client))
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn decompress_mbox(compressed: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(compressed);
    let mut limited = decoder.take((MAX_MBOX_DECOMPRESSED + 1) as u64);
    let mut raw = Vec::new();
    limited
        .read_to_end(&mut raw)
        .context("failed to decompress lore mbox")?;
    if raw.len() > MAX_MBOX_DECOMPRESSED {
        return Err(anyhow!(
            "decompressed lore mbox exceeds the {MAX_MBOX_DECOMPRESSED} byte limit"
        ));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mbox_server_with_statuses(
        raw: &[u8],
        statuses: Vec<StatusCode>,
    ) -> Result<(String, tokio::task::JoinHandle<Result<Vec<String>>>)> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(raw)?;
        let compressed = encoder.finish()?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut socket, _) = listener.accept().await?;
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.len() > 8192 {
                        return Err(anyhow!("test HTTP request exceeded header limit"));
                    }
                }

                let reason = match status {
                    StatusCode::OK => "OK",
                    StatusCode::NOT_FOUND => "Not Found",
                    StatusCode::TOO_MANY_REQUESTS => "Too Many Requests",
                    StatusCode::SERVICE_UNAVAILABLE => "Service Unavailable",
                    _ => "Test Response",
                };
                let body = if status.is_success() {
                    compressed.as_slice()
                } else {
                    &[]
                };
                let headers = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status.as_u16(),
                    reason,
                    body.len()
                );
                socket.write_all(headers.as_bytes()).await?;
                socket.write_all(body).await?;
                socket.shutdown().await?;
                requests.push(String::from_utf8(request)?);
            }
            Ok(requests)
        });

        Ok((format!("http://{address}/"), server))
    }

    async fn spawn_mbox_server(
        raw: &[u8],
    ) -> Result<(String, tokio::task::JoinHandle<Result<Vec<String>>>)> {
        spawn_mbox_server_with_statuses(raw, vec![StatusCode::OK]).await
    }

    #[test]
    fn reuses_http_client_across_lore_clients() -> Result<()> {
        let first = LoreMboxClient::with_base_url("http://127.0.0.1:1/")?;
        let second = LoreMboxClient::with_base_url("http://127.0.0.1:2/")?;

        assert!(std::ptr::eq(first.client, second.client));
        Ok(())
    }

    #[tokio::test]
    async fn searches_patch_id_with_post_and_decompresses_response() -> Result<()> {
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let expected = b"test mbox";
        let (base_url, server) = spawn_mbox_server(expected).await?;
        let client = LoreMboxClient::with_base_url(&base_url)?;

        let raw = client.search_patch_id(patch_id).await?;
        let requests = server.await??;

        assert_eq!(raw, expected);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /?x=m&q=patchid%3A"));
        assert!(requests[0].contains(patch_id));
        Ok(())
    }

    #[tokio::test]
    async fn fetches_thread_with_get_and_decompresses_response() -> Result<()> {
        let expected = b"test thread mbox";
        let (base_url, server) = spawn_mbox_server(expected).await?;
        let client = LoreMboxClient::with_base_url(&base_url)?;

        let raw = client.fetch_thread("message%2Fpart@example.com").await?;
        let requests = server.await??;

        assert_eq!(raw, expected);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /message%2Fpart@example.com/t.mbox.gz "));
        Ok(())
    }

    #[tokio::test]
    async fn retries_transient_http_status() -> Result<()> {
        let expected = b"test retry mbox";
        let (base_url, server) = spawn_mbox_server_with_statuses(
            expected,
            vec![StatusCode::SERVICE_UNAVAILABLE, StatusCode::OK],
        )
        .await?;
        let client = LoreMboxClient::with_base_url_and_retry_delays(
            &base_url,
            [Duration::ZERO, Duration::ZERO],
        )?;

        let raw = client.fetch_thread("message@example.com").await?;
        let requests = server.await??;

        assert_eq!(raw, expected);
        assert_eq!(requests.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn does_not_retry_permanent_http_status() -> Result<()> {
        let (base_url, server) =
            spawn_mbox_server_with_statuses(b"", vec![StatusCode::NOT_FOUND]).await?;
        let client = LoreMboxClient::with_base_url_and_retry_delays(
            &base_url,
            [Duration::ZERO, Duration::ZERO],
        )?;

        let error = client
            .fetch_thread("missing@example.com")
            .await
            .expect_err("404 response should fail");
        let requests = server.await??;

        assert!(error.to_string().contains("404"));
        assert_eq!(requests.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn bounds_transient_http_retries() -> Result<()> {
        let (base_url, server) = spawn_mbox_server_with_statuses(
            b"",
            vec![
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::SERVICE_UNAVAILABLE,
            ],
        )
        .await?;
        let client = LoreMboxClient::with_base_url_and_retry_delays(
            &base_url,
            [Duration::ZERO, Duration::ZERO],
        )?;

        let error = client
            .fetch_thread("unavailable@example.com")
            .await
            .expect_err("retries should be exhausted");
        let requests = server.await??;

        assert!(error.to_string().contains("503"));
        assert_eq!(requests.len(), 3);
        Ok(())
    }
}

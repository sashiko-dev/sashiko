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

//! Local HTTP servers for testing how providers classify transport failures.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinHandle;

pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(TEST_TIMEOUT)
        .build()
        .unwrap()
}

pub(crate) struct TestServer {
    pub(crate) url: String,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start(
        connections: Option<usize>,
        response: impl Fn(SocketAddr) -> Vec<u8> + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut served = 0;
            while connections.is_none_or(|n| served < n) {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                stream.write_all(&response(address)).await.unwrap();
                stream.flush().await.unwrap();
                served += 1;
            }
        });
        Self {
            url: format!("http://{address}"),
            handle,
        }
    }

    pub(crate) async fn finish(self) {
        tokio::time::timeout(TEST_TIMEOUT, self.handle)
            .await
            .expect("test server did not finish in time")
            .expect("test server panicked");
    }
}

pub(crate) async fn serve(bytes: Vec<u8>) -> TestServer {
    TestServer::start(Some(1), move |_| bytes.clone()).await
}

pub(crate) struct RedirectLoop(TestServer);

impl RedirectLoop {
    pub(crate) async fn start() -> Self {
        Self(
            TestServer::start(None, |address| {
                complete("302 Found", &format!("Location: http://{address}/\r\n"), "")
            })
            .await,
        )
    }

    pub(crate) fn url(&self) -> &str {
        &self.0.url
    }
}

impl Drop for RedirectLoop {
    fn drop(&mut self) {
        self.0.handle.abort();
    }
}

pub(crate) struct RefusedPort {
    pub(crate) url: String,
    _socket: TcpSocket,
}

impl RefusedPort {
    pub(crate) fn new() -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = socket.local_addr().unwrap();
        Self {
            url: format!("http://{address}"),
            _socket: socket,
        }
    }
}

async fn read_request(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "client closed before sending a full request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "client closed before sending the request body");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn response(status: &str, extra_headers: &str, content_length: usize, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}\
         Content-Length: {content_length}\r\nConnection: close\r\n\r\n{body}"
    )
    .into_bytes()
}

/// A Content-Length larger than the bytes sent simulates a dropped body.
pub(crate) fn truncated(status: &str, extra_headers: &str, body: &str) -> Vec<u8> {
    response(status, extra_headers, 4096, body)
}

pub(crate) fn complete(status: &str, extra_headers: &str, body: &str) -> Vec<u8> {
    response(status, extra_headers, body.len(), body)
}

pub(crate) const BODY_READ_FAILURE: &str = "error decoding response body";

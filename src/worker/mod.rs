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

#[cfg(feature = "server")]
pub mod bug_worker;
#[cfg(feature = "server")]
pub mod compressor;
#[cfg(feature = "server")]
pub mod email;
#[cfg(feature = "server")]
pub mod forge;
pub mod location_resolver;
#[cfg(feature = "server")]
pub mod patchwork;
pub mod prefetch;
pub mod prompts;
#[cfg(feature = "server")]
pub mod repack;
#[cfg(feature = "server")]
pub mod sync;

pub use location_resolver::*;
pub use prompts::*;

use std::time::Duration;

/// Connect timeout applied to every outbox delivery client.
pub const OUTBOX_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total per-request timeout applied to every outbox delivery client.
pub const OUTBOX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Build the HTTP client used by the outbox delivery workers.
///
/// Invariant: every request an outbox worker awaits must fail within a
/// bounded time. ForgeWorker and PatchworkWorker each drain their outbox
/// table sequentially from a single loop, so one await that never
/// resolves (a remote that accepts the connection and then goes silent)
/// stalls delivery of every pending entry in that table for the lifetime
/// of the daemon; nothing else drains it. reqwest::Client::new() carries
/// no timeout at all, so the workers must obtain their client from this
/// helper, which bounds both connection establishment and the whole
/// request (connect, send, response headers and body).
pub fn build_outbox_client(
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
}

/// Build the outbox delivery client with the production timeouts.
pub fn outbox_client() -> Result<reqwest::Client, reqwest::Error> {
    build_outbox_client(OUTBOX_CONNECT_TIMEOUT, OUTBOX_REQUEST_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn outbox_client_request_fails_within_bound_on_stalled_remote() {
        // The listener accepts the connection at the kernel level but the
        // test never reads or answers, modelling a remote that wedges after
        // the TCP handshake.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());

        let client =
            build_outbox_client(Duration::from_secs(1), Duration::from_millis(200)).unwrap();
        let started = Instant::now();
        let err = client.get(&url).send().await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(err.is_timeout(), "expected timeout error, got {err:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "request took {elapsed:?} to fail"
        );
        drop(listener);
    }

    #[test]
    fn production_outbox_client_builds() {
        assert!(outbox_client().is_ok());
    }
}

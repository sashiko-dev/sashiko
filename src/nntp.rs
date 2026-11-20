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
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_util::either::Either;
use tracing::{debug, info, warn};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// The command methods read and write through this, so plaintext and
/// NNTPS differ only in how `connect` builds the stream.
type NntpStream = Either<TcpStream, TlsStream<TcpStream>>;

pub struct NntpClient {
    /// Taken during each timed read or write and only restored on
    /// success, so a timed-out or failed operation drops the socket and
    /// cannot leave a half-written or desynchronized stream behind.
    stream: Option<BufReader<NntpStream>>,
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

/// The crypto provider backing every NNTPS handshake.
///
/// Optional features (such as `bedrock`) can link `ring` alongside
/// `aws-lc-rs`. With two providers present rustls cannot pick a process
/// default on its own, and `ClientConfig::builder()` panics. Neither
/// crate installs a default; each falls back to its own provider when
/// none is set. Naming the provider here makes the choice local rather
/// than a process-wide one for this module alone.
fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider())
}

/// Trust anchors come from the host certificate store, so an internal
/// CA is installed by the deployment rather than named in the config.
///
/// The ingestor reconnects every cycle, so a usable config is built
/// once and cached. A failure is not cached. The host trust store can
/// be populated after this process starts, and a cached error would
/// fail every later cycle until a restart.
async fn native_tls_config() -> Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

    if let Some(config) = CONFIG.get() {
        return Ok(config.clone());
    }

    // Loading the store walks the certificate directory with blocking
    // file I/O, so it runs off the executor thread.
    let loaded = tokio::task::spawn_blocking(rustls_native_certs::load_native_certs)
        .await
        .map_err(|e| anyhow!("certificate loader panicked: {}", e))?;
    for error in &loaded.errors {
        warn!("Ignoring unreadable system certificate: {}", error);
    }

    let mut roots = RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
    debug!("Loaded {} system certificates ({} ignored)", added, ignored);
    // An empty root store does not fail here; the handshake then fails
    // with an opaque UnknownIssuer alert.
    if added == 0 {
        return Err(anyhow!(
            "no usable system certificate found; install the CA in the host trust store"
        ));
    }

    let config = client_config(roots)?;
    Ok(CONFIG.get_or_init(|| Arc::new(config)).clone())
}

/// The one builder chain for every client config, so the TLS test
/// exercises the same protocol and provider choices as production.
fn client_config(roots: RootCertStore) -> Result<ClientConfig> {
    Ok(ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("TLS protocol versions rejected: {}", e))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

impl NntpClient {
    /// Connect to `host`, wrapping the session in TLS when `tls` is
    /// set. NNTPS is implicit. The handshake completes before the
    /// server sends its greeting.
    pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Self> {
        let tls_config = if tls {
            Some(native_tls_config().await?)
        } else {
            None
        };
        Self::connect_with_tls_config(host, port, tls_config).await
    }

    pub(crate) async fn connect_with_tls_config(
        host: &str,
        port: u16,
        tls_config: Option<Arc<ClientConfig>>,
    ) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        info!(
            "Connecting to NNTP server at {} (tls: {})",
            addr,
            tls_config.is_some()
        );
        // The tuple form brackets an IPv6 literal itself, which the
        // "host:port" string does not, and ServerName accepts one.
        let tcp = timeout(DEFAULT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| anyhow!("Connection timed out"))??;
        if let Err(e) = tcp.set_nodelay(true) {
            debug!("Failed to set TCP_NODELAY on NNTP socket: {}", e);
        }

        let stream = match tls_config {
            Some(config) => {
                let server_name = ServerName::try_from(host.to_string())
                    .map_err(|_| anyhow!("Not a valid TLS server name: {}", host))?;
                let stream = timeout(
                    DEFAULT_TIMEOUT,
                    TlsConnector::from(config).connect(server_name, tcp),
                )
                .await
                .map_err(|_| anyhow!("TLS handshake timed out"))??;
                Either::Right(stream)
            }
            None => Either::Left(tcp),
        };

        let mut reader = BufReader::new(stream);

        let mut buf = Vec::new();
        timeout(DEFAULT_TIMEOUT, reader.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| anyhow!("Timeout reading welcome message"))??;
        if !buf.ends_with(b"\n") {
            return Err(anyhow!("Unexpected EOF reading welcome message"));
        }
        let response = String::from_utf8_lossy(&buf).trim().to_string();

        if !matches!(Self::status_code(&response), Some("200" | "201")) {
            return Err(anyhow!("Unexpected welcome message: {}", response));
        }

        debug!("Connected: {}", response);
        Ok(Self {
            stream: Some(reader),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    fn status_code(response: &str) -> Option<&str> {
        let code = response.get(..3)?;
        if code.bytes().all(|b| b.is_ascii_digit())
            && response.as_bytes().get(3).is_none_or(|&b| b == b' ')
        {
            Some(code)
        } else {
            None
        }
    }

    /// True while the underlying socket has not been dropped by an I/O
    /// error, unexpected EOF, protocol/server error, timeout, or `QUIT`.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    async fn read_line_with_timeout(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| anyhow!("NNTP connection closed after prior I/O error or timeout"))?;
        match timeout(self.timeout, stream.read_until(b'\n', buf)).await {
            Ok(Ok(0)) => Err(anyhow!("Unexpected EOF from NNTP server")),
            Ok(Ok(_)) if !buf.ends_with(b"\n") => {
                Err(anyhow!("Unexpected mid-line EOF from NNTP server"))
            }
            Ok(Ok(n)) => {
                self.stream = Some(stream);
                Ok(n)
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(anyhow!("Read timed out")),
        }
    }

    async fn send_command(&mut self, command: &str) -> Result<()> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| anyhow!("NNTP connection closed after prior I/O error or timeout"))?;
        let mut frame = Vec::with_capacity(command.len() + 2);
        frame.extend_from_slice(command.as_bytes());
        frame.extend_from_slice(b"\r\n");
        let res = timeout(self.timeout, async {
            stream.get_mut().write_all(&frame).await?;
            stream.get_mut().flush().await
        })
        .await;
        match res {
            Ok(Ok(())) => {
                self.stream = Some(stream);
                Ok(())
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(anyhow!("Write timed out")),
        }
    }

    async fn read_response(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        self.read_line_with_timeout(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).trim().to_string())
    }

    pub async fn list(&mut self) -> Result<Vec<String>> {
        self.send_command("LIST").await?;
        let response = self.read_response().await?;

        if Self::status_code(&response) != Some("215") {
            self.stream = None;
            return Err(anyhow!("Failed to retrieve list: {}", response));
        }

        let mut groups = Vec::new();
        loop {
            let mut buf = Vec::new();
            self.read_line_with_timeout(&mut buf).await?;

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

        if Self::status_code(&response) != Some("211") {
            if Self::status_code(&response) != Some("411") {
                self.stream = None;
            }
            return Err(anyhow!(
                "Failed to select group {}: {}",
                group_name,
                response
            ));
        }

        let parts: Vec<&str> = response.split_whitespace().collect();
        if parts.len() < 5 {
            self.stream = None;
            return Err(anyhow!("Invalid GROUP response format: {}", response));
        }

        if parts[4] != group_name {
            self.stream = None;
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

        if Self::status_code(&response) != Some("220") {
            if !matches!(Self::status_code(&response), Some("423" | "430")) {
                self.stream = None;
            }
            return Err(anyhow!("Failed to retrieve article {}: {}", id, response));
        }

        let mut lines = Vec::new();
        loop {
            let mut buf = Vec::new();
            self.read_line_with_timeout(&mut buf).await?;

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
        if self.stream.is_none() {
            return Ok(());
        }
        self.send_command("QUIT").await?;
        let response = self.read_response().await?;

        if !response.starts_with("205") {
            debug!("QUIT response was not 205: {}", response);
        }
        if let Some(mut stream) = self.stream.take() {
            // Dropping a TlsStream sends no close_notify; only shutdown
            // does. Either forwards poll_shutdown to whichever side it holds.
            // The close_notify is a write, so a peer that has stopped
            // reading can stall it like any other.
            match timeout(self.timeout, stream.get_mut().shutdown()).await {
                Ok(Err(e)) => debug!("Shutdown after QUIT failed: {}", e),
                Err(_) => debug!("Shutdown after QUIT timed out"),
                Ok(Ok(())) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::{RootCertStore, ServerConfig};

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

        let mut client = NntpClient::connect("127.0.0.1", address.port(), false)
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

        let mut client = NntpClient::connect("127.0.0.1", address.port(), false)
            .await
            .unwrap();
        let err = client.group("org.kernel.vger.linux-nfs").await.unwrap_err();
        server.await.unwrap();

        assert!(err.to_string().contains(
            "Mismatched GROUP response: expected org.kernel.vger.linux-nfs, got org.kvack.linux-mm"
        ));
    }

    /// The last line is dot-stuffed on the wire, so a client that
    /// forgets to unstuff it returns two leading dots.
    const ARTICLE_LINES: &[&str] = &[
        "From: someone@example.com",
        "Subject: [PATCH] test",
        "",
        ".signature",
    ];

    /// Speak just enough NNTP to serve one ARTICLE and a QUIT.
    async fn serve_one<S>(stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut stream = BufReader::new(stream);
        stream.write_all(b"201 test server ready\r\n").await?;
        stream.flush().await?;

        loop {
            let mut line = Vec::new();
            if stream.read_until(b'\n', &mut line).await? == 0 {
                break;
            }
            let command = String::from_utf8_lossy(&line).trim().to_string();

            if command.starts_with("ARTICLE") {
                stream
                    .write_all(b"220 1 <test@example.com> article\r\n")
                    .await?;
                for line in ARTICLE_LINES {
                    if line.starts_with('.') {
                        stream.write_all(b".").await?;
                    }
                    stream.write_all(line.as_bytes()).await?;
                    stream.write_all(b"\r\n").await?;
                }
                stream.write_all(b".\r\n").await?;
                stream.flush().await?;
            } else if command.starts_with("QUIT") {
                stream.write_all(b"205 closing connection\r\n").await?;
                stream.flush().await?;
                break;
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn tls_article_round_trip() {
        let issued = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let cert = issued.cert.der().clone();
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));

        let server_config = ServerConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(socket).await.unwrap();
            serve_one(stream).await.unwrap();
        });

        // Trust only the certificate the test server just minted, so the
        // handshake exercises real verification rather than skipping it.
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();

        let mut client = NntpClient::connect_with_tls_config(
            &addr.ip().to_string(),
            addr.port(),
            Some(Arc::new(client_config(roots).unwrap())),
        )
        .await
        .expect("TLS connect");

        assert_eq!(client.article("1").await.unwrap(), ARTICLE_LINES);
        client.quit().await.unwrap();
    }

    #[tokio::test]
    async fn unexpected_eof_mid_article_errors_and_marks_disconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"200 mock server ready\r\n")
                .await
                .unwrap();

            let mut reader = BufReader::new(&mut stream);
            let mut command = Vec::new();
            reader.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"ARTICLE 7\r\n");

            // Drop socket mid-body without terminating ".\r\n".
            stream
                .write_all(b"220 7 <partial@example.test> article\r\npartial line\r\n")
                .await
                .unwrap();
        });

        let mut client = NntpClient::connect("127.0.0.1", address.port(), false)
            .await
            .unwrap();
        assert!(client.is_connected());

        let err = client.article("7").await.unwrap_err();
        server.await.unwrap();

        assert!(
            err.to_string().contains("Unexpected EOF"),
            "unexpected error: {err}"
        );
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn mid_line_eof_after_dot_errors_and_marks_disconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"200 mock server ready\r\n")
                .await
                .unwrap();

            let mut reader = BufReader::new(&mut stream);
            let mut command = Vec::new();
            reader.read_until(b'\n', &mut command).await.unwrap();
            assert_eq!(command, b"ARTICLE 8\r\n");

            // Drop socket right after a bare dot without trailing CRLF/LF.
            stream
                .write_all(b"220 8 <dot-eof@example.test> article\r\nbody line\r\n.")
                .await
                .unwrap();
        });

        let mut client = NntpClient::connect("127.0.0.1", address.port(), false)
            .await
            .unwrap();
        let err = client.article("8").await.unwrap_err();
        server.await.unwrap();

        assert!(
            err.to_string().contains("EOF"),
            "expected mid-line EOF error, got: {err}"
        );
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn benign_4xx_keeps_stream_while_protocol_or_server_errors_drop_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"200 mock server ready\r\n")
                .await
                .unwrap();

            let mut reader = BufReader::new(&mut stream);
            let mut cmd = Vec::new();

            reader.read_until(b'\n', &mut cmd).await.unwrap();
            assert_eq!(cmd, b"GROUP missing.group\r\n");
            reader
                .get_mut()
                .write_all(b"411 no such news group\r\n")
                .await
                .unwrap();

            cmd.clear();
            reader.read_until(b'\n', &mut cmd).await.unwrap();
            assert_eq!(cmd, b"ARTICLE 99\r\n");
            reader
                .get_mut()
                .write_all(b"423 no such article number\r\n")
                .await
                .unwrap();

            cmd.clear();
            reader.read_until(b'\n', &mut cmd).await.unwrap();
            assert_eq!(cmd, b"GROUP closing.group\r\n");
            reader
                .get_mut()
                .write_all(b"400 service discontinued\r\n")
                .await
                .unwrap();
        });

        let mut client = NntpClient::connect("127.0.0.1", address.port(), false)
            .await
            .unwrap();
        assert!(client.group("missing.group").await.is_err());
        assert!(
            client.is_connected(),
            "411 must preserve healthy NNTP connection"
        );

        assert!(client.article("99").await.is_err());
        assert!(
            client.is_connected(),
            "423 must preserve healthy NNTP connection"
        );

        assert!(client.group("closing.group").await.is_err());
        assert!(
            !client.is_connected(),
            "400 service discontinued must drop stream so next group reconnects"
        );
        server.await.unwrap();
    }
}

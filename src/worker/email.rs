use crate::settings::{MailTransport, SmtpSettings};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSendmailTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

const SMTP_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const SMTP_SEND_TIMEOUT: Duration = Duration::from_secs(60);
const SMTP_MAX_ATTEMPTS: u32 = 3;
const SMTP_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);

/// How long one sendmail invocation may run. The same bound lettre
/// applies to an SMTP session, since both wait on a peer that has
/// nothing left to say once it has accepted the message.
const SENDMAIL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct EmailWorker {
    db: Arc<crate::db::Database>,
    settings: SmtpSettings,
    /// Mirrors server.log_sign_in_links, so that the one switch governs every
    /// place a link could reach the log.
    log_sign_in_links: bool,
    heartbeat: Option<Arc<AtomicI64>>,
}

impl EmailWorker {
    pub fn new(
        db: Arc<crate::db::Database>,
        settings: SmtpSettings,
        log_sign_in_links: bool,
    ) -> Self {
        Self {
            db,
            settings,
            log_sign_in_links,
            heartbeat: None,
        }
    }

    pub fn with_heartbeat(mut self, heartbeat: Arc<AtomicI64>) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    fn record_heartbeat(&self) {
        if let Some(hb) = &self.heartbeat {
            hb.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        }
    }

    /// Reports a sendmail binary that the configuration names but the
    /// host lacks. Startup refuses to run on it: the worker would
    /// spawn-fail every message and mark each one Failed, which is
    /// terminal, whereas mail left Pending survives until the path is
    /// corrected. A dry run never spawns the binary, so it is exempt.
    pub fn check_sendmail_path(settings: &SmtpSettings) -> Result<(), String> {
        if settings.dry_run || settings.transport != MailTransport::Sendmail {
            return Ok(());
        }

        // A directory or a file without an execute bit exists but
        // spawn-fails all the same, so test for what spawn needs.
        use std::os::unix::fs::PermissionsExt;
        let path = settings.sendmail_command();
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => Ok(()),
            _ => Err(format!(
                "smtp.sendmail_path \"{}\" is not an executable file",
                path
            )),
        }
    }

    pub async fn run(&self) {
        info!("Starting Email Worker...");
        loop {
            // Reclaim ghost emails (crashed while sending)
            if let Err(e) = self.db.sweep_ghost_emails().await {
                error!("Failed to sweep ghost emails: {}", e);
            }

            // Lock and send next pending email
            match self.db.lock_pending_email().await {
                Ok(Some(email)) => {
                    info!(
                        "Locked pending {} email ID {} for patch {:?}",
                        email.kind.as_str(),
                        email.id,
                        email.patch_id
                    );
                    let mut send_result = Ok(());
                    for attempt in 1..=SMTP_MAX_ATTEMPTS {
                        // A panic, e.g. lettre failing to display a header,
                        // must fail the row: left Sending, it is swept back
                        // to Pending and panics the worker again forever.
                        let send = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                            self.send_email(&email),
                        ));
                        send_result = match tokio::time::timeout(SMTP_SEND_TIMEOUT, send).await {
                            Ok(Ok(res)) => res,
                            Ok(Err(panic)) => Err(anyhow::anyhow!(
                                "panicked while building or sending: {}",
                                panic
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or("unknown panic")
                            )),
                            Err(_) => Err(anyhow::anyhow!(
                                "SMTP delivery timed out after {}s",
                                SMTP_SEND_TIMEOUT.as_secs()
                            )),
                        };
                        match &send_result {
                            Ok(()) => break,
                            Err(e) if attempt < SMTP_MAX_ATTEMPTS && is_transient_smtp_error(e) => {
                                let delay = SMTP_RETRY_BASE_DELAY * attempt;
                                warn!(
                                    "Transient SMTP failure sending email ID {} (attempt {}/{}): {}; retrying in {}s",
                                    email.id,
                                    attempt,
                                    SMTP_MAX_ATTEMPTS,
                                    e,
                                    delay.as_secs()
                                );
                                self.record_heartbeat();
                                sleep(delay).await;
                            }
                            Err(_) => break,
                        }
                    }
                    match send_result {
                        Ok(_) => {
                            info!("Successfully sent email ID {}", email.id);
                            if let Err(e) = self.db.mark_email_sent(email.id).await {
                                error!("Failed to mark email {} as sent: {}", email.id, e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to send email ID {}: {}", email.id, e);
                            if let Err(db_err) =
                                self.db.mark_email_failed(email.id, &e.to_string()).await
                            {
                                error!("Failed to mark email {} as failed: {}", email.id, db_err);
                            }
                        }
                    }
                    self.record_heartbeat();
                }
                Ok(None) => {
                    self.record_heartbeat();
                    // No pending emails, sleep
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Database error while locking pending email: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    async fn send_email(&self, email_row: &crate::db::EmailOutboxRow) -> anyhow::Result<()> {
        if self.settings.dry_run {
            info!(
                "DRY RUN: Would have sent email to {}, cc {}, subject '{}'",
                email_row.to_addresses, email_row.cc_addresses, email_row.subject
            );
            // The body of a sign-in mail carries the link, which is a bearer
            // credential and so is withheld from the log by default. Every
            // other kind of mail is safe to show in full.
            if email_row.kind == crate::db::EmailKind::SignInLink && !self.log_sign_in_links {
                info!(
                    "DRY RUN Body withheld because it contains a sign-in link. Set \
                     server.log_sign_in_links to print it."
                );
            } else {
                info!("DRY RUN Body:\n{}", email_row.body);
            }
            return Ok(());
        }

        let msg = build_email_message(&self.settings, email_row)?;

        match self.settings.transport {
            MailTransport::Smtp => Self::send_via_smtp(&self.settings, msg).await,
            MailTransport::Sendmail => Self::send_via_sendmail(&self.settings, msg).await,
        }
    }

    async fn send_via_smtp(settings: &SmtpSettings, msg: Message) -> anyhow::Result<()> {
        let server = settings
            .server
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("smtp.server is not configured"))?;
        let port = settings
            .port
            .ok_or_else(|| anyhow::anyhow!("smtp.port is not configured"))?;

        let mut mailer_builder = AsyncSmtpTransport::<Tokio1Executor>::relay(server)?
            .port(port)
            .timeout(Some(SMTP_SOCKET_TIMEOUT));

        if let (Some(user), Some(pass)) = (&settings.username, &settings.password) {
            let creds = Credentials::new(user.to_string(), pass.to_string());
            mailer_builder = mailer_builder.credentials(creds);
        }

        let mailer = mailer_builder.build();

        mailer.send(msg).await?;

        Ok(())
    }

    /// Hands the message to the local MTA. lettre passes the envelope
    /// on the command line rather than through -t, so the recipients
    /// are the ones sashiko addressed and not whatever the MTA parses
    /// back out of the headers. lettre ends the options with "--"
    /// ahead of the recipients, so an address that begins with a
    /// hyphen reaches the MTA as a recipient and not as an option.
    ///
    /// The wait is bounded because the outbox is drained one message
    /// at a time, so an MTA that never exits would hold every later
    /// message. lettre spawns the child with kill_on_drop, so the
    /// timeout also reaps it; a test holds lettre to that.
    async fn send_via_sendmail(settings: &SmtpSettings, msg: Message) -> anyhow::Result<()> {
        let mailer =
            AsyncSendmailTransport::<Tokio1Executor>::new_with_command(settings.sendmail_command());

        tokio::time::timeout(SENDMAIL_TIMEOUT, mailer.send(msg))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} did not exit within {} seconds",
                    settings.sendmail_command(),
                    SENDMAIL_TIMEOUT.as_secs()
                )
            })??;

        Ok(())
    }
}

fn build_email_message(
    settings: &SmtpSettings,
    email_row: &crate::db::EmailOutboxRow,
) -> anyhow::Result<Message> {
    let from = parse_lenient(&settings.sender_address)?;
    let message_id = format!("<sashiko-outbox-{}@{}>", email_row.id, from.email.domain());
    let clean_subject = crate::patch::clean_display_name(&email_row.subject);
    let mut builder = Message::builder()
        .message_id(Some(message_id))
        .from(from.clone())
        .subject(clean_subject);

    if let Some(label) = email_row.kind.transactional_list_label() {
        // Email addressed to a person rather than a thread must not provoke a
        // vacation autoresponder, and should be filable. Its replies go to the
        // sender: reply_to can be a review list, whose public archive would
        // hold the link the reply quotes.
        builder = builder
            .header(AutoSubmitted("auto-generated".to_string()))
            .header(ListId(format!("<{}.{}>", label, from.email.domain())));
    } else if matches!(
        email_row.kind,
        crate::db::EmailKind::ReviewNotification | crate::db::EmailKind::ReviewReply
    ) && let Some(reply_to) = &settings.reply_to
    {
        match parse_lenient(reply_to) {
            Ok(addr) => builder = builder.reply_to(addr),
            Err(e) => warn!("Failed to parse reply_to address '{}': {}", reply_to, e),
        }
    }

    let to_addresses: Vec<String> = serde_json::from_str(&email_row.to_addresses)?;
    for to in to_addresses {
        match parse_lenient(&to) {
            Ok(addr) => builder = builder.to(addr),
            Err(e) => warn!("Failed to parse 'to' address '{}': {}", to, e),
        }
    }

    let cc_addresses: Vec<String> = serde_json::from_str(&email_row.cc_addresses)?;
    for cc in cc_addresses {
        match parse_lenient(&cc) {
            Ok(addr) => builder = builder.cc(addr),
            Err(e) => warn!("Failed to parse 'cc' address '{}': {}", cc, e),
        }
    }

    let clean_in_reply_to = crate::patch::clean_display_name(&email_row.in_reply_to);
    if !clean_in_reply_to.is_empty() {
        builder = builder.header(lettre::message::header::InReplyTo::from(format!(
            "<{}>",
            clean_in_reply_to
        )));
    }

    if !email_row.references_hdr.is_empty() {
        let refs: Vec<String> = email_row
            .references_hdr
            .split_whitespace()
            .map(crate::patch::clean_display_name)
            .filter(|part| !part.is_empty())
            .map(|part| format!("<{}>", part))
            .collect();
        if !refs.is_empty() {
            builder = builder.references(refs.join(" "));
        }
    }

    Ok(builder
        .header(ContentType::TEXT_PLAIN)
        .body(email_row.body.clone())?)
}

/// Headers lettre does not model, declared here so the builder can carry them.
macro_rules! text_header {
    ($name:ident, $wire:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone)]
        struct $name(String);

        impl lettre::message::header::Header for $name {
            fn name() -> lettre::message::header::HeaderName {
                lettre::message::header::HeaderName::new_from_ascii_str($wire)
            }

            fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
                Ok(Self(s.to_string()))
            }

            fn display(&self) -> lettre::message::header::HeaderValue {
                lettre::message::header::HeaderValue::new(Self::name(), self.0.clone())
            }
        }
    };
}

text_header!(
    AutoSubmitted,
    "Auto-Submitted",
    "Tells autoresponders that nobody is waiting for a reply."
);
text_header!(
    ListId,
    "List-Id",
    "Gives recipients something stable to filter transactional mail on."
);

/// Returns true only when the failure is a transient SMTP 4xx rejection or a
/// pre-session connection establishment error where the server has definitely
/// not accepted the message. Post-connection socket/delivery timeouts and
/// mid-stream network errors are not retried because they can occur after the
/// DATA payload was already accepted by the remote MTA.
fn is_transient_smtp_error(err: &anyhow::Error) -> bool {
    if let Some(smtp_err) = err.downcast_ref::<lettre::transport::smtp::Error>()
        && smtp_err.is_transient()
    {
        return true;
    }
    // lettre reports a sendmail failure as its stderr and drops the
    // exit status, so EX_TEMPFAIL from a loaded queue is not told
    // apart from a rejection. Retrying a rejection costs two more
    // attempts; marking a temporary failure Failed loses the message.
    if err
        .downcast_ref::<lettre::transport::sendmail::Error>()
        .is_some()
    {
        return true;
    }
    let msg = err.to_string();
    msg.starts_with("transient error (4") || msg.starts_with("Connection error")
}

fn parse_lenient(s: &str) -> anyhow::Result<lettre::message::Mailbox> {
    if let Some(start) = s.find('<')
        && let Some(end) = s.rfind('>')
        && start < end
    {
        let name = s[..start].trim();
        let email = s[start + 1..end].trim();
        let addr: lettre::Address = email.parse()?;
        let clean_name = crate::patch::clean_display_name(name.trim_matches('"'));
        return Ok(lettre::message::Mailbox::new(
            (!clean_name.is_empty()).then_some(clean_name),
            addr,
        ));
    }
    let addr: lettre::Address = s.parse()?;
    Ok(lettre::message::Mailbox::new(None, addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes an executable script named sendmail in `dir`. A child
    /// process writes it: a script written from the test binary can fail
    /// to exec with ETXTBSY, since a child another test thread forks
    /// meanwhile holds the write fd open until its own exec.
    fn write_script(dir: &std::path::Path, body: &str) -> String {
        let script = dir.join("sendmail");
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "printf '%s' \"$1\" > \"$0\" && chmod 755 \"$0\""])
            .arg(&script)
            .arg(body)
            .status()
            .unwrap();
        assert!(status.success());
        script.to_str().unwrap().to_string()
    }

    /// Stands in for the MTA. Records the argument vector and the
    /// message on stdin so a test can inspect what sashiko handed over.
    fn stub_sendmail(dir: &std::path::Path) -> String {
        write_script(
            dir,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/argv\"\ncat > \"$(dirname \"$0\")/stdin\"\n",
        )
    }

    fn sendmail_settings(path: String) -> SmtpSettings {
        SmtpSettings {
            transport: MailTransport::Sendmail,
            server: None,
            port: None,
            username: None,
            password: None,
            sendmail_path: Some(path),
            sender_address: "bot@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        }
    }

    #[tokio::test]
    async fn test_sendmail_receives_envelope_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let settings = sendmail_settings(stub_sendmail(dir.path()));

        let msg = Message::builder()
            .from(settings.sender_address.parse().unwrap())
            .to("maintainer@example.com".parse().unwrap())
            .cc("list@example.com".parse().unwrap())
            .subject("Re: [PATCH] fix a thing")
            .header(ContentType::TEXT_PLAIN)
            .body("Reviewed-by: Sashiko\n".to_string())
            .unwrap();

        EmailWorker::send_via_sendmail(&settings, msg)
            .await
            .unwrap();

        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-i"), "argv was {}", argv);
        assert!(argv.contains("-f bot@sashiko.dev"), "argv was {}", argv);
        assert!(argv.contains("maintainer@example.com"), "argv was {}", argv);
        assert!(argv.contains("list@example.com"), "argv was {}", argv);

        let body = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
        assert!(body.contains("Subject: Re: [PATCH] fix a thing"));
        assert!(body.contains("Reviewed-by: Sashiko"));
    }

    /// Recipients come from the headers of a patch, and a local part
    /// may begin with a hyphen. A lettre that stopped emitting "--"
    /// fails here.
    #[tokio::test]
    async fn test_sendmail_recipient_cannot_be_an_option() {
        let dir = tempfile::tempdir().unwrap();
        let settings = sendmail_settings(stub_sendmail(dir.path()));

        let msg = Message::builder()
            .from(settings.sender_address.parse().unwrap())
            .to("-oQtmp@example.com".parse().unwrap())
            .subject("Re: [PATCH] fix a thing")
            .header(ContentType::TEXT_PLAIN)
            .body("Reviewed-by: Sashiko\n".to_string())
            .unwrap();

        EmailWorker::send_via_sendmail(&settings, msg)
            .await
            .unwrap();

        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(
            argv.contains("-f bot@sashiko.dev -- -oQtmp@example.com"),
            "argv was {}",
            argv
        );
    }

    #[tokio::test]
    async fn test_sendmail_reports_a_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        // Read the message before rejecting it, as an MTA does. A script
        // that exits first races lettre's write to its stdin, and the
        // test then sees EPIPE instead of the exit status.
        let script = write_script(
            dir.path(),
            "#!/bin/sh\ncat >/dev/null\necho 'queue full' >&2\nexit 75\n",
        );

        let settings = sendmail_settings(script);
        let msg = Message::builder()
            .from(settings.sender_address.parse().unwrap())
            .to("maintainer@example.com".parse().unwrap())
            .subject("Re: [PATCH] fix a thing")
            .header(ContentType::TEXT_PLAIN)
            .body("body\n".to_string())
            .unwrap();

        let err = EmailWorker::send_via_sendmail(&settings, msg)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("queue full"), "error was {}", err);
        // Exit 75 is EX_TEMPFAIL. lettre does not pass the status on,
        // so every sendmail failure is retried.
        assert!(is_transient_smtp_error(&err));
    }

    /// The timeout drops lettre's send future, and lettre spawns the
    /// child with kill_on_drop. A lettre that stopped doing so would
    /// leave the MTA running past the timeout, and the retry would
    /// then deliver the message twice. The test drops the future by
    /// aborting the task that owns it once the MTA is known to be
    /// running, so it does not race process startup against a clock.
    /// The child's fate is read from /proc, so the test is Linux-only;
    /// elsewhere a missing /proc would read as a dead child.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_sendmail_timeout_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        // exec keeps the shell's PID, so the recorded PID is the
        // process lettre spawned and the one the kill must reach.
        let script = write_script(
            dir.path(),
            "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/pid\"\nexec sleep 600\n",
        );

        let settings = sendmail_settings(script);
        let msg = Message::builder()
            .from(settings.sender_address.parse().unwrap())
            .to("maintainer@example.com".parse().unwrap())
            .subject("Re: [PATCH] fix a thing")
            .header(ContentType::TEXT_PLAIN)
            .body("body\n".to_string())
            .unwrap();

        let send =
            tokio::spawn(async move { EmailWorker::send_via_sendmail(&settings, msg).await });

        let pid_file = dir.path().join("pid");
        let mut pid = None;
        for _ in 0..600 {
            if let Ok(s) = std::fs::read_to_string(&pid_file)
                && let Ok(p) = s.trim().parse::<u32>()
            {
                pid = Some(p);
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        let pid = pid.expect("stub sendmail did not start");

        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());
        // The kill is delivered on drop and the reap happens later on
        // tokio's reaper, so the child is gone or a zombie, never
        // still sleeping.
        let stat = format!("/proc/{}/stat", pid);
        let mut state = String::new();
        for _ in 0..50 {
            state = match std::fs::read_to_string(&stat) {
                Ok(s) => s
                    .rsplit(") ")
                    .next()
                    .unwrap_or("")
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_string(),
                // Only a vanished /proc entry means the child is gone;
                // any other read error is a broken test, not a kill.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => panic!("reading {} failed: {}", stat, e),
            };
            if state.is_empty() || state == "Z" {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert!(
            state.is_empty() || state == "Z",
            "sendmail child {} is still running in state {}",
            pid,
            state
        );
    }

    #[test]
    fn test_email_parsing() {
        let addr_str = "\"Thomas Richard (TI)\" <thomas.richard@bootlin.com>";
        let parsed = parse_lenient(addr_str);
        assert!(parsed.is_ok(), "Failed to parse: {:?}", parsed.err());
        assert_eq!(
            format!("{}", parsed.unwrap()),
            "\"Thomas Richard (TI)\" <thomas.richard@bootlin.com>"
        );
    }

    #[test]
    fn test_email_parsing_unquoted() {
        let addr_str = "Thomas Richard (TI) <thomas.richard@bootlin.com>";
        let parsed = parse_lenient(addr_str);
        assert!(parsed.is_ok(), "Failed to parse: {:?}", parsed.err());
        assert_eq!(
            format!("{}", parsed.unwrap()),
            "\"Thomas Richard (TI)\" <thomas.richard@bootlin.com>"
        );
    }

    #[test]
    fn test_email_parsing_plain() {
        let addr_str = "thomas.richard@bootlin.com";
        let parsed = parse_lenient(addr_str);
        assert!(parsed.is_ok(), "Failed to parse: {:?}", parsed.err());
        // We will see what format!() returns for plain email
        info!("Plain email formatted: {}", parsed.as_ref().unwrap());
    }

    #[test]
    fn test_email_parsing_strips_control_characters_from_name() {
        let addr = parse_lenient("\"Bob\r\nBcc: victim@example.com\" <bob@example.com>").unwrap();
        assert_eq!(addr.name.as_deref(), Some("BobBcc: victim@example.com"));
        let addr = parse_lenient("\"\r\n\" <bob@example.com>").unwrap();
        assert_eq!(addr.name, None);
    }

    #[tokio::test]
    async fn test_crlf_display_name_does_not_wedge_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            crate::db::Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();
        // The author and subject strings parse_email used to store for headers
        // containing RFC 2047 encoded CR/LF sequences.
        db.insert_transactional_email(
            crate::db::EmailKind::BugAssignment,
            "Pending",
            "\"Bob\r\nX\" <bob@example.com>",
            &[],
            "sub\r\nject",
            "body",
        )
        .await
        .unwrap();

        let settings = sendmail_settings(stub_sendmail(dir.path()));
        let worker = EmailWorker::new(db.clone(), settings, false);
        let handle = tokio::spawn(async move { worker.run().await });

        let mut status = String::new();
        for _ in 0..60 {
            sleep(Duration::from_millis(50)).await;
            assert!(!handle.is_finished(), "email worker exited");
            let mut rows = db
                .conn
                .query("SELECT status FROM email_outbox", ())
                .await
                .unwrap();
            status = rows.next().await.unwrap().unwrap().get(0).unwrap();
            if status == "Sent" || status == "Failed" {
                break;
            }
        }
        handle.abort();
        assert_eq!(status, "Sent");
    }

    #[test]
    fn test_is_transient_smtp_error_classification() {
        let transient_454 = anyhow::anyhow!(
            "transient error (454): 4.7.0 Temporary authentication failure: generic failure"
        );
        assert!(is_transient_smtp_error(&transient_454));

        let conn_err = anyhow::anyhow!("Connection error: connection refused");
        assert!(is_transient_smtp_error(&conn_err));

        let timeout_err = anyhow::anyhow!("SMTP delivery timed out after 60s");
        assert!(!is_transient_smtp_error(&timeout_err));

        let net_err = anyhow::anyhow!("network error: connection reset by peer");
        assert!(!is_transient_smtp_error(&net_err));

        let permanent_550 = anyhow::anyhow!("permanent error (550): 5.1.1 User unknown");
        assert!(!is_transient_smtp_error(&permanent_550));

        let parse_err = parse_lenient("not-an-email").unwrap_err();
        assert!(!is_transient_smtp_error(&parse_err));
    }

    #[test]
    fn test_build_email_message_sets_deterministic_message_id() {
        let settings = SmtpSettings {
            transport: MailTransport::Smtp,
            server: Some("smtp.example.com".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "Sashiko Bot <sashiko@linux.dev>".to_string(),
            reply_to: None,
            dry_run: true,
        };
        let row = crate::db::EmailOutboxRow {
            id: 160058,
            patch_id: Some(42),
            kind: crate::db::EmailKind::ReviewNotification,
            status: "Sending".to_string(),
            to_addresses: "[\"dev@example.com\"]".to_string(),
            cc_addresses: "[]".to_string(),
            subject: "Re: [PATCH] test".to_string(),
            in_reply_to: "orig-msg@example.com".to_string(),
            references_hdr: "orig-msg@example.com".to_string(),
            body: "Review body".to_string(),
            locked_at: Some(1000),
            error_log: None,
            created_at: 1000,
        };

        let msg1 = String::from_utf8(build_email_message(&settings, &row).unwrap().formatted())
            .expect("valid utf8");
        let msg2 = String::from_utf8(build_email_message(&settings, &row).unwrap().formatted())
            .expect("valid utf8");

        assert!(
            msg1.contains("Message-ID: <sashiko-outbox-160058@linux.dev>\r\n"),
            "missing deterministic Message-ID in formatted message:\n{}",
            msg1
        );
        assert!(
            msg2.contains("Message-ID: <sashiko-outbox-160058@linux.dev>\r\n"),
            "missing deterministic Message-ID on retry build:\n{}",
            msg2
        );
    }

    #[test]
    fn test_transactional_email_is_marked_automated_and_filable() {
        let settings = SmtpSettings {
            transport: MailTransport::Smtp,
            server: Some("smtp.example.com".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "Sashiko Bot <sashiko@linux.dev>".to_string(),
            reply_to: Some("reviews@lists.example.org".to_string()),
            dry_run: true,
        };
        let row = |kind| crate::db::EmailOutboxRow {
            id: 1,
            patch_id: None,
            kind,
            status: "Sending".to_string(),
            to_addresses: "[\"dev@example.com\"]".to_string(),
            cc_addresses: "[]".to_string(),
            subject: "subject".to_string(),
            in_reply_to: String::new(),
            references_hdr: String::new(),
            body: "body".to_string(),
            locked_at: Some(1000),
            error_log: None,
            created_at: 1000,
        };
        let format = |kind| {
            String::from_utf8(
                build_email_message(&settings, &row(kind))
                    .unwrap()
                    .formatted(),
            )
            .expect("valid utf8")
        };

        let assignment = format(crate::db::EmailKind::BugAssignment);
        assert!(assignment.contains("Auto-Submitted: auto-generated\r\n"));
        assert!(assignment.contains("List-Id: <sashiko-bugs.linux.dev>\r\n"));
        assert!(!assignment.contains("Reply-To"));

        let sign_in = format(crate::db::EmailKind::SignInLink);
        assert!(sign_in.contains("List-Id: <sashiko-auth.linux.dev>\r\n"));
        assert!(!sign_in.contains("Reply-To"));

        let review = format(crate::db::EmailKind::ReviewNotification);
        assert!(!review.contains("Auto-Submitted"));
        assert!(!review.contains("List-Id"));
        assert!(review.contains("Reply-To: reviews@lists.example.org\r\n"));

        let cover_reply = format(crate::db::EmailKind::ReviewReply);
        assert!(!cover_reply.contains("Auto-Submitted"));
        assert!(!cover_reply.contains("List-Id"));
        assert!(cover_reply.contains("Reply-To: reviews@lists.example.org\r\n"));

        let unrecognized = format(crate::db::EmailKind::Unrecognized);
        assert!(!unrecognized.contains("List-Id"));
        assert!(!unrecognized.contains("Reply-To"));
    }

    #[test]
    fn test_missing_sendmail_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent-sendmail");
        let settings = sendmail_settings(missing.to_str().unwrap().to_string());

        let err = EmailWorker::check_sendmail_path(&settings).unwrap_err();
        assert!(err.contains("nonexistent-sendmail"), "error was {}", err);
    }

    #[test]
    fn test_non_executable_sendmail_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("sendmail");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        let settings = sendmail_settings(plain.to_str().unwrap().to_string());

        let err = EmailWorker::check_sendmail_path(&settings).unwrap_err();
        assert!(err.contains("not an executable file"), "error was {}", err);
    }

    #[test]
    fn test_present_sendmail_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let settings = sendmail_settings(stub_sendmail(dir.path()));

        assert!(EmailWorker::check_sendmail_path(&settings).is_ok());
    }

    #[test]
    fn test_dry_run_does_not_need_sendmail() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent-sendmail");
        let mut settings = sendmail_settings(missing.to_str().unwrap().to_string());
        settings.dry_run = true;

        assert!(EmailWorker::check_sendmail_path(&settings).is_ok());
    }

    #[test]
    fn test_smtp_transport_does_not_need_sendmail() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent-sendmail");
        let mut settings = sendmail_settings(missing.to_str().unwrap().to_string());
        settings.transport = MailTransport::Smtp;

        assert!(EmailWorker::check_sendmail_path(&settings).is_ok());
    }
}

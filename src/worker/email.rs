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
                        send_result =
                            match tokio::time::timeout(SMTP_SEND_TIMEOUT, self.send_email(&email))
                                .await
                            {
                                Ok(res) => res,
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
    /// timeout also reaps it.
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
    let mut builder = Message::builder()
        .message_id(Some(message_id))
        .from(from.clone())
        .subject(&email_row.subject);

    if email_row.kind == crate::db::EmailKind::SignInLink {
        // Mail a person receives because they just asked for it must not
        // provoke a vacation autoresponder, and should be filable.
        builder = builder
            .header(AutoSubmitted("auto-generated".to_string()))
            .header(ListId(format!("<sashiko-auth.{}>", from.email.domain())));
    }

    if let Some(reply_to) = &settings.reply_to {
        match reply_to.parse() {
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

    if !email_row.in_reply_to.is_empty() {
        builder = builder.header(lettre::message::header::InReplyTo::from(format!(
            "<{}>",
            email_row.in_reply_to
        )));
    }

    if !email_row.references_hdr.is_empty() {
        let refs: Vec<String> = email_row
            .references_hdr
            .split_whitespace()
            .map(|part| format!("<{}>", part))
            .collect();
        builder = builder.references(refs.join(" "));
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
        if name.is_empty() {
            return Ok(lettre::message::Mailbox::new(None, addr));
        } else {
            let clean_name = name.trim_matches('"').to_string();
            return Ok(lettre::message::Mailbox::new(Some(clean_name), addr));
        }
    }
    let addr: lettre::Address = s.parse()?;
    Ok(lettre::message::Mailbox::new(None, addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the MTA. Records the argument vector and the
    /// message on stdin so a test can inspect what sashiko handed over.
    fn stub_sendmail(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("sendmail");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/argv\"\ncat > \"$(dirname \"$0\")/stdin\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.to_str().unwrap().to_string()
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
        let script = dir.path().join("sendmail");
        // Read the message before rejecting it, as an MTA does. A script
        // that exits first races lettre's write to its stdin, and the
        // test then sees EPIPE instead of the exit status.
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\necho 'queue full' >&2\nexit 75\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let settings = sendmail_settings(script.to_str().unwrap().to_string());
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

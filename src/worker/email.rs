use crate::settings::{MailTransport, SmtpSettings};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSendmailTransport, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

pub struct EmailWorker {
    db: std::sync::Arc<crate::db::Database>,
    settings: SmtpSettings,
}

impl EmailWorker {
    pub fn new(db: std::sync::Arc<crate::db::Database>, settings: SmtpSettings) -> Self {
        Self { db, settings }
    }

    /// Refuses to start when the sendmail binary named in the
    /// configuration is absent. Running on would spawn-fail every
    /// message and mark each one Failed, which is terminal; mail left
    /// Pending survives until the path is corrected and sashiko
    /// restarts. A dry run never spawns the binary, so it is exempt.
    fn check_sendmail_path(settings: &SmtpSettings) -> Result<(), String> {
        if settings.dry_run || settings.transport != MailTransport::Sendmail {
            return Ok(());
        }

        let path = settings.sendmail_command();
        if std::path::Path::new(path).exists() {
            Ok(())
        } else {
            Err(format!("smtp.sendmail_path \"{}\" does not exist", path))
        }
    }

    pub async fn run(&self) {
        info!("Starting Email Worker...");
        if let Err(e) = Self::check_sendmail_path(&self.settings) {
            error!("{}. Not sending mail; queued mail stays pending.", e);
            return;
        }
        loop {
            // Reclaim ghost emails (crashed while sending)
            if let Err(e) = self.db.sweep_ghost_emails().await {
                error!("Failed to sweep ghost emails: {}", e);
            }

            // Lock and send next pending email
            match self.db.lock_pending_email().await {
                Ok(Some(email)) => {
                    info!(
                        "Locked pending email ID {} for patch {:?}",
                        email.id, email.patch_id
                    );
                    match self.send_email(&email).await {
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
                }
                Ok(None) => {
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

    fn build_message(&self, email_row: &crate::db::EmailOutboxRow) -> anyhow::Result<Message> {
        let mut builder = Message::builder()
            .from(self.settings.sender_address.parse()?)
            .subject(&email_row.subject);

        if let Some(reply_to) = &self.settings.reply_to {
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

    async fn send_email(&self, email_row: &crate::db::EmailOutboxRow) -> anyhow::Result<()> {
        if self.settings.dry_run {
            info!(
                "DRY RUN: Would have sent email to {}, cc {}, subject '{}'",
                email_row.to_addresses, email_row.cc_addresses, email_row.subject
            );
            info!("DRY RUN Body:\n{}", email_row.body);
            return Ok(());
        }

        let msg = self.build_message(email_row)?;

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

        let mut mailer_builder = AsyncSmtpTransport::<Tokio1Executor>::relay(server)?.port(port);

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
    /// back out of the headers.
    async fn send_via_sendmail(settings: &SmtpSettings, msg: Message) -> anyhow::Result<()> {
        let mailer =
            AsyncSendmailTransport::<Tokio1Executor>::new_with_command(settings.sendmail_command());

        mailer.send(msg).await?;

        Ok(())
    }
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

    #[tokio::test]
    async fn test_sendmail_reports_a_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("sendmail");
        std::fs::write(&script, "#!/bin/sh\necho 'queue full' >&2\nexit 75\n").unwrap();
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
    fn test_missing_sendmail_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent-sendmail");
        let settings = sendmail_settings(missing.to_str().unwrap().to_string());

        let err = EmailWorker::check_sendmail_path(&settings).unwrap_err();
        assert!(err.contains("nonexistent-sendmail"), "error was {}", err);
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

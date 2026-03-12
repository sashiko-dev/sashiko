use crate::settings::SmtpSettings;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

pub struct EmailWorker {
    db: std::sync::Arc<crate::db::Database>,
    settings: SmtpSettings,
}

impl EmailWorker {
    pub fn new(db: std::sync::Arc<crate::db::Database>, settings: SmtpSettings) -> Self {
        Self { db, settings }
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
                        "Locked pending email ID {} for patch {}",
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

    async fn send_email(&self, email_row: &crate::db::EmailOutboxRow) -> anyhow::Result<()> {
        let mut builder = Message::builder()
            .from(self.settings.sender_address.parse()?)
            .subject(&email_row.subject);

        let to_addresses: Vec<String> = serde_json::from_str(&email_row.to_addresses)?;
        for to in to_addresses {
            builder = builder.to(to.parse()?);
        }

        let cc_addresses: Vec<String> = serde_json::from_str(&email_row.cc_addresses)?;
        for cc in cc_addresses {
            builder = builder.cc(cc.parse()?);
        }

        if !email_row.in_reply_to.is_empty() {
            builder = builder.header(lettre::message::header::InReplyTo::from(format!(
                "<{}>",
                email_row.in_reply_to
            )));
        }

        if !email_row.references_hdr.is_empty() {
            builder = builder.header(lettre::message::header::References::from(format!(
                "<{}>",
                email_row.references_hdr
            )));
        }

        let msg = builder
            .header(ContentType::TEXT_PLAIN)
            .body(email_row.body.clone())?;

        let mut mailer_builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.settings.server)
                .port(self.settings.port);

        if let (Some(user), Some(pass)) = (&self.settings.username, &self.settings.password) {
            let creds = Credentials::new(user.to_string(), pass.to_string());
            mailer_builder = mailer_builder.credentials(creds);
        }

        let mailer = mailer_builder.build();

        mailer.send(msg).await?;

        Ok(())
    }
}

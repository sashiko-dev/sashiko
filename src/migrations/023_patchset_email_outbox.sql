-- Support patchset-level email notifications (consolidated multi-patch
-- summaries and opt-in failure notifications) alongside per-patch emails, and
-- persist each patch's related pre-existing bug count so retries and series
-- summaries retain the count after unstamped reservations are released.
ALTER TABLE email_outbox ADD COLUMN patchset_id INTEGER REFERENCES patchsets(id);
ALTER TABLE email_outbox ADD COLUMN related_preexisting_count INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_email_outbox_patchset_id ON email_outbox(patchset_id);

-- Support patchset-level email notifications (consolidated multi-patch
-- summaries and opt-in failure notifications) alongside per-patch emails.
ALTER TABLE email_outbox ADD COLUMN patchset_id INTEGER REFERENCES patchsets(id);
CREATE INDEX IF NOT EXISTS idx_email_outbox_patchset_id ON email_outbox(patchset_id);

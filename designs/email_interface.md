# Sashiko Email Interface: Implementation Plan

## 1. Architecture Overview
To guarantee reliability and prevent duplicate emails during crashes, the system uses a decoupled **Transactional Outbox** pattern. The Review Worker determines *who* to email and *what* to say, saving it to a database queue in the same transaction that marks the review as complete. A separate, resilient Background Sender handles the actual SMTP transmission.

## 2. Configuration (`email_policy.toml`)
**Mechanism:** Read-on-demand for hot-reloading without daemon restarts.
**Logic:** Maps incoming mailing list addresses (from the patch's `To`/`Cc` headers) to routing rules.

```toml
[defaults]
reply_all = false
reply_to_author = true
cc_individuals = true
mute_all = false
cc = []

[subsystems.mm]
lists = ["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
reply_all = true
reply_to_author = true
cc_individuals = true

[subsystems.bpf]
lists = ["bpf@vger.kernel.org"]
reply_all = false
reply_to_author = true
cc_individuals = false

[subsystems.net]
lists = ["netdev@vger.kernel.org"]
mute_all = true
```

## 3. Recipient Resolution Engine
**Trigger:** Executed right after the AI generates the LKML-formatted inline review.
**Algorithm:**
1.  **Extract:** Get all addresses from the original patch email's `To` and `Cc` headers.
2.  **Match:** Find all subsystems in `email_policy.toml` whose `lists` contain any of the extracted addresses. (Fallback to `[defaults]`).
3.  **Resolve Conflicts (Safety First):**
    *   **Mute:** If *any* matched policy has `mute_all = true`, abort the email process completely.
    *   **Downgrade:** If *any* matched policy has `reply_all = false`, remove *all* configured mailing list addresses from the outgoing recipients. (Forces a private thread).
4.  **Build List:** Add author (`reply_to_author`), maintainers/individuals (`cc_individuals`), and `cc`.
5.  **Sanitize:** Deduplicate (case-insensitive) and remove Sashiko's own email address.

## 4. Database Schema (The Outbox)
**Table:** `email_outbox` (in `src/db.rs`)
**Columns:**
*   `id` (PK)
*   `patch_id` (FK)
*   `status` (`Pending`, `Sending`, `Sent`, `Failed`)
*   `to_addresses` / `cc_addresses` (JSON arrays of resolved recipients)
*   `subject` (Original subject prepended with `Re: `)
*   `in_reply_to` / `references` (Original patch's `Message-ID` for strict LKML threading)
*   `body` (Text payload)
*   `locked_at` (Timestamp for crash recovery)
*   `error_log` (Text)

## 5. Background Sender (`src/worker/email.rs`)
**Execution:** Dedicated Tokio background task.
1.  **Poll & Lock:** Finds `Pending` emails. Atomically updates to `Sending` and sets `locked_at`.
2.  **Transmit:** Uses the `lettre` crate to send via SMTP (`Settings.toml` credentials).
3.  **Acknowledge:** Updates DB to `Sent` on success, or `Failed` on hard error.
4.  **Crash Recovery:** Periodically sweeps for "Ghost" records (`status = Sending` where `locked_at` is older than 10 minutes) and resets them to `Pending`.

## 6. Email Formatting
**Format:** `text/plain; charset=utf-8`
**Structure:** Uses standard `>` inline quoting around the generated AI review text, followed by the finalized footer:

```text
{lkml_formatted_ai_review}

--
Sashiko AI review · https://sashiko.dev/patch/{msg_id}
```
## 7. UI / CLI Visibility
*   **State Machine:** Patches will transition through: `Ingested` -> `Reviewing` -> `Reviewed` -> `Email Pending` -> `Email Sent` (or `Email Skipped`).
*   **Logs/Output:** The frontend and CLI will explicitly display the delivery status alongside the exact `To` and `Cc` arrays so developers have full transparency into the bot's communication.

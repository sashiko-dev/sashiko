# Design: Scoped & Authorized Bug Request APIs

## Status
Proposed

## Context & Motivation
Sashiko's Linux bug pipeline analyzes and tracks pre-existing and patch-introduced vulnerabilities across the Linux kernel. Kernel vulnerabilities—particularly zero-day defects and critical concurrency/memory-safety bugs—are highly sensitive security assets before they are patched upstream.

Currently:
1. `GET /api/bugs` returns an unrestricted, paginated list of all bugs in the database to any caller.
2. `GET /api/bugs` and `GET /api/bug` decompress and transmit the complete multi-turn `logs` JSON (often 50KB–200KB per bug) on every request, even when only a high-level summary is needed.
3. There is no access control, role differentiation, or scoped filtering (subsystems vs. security priorities).

This design establishes a fine-grained, secure API architecture for querying bugs:
- **No Global Dumps**: Unrestricted querying of all bugs across the kernel is prohibited.
- **Maintainer Scoping**: Maintainers can only query and view bugs belonging to their designated subsystems.
- **Security Engineer Scoping**: Security engineers can query bugs across subsystems, but only at or above designated security priority/severity thresholds (e.g., High, Critical).
- **Log Segregation**: AI execution logs and reasoning traces are decoupled from bug metadata and accessed via a dedicated, lazily-loaded endpoint under separate authorization.

---

## Access Control & Persona Model

### 1. The Scoping Principle
To prevent mass vulnerability harvesting, bug retrieval is strictly scoped to the caller's authorized domains. The API will not support wildcard/unbounded queries unless authenticated with an internal `Admin`/`Auditor` scope.

### 2. Personas & Authorization Scopes

| Persona | Allowed Bug Scope | Typical Filter Constraints | Log Access |
| :--- | :--- | :--- | :--- |
| **Subsystem Maintainer** | Bugs where `subsystems` overlaps with the maintainer's assigned trees (e.g. `btrfs`, `net/sched`, `drm/msm`). | Must provide `subsystem` parameter matching allowed scopes. | Summary & Inline review by default. Logs require explicit scope. |
| **Security Engineer** | Bugs across subsystems that satisfy a security severity threshold (`High`, `Critical`). | Enforced `min_severity=high` (or `critical`). Lower-severity noise is filtered out. | Full access to logs and reproduction evidence for triage. |
| **Patch Author / Reporter** | Specific bugs discovered within patchsets submitted by the caller. | Scoped to `discovered_in_patchset_id` or `patch_id` matching the caller. | View review outcome, no internal worker prompts. |
| **Admin / Auditor** | Full access for internal benchmarking, migration, and system operations. | Unrestricted (used by CLI, local workers, and benchmarks). | Full access. |

---

## API Endpoints Specification

### 1. `GET /api/bugs` (Scoped Bug Listing)

Returns a paginated list of **lightweight bug summaries**. It explicitly excludes `logs`, `inline_review`, and `vector_json`.

#### Query Parameters
- `subsystem` (optional for security/admin, required for maintainers): e.g. `btrfs`, `fs/smb`. Must fall within the caller's authorized subsystem list.
- `min_severity` (optional for maintainers/admin, enforced for security): e.g. `high`, `critical`.
- `status` (optional): `open` (default), `dismissed`, `fixed`, `all`.
- `page` (optional, default `1`): Page number.
- `per_page` (optional, default `25`, max `100`): Results per page.

#### Enforcement Rules
- If an unprivileged caller requests `/api/bugs` without specifying an authorized `subsystem` or `min_severity`, the API responds with `403 Forbidden` (`{"error": "Query exceeds authorized scope. Specify a subsystem or severity filter."}`).
- A Maintainer attempting to filter on a subsystem outside their assigned scope receives `403 Forbidden`.
- A Security Engineer attempting to query `Low` or `Medium` severity bugs without subsystem ownership receives `403 Forbidden`.

#### Response Schema (`200 OK`)
```json
{
  "items": [
    {
      "id": 8,
      "slug": "pb-f1a2b3c4",
      "status": "open",
      "canonical_title": "ksmbd: reference count leak in parse_durable_handle_context()",
      "primary_subsystem": "smb/server",
      "affected_source_files": ["fs/smb/server/smb2pdu.c"],
      "severity": "High",
      "severity_explanation": "Unchecked error path leaves allocated file descriptor referenced.",
      "is_fixed": false,
      "created_at": 1724819000
    }
  ],
  "total": 1,
  "page": 1,
  "per_page": 25
}
```

---

### 2. `GET /api/bug?id=:id` or `GET /api/bug?slug=:slug` (Bug Details)

Returns the complete technical description and inline review comment for a single bug.

#### Authorization Check
- Evaluates whether the requested bug's `subsystems` match the caller's maintainer scope, OR if the bug's `severity` satisfies the caller's security scope, OR if the caller is an admin.
- If unauthorized, returns `404 Not Found` (or `403 Forbidden`) to prevent existence enumeration.

#### Response Schema (`200 OK`)
```json
{
  "id": 8,
  "slug": "pb-f1a2b3c4",
  "status": "open",
  "problem": "ksmbd: reference count leak in parse_durable_handle_context()",
  "severity": "High",
  "severity_explanation": "Unchecked error path leaves allocated file descriptor referenced.",
  "subsystems": ["smb/server"],
  "source_files": ["fs/smb/server/smb2pdu.c"],
  "locations": [
    {
      "file": "fs/smb/server/smb2pdu.c",
      "function_or_symbol": "parse_durable_handle_context",
      "line": 3721
    }
  ],
  "inline_review": "In parse_durable_handle_context(), dh_info->fp is allocated...\n\nfs/smb/server/smb2pdu.c:2810-2825\n    rc = parse_durable_handle_context(work, req, lc, &dh_info);",
  "introduced_in_commit": "1234567890ab (smb: add durable v2 handle support)",
  "verified_on_sha": "1b78070aaef6",
  "is_fixed": false,
  "created_at": 1724819000
}
```
*(Notice: `logs` is omitted from this payload.)*

---

### 3. `GET /api/bug/:id/logs` (Dedicated Log Retrieval)

Lazily retrieves the AI interaction history, reasoning traces, and verification steps for a bug.

#### Endpoint URL
- `GET /api/bug/{id}/logs` (or `GET /api/bug/logs?id={id}`)

#### Performance & Security Benefits
1. **Lazy Loading**: Only callers who actively open the "Execution Logs" or "AI Verification Steps" tab in the UI trigger this request.
2. **Database Bandwidth**: Avoids loading and decompressing large zlib compressed blobs on every list or detail request.
3. **Restricted Visibility**: Allows restricting raw AI prompts and token accounting to administrators and security auditors, while maintainers can consume the concise `inline_review`.

#### Response Schema (`200 OK`)
```json
{
  "bug_id": 8,
  "verified_on_sha": "1b78070aaef6",
  "turns": [
    {
      "role": "model",
      "thought": "Let's inspect parse_durable_handle_context in fs/smb/server/smb2pdu.c...",
      "tool_calls": [
        {
          "name": "git_read_files",
          "args": { "paths": ["fs/smb/server/smb2pdu.c"] }
        }
      ]
    }
  ]
}
```

---

## Database Layer Optimizations

### 1. Separate Summary vs. Detail Queries
Currently, `get_bugs_list` executes:
```sql
SELECT id, slug, status, problem, severity, severity_explanation, locations,
       subsystems, source_files, inline_review, logs, vector_json, ...
FROM bugs ORDER BY id DESC LIMIT ? OFFSET ?
```
This is updated to `get_bugs_summary_list`:
```sql
SELECT id, slug, status, problem, severity, severity_explanation,
       subsystems, source_files, is_fixed, created_at
FROM bugs
WHERE (:subsystem IS NULL OR EXISTS (
    SELECT 1 FROM json_each(bugs.subsystems) WHERE value = :subsystem
))
AND (:min_severity IS NULL OR severity >= :min_severity)
AND (:status IS NULL OR status = :status)
ORDER BY id DESC LIMIT :limit OFFSET :offset
```

### 2. Dedicated Log Query
```rust
pub async fn get_bug_logs(&self, bug_id: i64) -> Result<Option<String>> {
    let mut rows = self.conn.query(
        "SELECT logs FROM bugs WHERE id = ?",
        libsql::params![bug_id],
    ).await?;
    if let Ok(Some(row)) = rows.next().await {
        Ok(crate::compression::get_compressed_string_opt(&row, 0).unwrap_or_default())
    } else {
        Ok(None)
    }
}
```

---

## Phased Implementation Plan

### Phase 1: Log Segregation & Query Optimization (Immediate)
- Extract `logs` out of `get_bugs_list` and `get_bug`.
- Implement `GET /api/bug/logs?id=:id` and `db.get_bug_logs(id)`.
- Update web UI to lazily fetch logs when expanding the logs drawer.

### Phase 2: Scoped Query Parameters
- Add `subsystem`, `min_severity`, and `status` query parameters to `GET /api/bugs`.
- Update `Database::get_bugs_list` with parameterized SQL filtering.

### Phase 3: Authorization Middleware & Role Enforcement
- Introduce an Axum `AuthContext` extractor supporting API keys, tokens, or headers.
- Enforce the scoping matrix: reject global list queries from non-admin callers, enforce maintainer subsystem boundaries, and restrict security engineers to `min_severity >= High`.

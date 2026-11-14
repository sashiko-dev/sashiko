# Robust CLI final-answer envelopes

CLI providers ask for tool calls or a JSON content envelope. Report stages
require a plain-text report body. Some models put literal tabs or newlines
inside the content string, leaving invalid JSON around an otherwise usable
report. Falling back to that raw envelope then fails plain-text validation.

Clarify the distinction between the transport envelope and its content in
the shared CLI prompt. Apply the existing clean_json_string utility only
when recovering an otherwise invalid, single-field content envelope. The
recovered value must be a string. Malformed tool calls, mixed objects,
missing fields, and structural JSON errors retain their existing behavior.

Preserve all characters in the decoded report, including quoted code and
internal fences. Existing report validators remain authoritative. This
normalization must not invent findings, remove invalid report sections, or
reinterpret thought text as an answer. The already supported outer JSON
fence extraction remains unchanged.

Plain-text fallback is expected for some CLI stages; log it at debug rather
than warn. Stage validation continues to surface actual formatting failures.

Design review: reusing the workflow's existing control-character escape
utility keeps the repair deterministic. Restricting recovery to the exact
content envelope prevents malformed tool requests from gaining executable
meaning. Strict JSON parsing remains first, preserving valid responses.

Test literal control characters in fenced report envelopes, exact report
preservation, malformed-tool and structural-error fallback, and clarified
prompt framing. Verify the captured Gemma report offline before any new
model call, then run make check-pr and local self-review.

# B4 prerequisite thread hints

[Standalone prerequisite message IDs](DESIGN_STANDALONE_PREREQUISITE_MESSAGE_IDS.md)
extends this design when no patch-ID metadata is present. The optional-hint
behavior described here remains unchanged when explicit patch IDs exist.

## Problem

Patch-ID searches currently expand every matching Lore thread with `t=1`.
An unchanged patch shared across many revisions can therefore pull in far
more mail than the requested prerequisite series. The Rockchip USBDP v16
series hits the 500-message limit this way, although its three named
prerequisite threads individually fit within that limit and contain all
33 declared stable patch IDs.

## Resolution

Keep the ordered `prerequisite-patch-id` list authoritative. Treat unquoted
`prerequisite-message-id` lines as optional retrieval hints, not additional
dependencies. A bare message-ID hint must not cause any patches to be applied.

1. Resolve the required patch IDs from Sashiko's database first.
2. If IDs are missing, fetch the hinted Lore threads in declaration order,
   once per normalized message ID. Stop as soon as all required IDs are found.
3. Calculate stable patch IDs for downloaded patches and retain only exact
   matches from the required set. Ignore unrelated patches and thread order.
4. Resolve any remaining IDs with the existing patch-ID search fallback.
5. Return only the declared patches, in their original patch-ID order.

Malformed hints are ignored, and unavailable, oversized, or irrelevant
hinted threads permit the normal search fallback within the shared request
budget. Neither a hint nor a search may substitute a different patch or
silently omit a required prerequisite.
Do not recursively follow metadata from hinted threads: b4 already specifies
the exact flattened dependency list.

## Bounds and transport

Retain the 128-patch limit, 500-message per-response limit, compressed and
decompressed byte limits, HTTP timeout/retries, and bounded patch-ID workers.
Thread fetches and searches share the existing eight-operation budget;
failed operations count too. Retain only the first eight unique, valid hints
before performing remote work. An exhausted budget remains an explicit failure.
Use the merged prerequisite resolver's counter and patch cache, including
when a Based-on series contains b4 metadata. Based-on series retrieval, b4
thread hints, and patch-ID searches all count toward the same budget.

Accept canonical message IDs with optional enclosing angle brackets, reject
whitespace/control characters and overlong IDs, and keep requests on Lore's
configured origin. Reuse the URL path-segment encoding change from the merged
Patchew support so all thread callers supply raw canonical IDs and encoding
happens exactly once in the shared client.

Preserve the complete, redacted error chain in baseline failure logs so a
message-count limit is visible instead of only a generic retrieval failure.

## Compatibility and verification

Patch-ID-only declarations retain their existing lookup behavior. This does
not add bare message-ID/change-ID dependencies or change Patchew priority.
No database migration or automatic retry of past reviews is needed.

Use synthetic regression tests for multiple hinted series, exact patch-ID
matching and ordering, local/cache hits, duplicate and malformed hints,
missing/stale/oversized hints, search fallback, shared operation limits, and
fixed-origin URL encoding. Include a case where broad search would exceed
500 messages but scoped threads supply more than eight required patches in
three requests. Cover b4 priority when both formats appear, ignored metadata
inside hinted threads, and the shared budget with nested Based-on series.
Recheck the real Rockchip prerequisites locally without
committing their emails or making live-server changes.

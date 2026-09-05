# Sashiko False-Positive Guide

Report only regressions introduced by the patch and supported by a concrete
triggering path.

Before reporting:

1. inspect the full Result propagation path and any caller-side validation;
2. inspect the lock or ownership boundary rather than inferring a race from
   two functions that cannot run concurrently;
3. distinguish durable authoritative state from a recoverable derived file;
4. verify whether a Tokio child uses kill-on-drop and whether timeout handling
   explicitly waits for termination;
5. check whether command data is passed as a separate argument rather than
   interpolated into a shell command;
6. distinguish optional configuration defaults from explicit invalid values;
7. check whether the suspicious behavior exists on the base revision;
8. verify later patches in the same series before reporting an intermediate
   inconsistency.

Do not dismiss a finding merely because a deployment normally uses localhost,
a webhook normally comes from GitHub, a channel normally remains open, or an
AI provider normally responds. Conversely, do not report a missing defense
when an earlier authenticated layer or exact caller contract proves the input
cannot reach the code.

Test-only localhost fixtures, fake providers, and temporary repositories are
not production bypasses unless the patch makes them reachable in production.
The explicit unsafe-submit option is not a production recommendation, but its
continued existence alone is not a new regression.

# OpenAI-compatible temperature fallback

Some compatible endpoints reject the `temperature` parameter for particular
models. The configured model name does not reliably describe that capability,
so the client should learn it from a rejected request.

## Request flow

1. Send the request with its requested temperature unless this client instance
   has already learned that the endpoint rejects the parameter.
2. On HTTP 400 or 422, recognize only an error that explicitly identifies
   `temperature` as an unsupported parameter.
3. For that error, remember the capability and retry the same request once
   without `temperature`. Propagate the retry result without another fallback.
4. Return every other error unchanged. Requests without temperature have no
   reason to retry.

The learned state is shared across concurrent calls through an atomic flag.
Calls already in flight may each receive one rejection, but later calls omit
the parameter. A new process probes again so it can adapt to endpoint changes.

## Caching

The local response cache keys on the original `AiRequest`, model, endpoint,
and output cap. The fallback changes only the wire request after a rejected
attempt. The learned flag is not added to `cache_identity`, because it is a
runtime observation rather than a user setting and may change during a run.
Cached responses remain tied to the requested temperature and the same
endpoint and model. A capability change at an endpoint is subject to the
response cache's existing TTL, as with other server-side model changes.

## Verification

Test a compatible endpoint that rejects temperature once, then accepts the
retry and a later call without that parameter. Also test that unrelated client
errors and requests without temperature do not trigger a retry.

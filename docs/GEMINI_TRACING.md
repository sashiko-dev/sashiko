# Capturing Gemini API Traces

This guide explains how to capture a full trace of Gemini API requests and responses during a `sashiko` execution. These traces are essential for creating deterministic mock data and debugging complex interactions.

## Prerequisites

- Built binaries: `target/release/sashiko` and `target/release/gemini-proxy`.
- A valid Gemini API Key.

## Step-by-Step Instructions

### 1. Start the Capture Proxy

Run the proxy in a separate terminal tab. It will act as a Man-in-the-Middle, forwarding requests to the real Gemini API and logging them locally.

```bash
REAL_GEMINI_URL="https://generativelanguage.googleapis.com" target/release/gemini-proxy
```

The proxy listens on `http://localhost:3000` by default.

### 2. Launch Sashiko

Run `sashiko` configured to use the local proxy as its Gemini endpoint.

```bash
GEMINI_BASE_URL=http://localhost:3000 
LLM_API_KEY=your_api_key_here 
target/release/sashiko --debug
```

### 3. Submit a Workload

Trigger the processing by submitting a query to `sashiko`'s API.

```bash
curl -X POST http://localhost:8080/api/submit 
     -H "Content-Type: application/json" 
     -d '{
           "type": "remote",
           "sha": "alongshanumber",
           "repo": "url/to/repo"
         }'
```

### 4. Locate Trace Files

Wait for the review process to complete. The trace files will be generated in `tests/data/traces/` with the following patterns:

- `trace_<timestamp>_req.json`: The raw JSON request sent to Gemini.
- `trace_<timestamp>_resp.json`: The raw JSON response returned by Gemini.

## Technical Overview

The `gemini-proxy` utilizes the `GEMINI_BASE_URL` override capability of `sashiko`. It captures the byte stream of both requests and responses, saving them to disk before returning the data to the caller. This ensures that the captured data is exactly what was exchanged over the wire.

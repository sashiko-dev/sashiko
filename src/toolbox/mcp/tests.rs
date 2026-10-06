// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests against an in-process mock MCP server.

use super::*;
use crate::toolbox::ToolBox;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Mutex;

const TOKEN: &str = "s3cret-token";
const SESSION: &str = "session-1";

#[derive(Default)]
struct MockState {
    /// Answer with SSE streams instead of JSON bodies.
    sse: bool,
    /// Answer tools/list in two pages.
    paginate: bool,
    /// Fail the next request carrying the session id with 404.
    expire_session_once: bool,
    /// Delay every response by this long.
    delay: Option<Duration>,
    /// Redirect every request elsewhere.
    redirect: bool,
    /// Methods received, in order.
    methods: Vec<String>,
    /// Authorization headers received.
    auth: Vec<Option<String>>,
    /// Session ids received on requests after initialize.
    sessions: Vec<Option<String>>,
}

type Shared = Arc<Mutex<MockState>>;

fn listing() -> Vec<Value> {
    vec![
        json!({
            "name": "search_docs",
            "description": "Search the documentation.",
            "inputSchema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }
        }),
        json!({ "name": "read-page", "description": "Read one section." }),
        json!({ "name": "delete_everything", "description": "Not allowed." }),
    ]
}

fn call_result(args: &Value) -> Value {
    match args["name"].as_str() {
        Some("search_docs") => json!({
            "content": [
                { "type": "text", "text": format!("hits for {}", args["arguments"]["query"]) },
                { "type": "image", "data": "...", "mimeType": "image/png" }
            ]
        }),
        Some("read-page") => json!({
            "content": [{ "type": "text", "text": "x".repeat(100) }]
        }),
        _ => json!({ "content": [{ "type": "text", "text": "no such tool" }], "isError": true }),
    }
}

async fn handler(State(state): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let msg: Value = serde_json::from_str(&body).unwrap_or_default();
    let method = msg["method"].as_str().unwrap_or_default().to_string();
    let (sse, paginate, delay, redirect, expire) = {
        let mut s = state.lock().unwrap();
        s.methods.push(method.clone());
        s.auth.push(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        );
        let session = headers
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if method != "initialize" {
            s.sessions.push(session.clone());
        }
        let expire = s.expire_session_once && session.is_some() && method == "tools/call";
        if expire {
            s.expire_session_once = false;
        }
        (s.sse, s.paginate, s.delay, s.redirect, expire)
    };
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    if redirect {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "http://127.0.0.1:1/elsewhere")],
        )
            .into_response();
    }
    if expire {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(id) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock", "version": "0" }
        }),
        "tools/list" if paginate && msg["params"]["cursor"].is_null() => {
            json!({ "tools": listing()[..1], "nextCursor": "page2" })
        }
        "tools/list" if paginate => json!({ "tools": listing()[1..] }),
        "tools/list" => json!({ "tools": listing() }),
        "tools/call" => call_result(&msg["params"]),
        _ => {
            let reply = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "token=abc unknown" } });
            return axum::Json(reply).into_response();
        }
    };
    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut resp = if sse {
        // A notification first, which the client must skip.
        let progress =
            json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
        let stream =
            format!("event: message\ndata: {progress}\n\nevent: message\ndata: {reply}\n\n");
        ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
    } else {
        axum::Json(reply).into_response()
    };
    if method == "initialize" {
        resp.headers_mut()
            .insert(SESSION_HEADER, SESSION.parse().unwrap());
    }
    resp
}

async fn start(state: MockState) -> (String, Shared) {
    let shared = Arc::new(Mutex::new(state));
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(handler))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{}/mcp", addr), shared)
}

fn server_settings(url: &str) -> McpServerSettings {
    McpServerSettings {
        name: "docs".to_string(),
        url: url.to_string(),
        bearer_token_env: None,
        allowed_tools: vec![
            "search_docs".to_string(),
            "read-page".to_string(),
            "not_offered".to_string(),
        ],
        stages: vec!["hardware".to_string()],
        prompt_hint: Some("Look up the documentation.".to_string()),
        timeout_secs: 5,
        max_output_bytes: 64,
    }
}

async fn discover(url: &str) -> Vec<McpTool> {
    discover_server_with_token(&server_settings(url), Some(TOKEN.to_string()))
        .await
        .unwrap()
}

fn context() -> SashikoToolContext {
    SashikoToolContext {
        worktree_path: std::path::PathBuf::from("."),
        prompts_path: None,
        active_patch_files: std::sync::RwLock::new(Vec::new()),
        virtual_head: std::sync::RwLock::new(None),
        cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
    }
}

#[tokio::test]
async fn test_discovery_exposes_only_allowed_tools() {
    let (url, state) = start(MockState::default()).await;
    let tools = discover(&url).await;
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["mcp_docs_read_page", "mcp_docs_search_docs"]);

    let search = &tools[1];
    assert!(search.description().starts_with("[MCP server docs] Search"));
    assert!(search.parameters().get("$schema").is_none());
    assert_eq!(search.parameters()["required"], json!(["query"]));
    // A tool listed without a schema gets an empty object schema.
    assert_eq!(tools[0].parameters()["type"], "object");

    let s = state.lock().unwrap();
    assert_eq!(
        s.methods,
        ["initialize", "notifications/initialized", "tools/list"]
    );
    let bearer = format!("Bearer {}", TOKEN);
    assert!(s.auth.iter().all(|a| a.as_deref() == Some(bearer.as_str())));
    // The session issued by initialize is sent on every later request.
    assert!(s.sessions.iter().all(|id| id.as_deref() == Some(SESSION)));
}

#[tokio::test]
async fn test_discovery_follows_pagination() {
    let (url, _) = start(MockState {
        paginate: true,
        ..Default::default()
    })
    .await;
    assert_eq!(discover(&url).await.len(), 2);
}

#[tokio::test]
async fn test_calls_return_text_and_skip_other_content() {
    let (url, _) = start(MockState::default()).await;
    let tools = discover(&url).await;
    let out = tools[1]
        .call(json!({ "query": "widget" }), &context())
        .await
        .unwrap();
    assert_eq!(out["source"], "mcp:docs");
    assert_eq!(out["truncated"], false);
    assert_eq!(
        out["content"],
        "hits for \"widget\"\n\n[image content omitted]"
    );
}

#[tokio::test]
async fn test_long_results_are_truncated() {
    let (url, _) = start(MockState::default()).await;
    let tools = discover(&url).await;
    let out = tools[0].call(json!({}), &context()).await.unwrap();
    assert_eq!(out["truncated"], true);
    assert_eq!(out["content"].as_str().unwrap().len(), 64);
    assert!(out["next_page_hint"].is_string());
}

#[tokio::test]
async fn test_sse_responses_skip_notifications() {
    let (url, _) = start(MockState {
        sse: true,
        ..Default::default()
    })
    .await;
    let tools = discover(&url).await;
    assert_eq!(tools.len(), 2);
    let out = tools[1]
        .call(json!({ "query": "gadget" }), &context())
        .await
        .unwrap();
    assert!(out["content"].as_str().unwrap().contains("gadget"));
}

#[tokio::test]
async fn test_expired_session_reconnects_once() {
    let (url, state) = start(MockState {
        expire_session_once: true,
        ..Default::default()
    })
    .await;
    let tools = discover(&url).await;
    let out = tools[1]
        .call(json!({ "query": "q" }), &context())
        .await
        .unwrap();
    assert!(out["content"].is_string());
    let s = state.lock().unwrap();
    assert_eq!(s.methods.iter().filter(|m| *m == "initialize").count(), 2);
}

#[tokio::test]
async fn test_tool_errors_are_results_not_failures() {
    let (url, _) = start(MockState::default()).await;
    let tools = discover(&url).await;
    let mut gone = tools[0].clone();
    gone.remote_name = "removed".to_string();
    let out = gone.call(json!({}), &context()).await.unwrap();
    assert_eq!(out["error"], "no such tool");
}

#[tokio::test]
async fn test_rpc_errors_are_redacted() {
    let (url, _) = start(MockState::default()).await;
    let client = McpClient::new(&server_settings(&url), None).unwrap();
    client.initialize().await.unwrap();
    let err = client.request("bogus", json!({})).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("token=[REDACTED]"), "{}", text);
    assert!(!text.contains("abc"));
}

#[tokio::test]
async fn test_redirects_are_not_followed() {
    let (url, state) = start(MockState {
        redirect: true,
        ..Default::default()
    })
    .await;
    let err = discover_server_with_token(&server_settings(&url), Some(TOKEN.to_string()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("307"), "{}", err);
    assert_eq!(state.lock().unwrap().methods.len(), 1);
}

#[tokio::test]
async fn test_slow_servers_time_out() {
    let (url, _) = start(MockState {
        delay: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .await;
    let mut settings = server_settings(&url);
    settings.timeout_secs = 1;
    assert!(discover_server_with_token(&settings, None).await.is_err());
}

#[tokio::test]
async fn test_unreachable_or_misconfigured_servers_are_skipped() {
    let mut bad_token = server_settings("http://127.0.0.1:1/mcp");
    bad_token.bearer_token_env = Some("SASHIKO_TEST_MCP_TOKEN_THAT_IS_NEVER_SET".to_string());
    let mut unreachable = server_settings("http://127.0.0.1:1/mcp");
    unreachable.name = "down".to_string();
    let settings = McpSettings {
        servers: vec![bad_token, unreachable],
    };
    let found = McpTools::discover(&settings).await;
    assert!(found.is_empty());
    assert!(found.hints.is_empty());
}

#[tokio::test]
async fn test_discovered_tools_register_in_a_toolbox() {
    let (url, _) = start(MockState::default()).await;
    let settings = McpSettings {
        servers: vec![server_settings(&url)],
    };
    let found = McpTools::discover(&settings).await;
    assert_eq!(found.hints.len(), 1);
    assert_eq!(found.hints[0].tools.len(), 2);

    let mut tb = ToolBox::new(std::path::PathBuf::from("."), None);
    for tool in &found.tools {
        tb.register_tool(tool.clone());
    }
    let out = tb
        .call("MCP_DOCS_SEARCH_DOCS", json!({ "query": "q" }))
        .await
        .unwrap();
    assert_eq!(out["source"], "mcp:docs");
}

#[test]
fn test_sse_parser_waits_for_complete_events() {
    let partial = b"data: {\"id\": 1}\n";
    assert!(sse_messages(partial).is_empty());
    let complete = b"data: {\"id\": 1}\r\n\r\n: comment\n\ndata: {\"id\":\ndata: 2}\n\n";
    let msgs = sse_messages(complete);
    assert_eq!(msgs, vec![json!({ "id": 1 }), json!({ "id": 2 })]);
}

#[test]
fn test_truncation_respects_char_boundaries() {
    assert_eq!(truncate_utf8("héllo", 2), "h");
    assert_eq!(truncate_utf8("héllo", 3), "hé");
    assert_eq!(truncate_utf8("abc", 10), "abc");
}

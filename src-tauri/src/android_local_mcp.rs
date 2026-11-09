//! Explicit local-only MCP transport. No LAN listener, no Cloudflare process,
//! no persistent URL, and no tools beyond the audited read-only OpenAI adapter.
//! A random secret path limits accidental access from other on-device apps.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::header::{CONTENT_TYPE, HOST, ORIGIN};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};

const BODY_LIMIT: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Default)]
struct LocalSession {
    generation: u64,
    worker: Option<JoinHandle<()>>,
    local_url: Option<String>,
    handled_tools: u64,
}

#[derive(Clone, Default)]
pub struct AndroidLocalMcp {
    session: Arc<Mutex<LocalSession>>,
}

impl AndroidLocalMcp {
    fn snapshot(&self) -> Result<Value, String> {
        let state = self
            .session
            .lock()
            .map_err(|_| "Local MCP state unavailable")?;
        let live = state
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished());
        Ok(json!({
            "state": if live {"listening"} else {"stopped"},
            "localUrl": if live {state.local_url.as_deref()} else {None},
            "toolCallsHandled": if live {state.handled_tools} else {0},
            "remoteVerified": false,
        }))
    }

    async fn start(&self) -> Result<Value, String> {
        // Listen on loopback only, never 0.0.0.0, and bind only following
        // explicit user initiation. The OS selects an unused port.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "Cannot bind Android localhost MCP port")?;
        let port = listener
            .local_addr()
            .map_err(|_| "Local MCP address unavailable")?
            .port();
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let path = format!("/mcp/{secret}");
        let local_url = format!("http://127.0.0.1:{port}{path}");
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Local MCP state unavailable")?;
        if session
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Err("Stop the existing local MCP listener first".into());
        }
        session.generation = session.generation.wrapping_add(1);
        let generation = session.generation;
        session.handled_tools = 0;
        session.local_url = Some(local_url);
        let state = self.session.clone();
        session.worker = Some(tokio::spawn(async move {
            serve(listener, path, port, state, generation).await;
        }));
        drop(session);
        self.snapshot()
    }

    fn stop(&self) -> Result<Value, String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Local MCP state unavailable")?;
        session.generation = session.generation.wrapping_add(1);
        if let Some(worker) = session.worker.take() {
            worker.abort();
        }
        session.local_url = None;
        session.handled_tools = 0;
        drop(session);
        self.snapshot()
    }
}

#[tauri::command]
pub async fn android_local_mcp_status(
    state: tauri::State<'_, AndroidLocalMcp>,
) -> Result<Value, String> {
    state.snapshot()
}

#[tauri::command]
pub async fn android_local_mcp_start(
    state: tauri::State<'_, AndroidLocalMcp>,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required".into());
    }
    if !cfg!(target_os = "android") {
        return Err("Local MCP is Android-only".into());
    }
    state.start().await
}

#[tauri::command]
pub async fn android_local_mcp_stop(
    state: tauri::State<'_, AndroidLocalMcp>,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required".into());
    }
    if !cfg!(target_os = "android") {
        return Err("Local MCP is Android-only".into());
    }
    state.stop()
}

fn reply(status: StatusCode, body: Bytes) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .header("cache-control", "no-store")
        .header("connection", "close")
        .body(Full::new(body))
        .expect("static HTTP response")
}

fn short_error(status: StatusCode) -> Response<Full<Bytes>> {
    reply(status, Bytes::from_static(b"{}"))
}

async fn handle(
    request: Request<Incoming>,
    path: &str,
    port: u16,
    state: &Arc<Mutex<LocalSession>>,
    generation: u64,
) -> Response<Full<Bytes>> {
    // No cross-origin browser requests, no unsafe methods or forwarding to
    // other paths. A proxy must preserve the local Host and the secret path.
    if request.uri().path() != path || request.uri().query().is_some() {
        return short_error(StatusCode::NOT_FOUND);
    }
    if request.headers().contains_key(ORIGIN) {
        return short_error(StatusCode::FORBIDDEN);
    }
    if request.headers().get(HOST).and_then(|h| h.to_str().ok())
        != Some(format!("127.0.0.1:{port}").as_str())
    {
        return short_error(StatusCode::BAD_REQUEST);
    }
    if request.method() != Method::POST {
        return short_error(StatusCode::METHOD_NOT_ALLOWED);
    }
    if !request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.eq_ignore_ascii_case("application/json")
                || v.to_ascii_lowercase().starts_with("application/json;")
        })
    {
        return short_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if request.body().size_hint().lower() > BODY_LIMIT as u64 {
        return short_error(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let mut body = request.into_body();
    let mut raw = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return short_error(StatusCode::BAD_REQUEST);
        };
        if let Ok(data) = frame.into_data() {
            if data.len() > BODY_LIMIT.saturating_sub(raw.len()) {
                return short_error(StatusCode::PAYLOAD_TOO_LARGE);
            }
            raw.extend_from_slice(&data);
        }
    }
    let Ok(rpc) = serde_json::from_slice::<Value>(&raw) else {
        return short_error(StatusCode::BAD_REQUEST);
    };
    let result = super::android_openai_tunnel::handle_rpc(&rpc);
    let Some(payload) = result.payload else {
        return reply(StatusCode::ACCEPTED, Bytes::new());
    };
    if result.successful_tool_call {
        if let Ok(mut session) = state.lock() {
            if session.generation == generation {
                session.handled_tools = session.handled_tools.saturating_add(1);
            }
        }
    }
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    reply(StatusCode::OK, Bytes::from(bytes))
}

async fn serve(
    listener: TcpListener,
    path: String,
    port: u16,
    state: Arc<Mutex<LocalSession>>,
    generation: u64,
) {
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();
    loop {
        let permit = match limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let (socket, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(_) => break,
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let scope = path.clone();
        let state = state.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                let state = state.clone();
                let scope = scope.clone();
                async move {
                    Ok::<_, Infallible>(handle(request, &scope, port, &state, generation).await)
                }
            });
            let conn = http1::Builder::new()
                .keep_alive(false)
                .max_buf_size(16 * 1024)
                .serve_connection(TokioIo::new(socket), service);
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, conn).await;
        });
        while tasks.try_join_next().is_some() {}
    }
    // Dropping JoinSet cancels all outstanding connections. No worker
    // survives a Stop or the Android process shutdown.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_mcp_serves_bounded_read_only_rpc_and_stops() {
        let app = AndroidLocalMcp::default();
        let started = app.start().await.unwrap();
        assert_eq!(started["state"], "listening");
        assert_eq!(started["remoteVerified"], false);
        let url = started["localUrl"].as_str().unwrap();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.split('/').last().unwrap().len() == 64);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let init = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{}}}))
            .send()
            .await
            .unwrap();
        assert!(init.status().is_success());
        assert_eq!(
            init.json::<Value>().await.unwrap()["result"]["protocolVersion"],
            "2025-06-18"
        );
        let tools = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 2);
        let echo = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"local-safe"}}}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(echo["result"]["content"][0]["text"], "local-safe");
        assert_eq!(app.snapshot().unwrap()["toolCallsHandled"], 1);
        assert_eq!(app.snapshot().unwrap()["remoteVerified"], false);
        let unsafe_tool = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"run_shell","arguments":{"command":"id"}}}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert!(unsafe_tool["error"].is_object());
        assert_eq!(
            app.snapshot().unwrap()["toolCallsHandled"],
            1,
            "Rejected tools must never count as executed"
        );
        assert_eq!(
            client
                .post(format!("{url}wrong"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client.get(url).send().await.unwrap().status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            client
                .post(url)
                .header("Origin", "https://evil.example")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(url)
                .header("Host", "evil.example")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .post(url)
                .header("Content-Type", "text/plain")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            client
                .post(url)
                .header("Content-Type", "application/json")
                .body("{broken")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .post(url)
                .body(vec![b'a'; BODY_LIMIT + 1])
                .header("Content-Type", "application/json")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let old_url = url.to_owned();
        app.stop().unwrap();
        assert_eq!(app.snapshot().unwrap()["state"], "stopped");
        assert!(app.snapshot().unwrap()["localUrl"].is_null());
        let restarted = app.start().await.unwrap();
        assert_ne!(
            restarted["localUrl"].as_str().unwrap(),
            old_url,
            "Stopping and restarting must rotate the secret MCP URL"
        );
        assert_eq!(restarted["toolCallsHandled"], 0);
        app.stop().unwrap();
    }
}

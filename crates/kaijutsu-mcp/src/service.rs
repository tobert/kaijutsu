//! Streamable HTTP service mode for `kaijutsu-mcp` over Unix domain sockets
//! and optional loopback TCP, with unified `POST /hook` dispatch.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hyper::body::{Bytes, Incoming};
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited, combinators::BoxBody};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_service::Service as TowerService;

use crate::KaijutsuMcp;
use crate::hook_listener::{HookListener, hook_runtime_dir};
use crate::hook_types::HookEvent;

/// Maximum accepted JSON payload size on `POST /hook` (4 MiB).
const MAX_HOOK_BODY_BYTES: usize = 4 * 1024 * 1024;

type BoxResponse = Response<BoxBody<Bytes, Infallible>>;

/// Configuration for running `kaijutsu-mcp` in Streamable HTTP service mode.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Optional Unix domain socket path (e.g. `$XDG_RUNTIME_DIR/kaijutsu/mcp.sock`).
    pub listen_unix: Option<PathBuf>,
    /// Optional loopback TCP address (e.g. `127.0.0.1:8787`).
    pub listen_http: Option<SocketAddr>,
    /// Cancellation token triggering graceful shutdown of all listeners.
    pub cancellation_token: CancellationToken,
}

/// Default service Unix domain socket path: `$XDG_RUNTIME_DIR/kaijutsu/mcp.sock`.
pub fn default_service_socket_path() -> Option<PathBuf> {
    Some(hook_runtime_dir()?.join("kaijutsu").join("mcp.sock"))
}

/// Routes `POST /hook` events to a registered per-session [`HookListener`] or
/// falls back to the service's shared default listener.
pub struct HookRouter {
    default_listener: Arc<HookListener>,
    sessions: RwLock<HashMap<String, Arc<HookListener>>>,
}

impl HookRouter {
    /// Create a router backed by a default [`HookListener`].
    pub fn new(default_listener: Arc<HookListener>) -> Self {
        Self {
            default_listener,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Register a dedicated listener for `session_id`.
    pub fn register(&self, session_id: String, listener: Arc<HookListener>) {
        self.sessions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id, listener);
    }

    /// Remove a previously registered listener for `session_id`.
    pub fn unregister(&self, session_id: &str) {
        self.sessions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    fn listener_for(&self, session_id: Option<&str>) -> Arc<HookListener> {
        if let Some(sid) = session_id {
            let guard = self.sessions.read().unwrap_or_else(|e| e.into_inner());
            if let Some(listener) = guard.get(sid) {
                return Arc::clone(listener);
            }
        }
        Arc::clone(&self.default_listener)
    }

    /// Dispatch a JSON `POST /hook` body and return `(status, json_body)`.
    pub async fn dispatch_json(&self, body: &[u8]) -> (StatusCode, String) {
        let value: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                let err = serde_json::json!({ "error": format!("Invalid JSON: {e}") });
                return (StatusCode::BAD_REQUEST, err.to_string());
            }
        };

        if value.get("ping").and_then(|v| v.as_bool()) == Some(true) {
            let ping = self.default_listener.handle_ping().await;
            let json = serde_json::to_string(&ping).unwrap_or_else(|_| "{}".to_string());
            return (StatusCode::OK, json);
        }

        let event: HookEvent = match serde_json::from_value(value) {
            Ok(e) => e,
            Err(e) => {
                let err = serde_json::json!({ "error": format!("Invalid HookEvent: {e}") });
                return (StatusCode::BAD_REQUEST, err.to_string());
            }
        };

        let listener = self.listener_for(event.session_id.as_deref());
        if event.event == "ping" {
            let ping = listener.handle_ping().await;
            let json = serde_json::to_string(&ping).unwrap_or_else(|_| "{}".to_string());
            return (StatusCode::OK, json);
        }

        let response = listener.handle_event(event).await;
        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        (StatusCode::OK, json)
    }
}

/// Serve Streamable HTTP MCP (`/mcp`), unified `POST /hook`, and `GET /health`
/// on the configured Unix socket and/or TCP loopback listeners until
/// `config.cancellation_token` is cancelled.
pub async fn serve(
    mcp: KaijutsuMcp,
    hook_router: Arc<HookRouter>,
    config: ServiceConfig,
) -> anyhow::Result<()> {
    if config.listen_unix.is_none() && config.listen_http.is_none() {
        anyhow::bail!("service mode requires at least one of --listen-unix or --listen-http");
    }

    let cancel = config.cancellation_token;
    let mcp_for_factory = mcp.clone();
    let http_config = StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp_for_factory.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let mut tasks = tokio::task::JoinSet::new();

    #[cfg(unix)]
    if let Some(unix_path) = config.listen_unix {
        let listener = HookListener::bind_socket(&unix_path).await?;
        tracing::info!(path = %unix_path.display(), "MCP Streamable HTTP service listening on Unix socket");
        let svc = mcp_service.clone();
        let hooks = Arc::clone(&hook_router);
        let ct = cancel.clone();
        tasks.spawn(async move {
            serve_unix(listener, unix_path, svc, hooks, ct).await
        });
    }

    #[cfg(not(unix))]
    if config.listen_unix.is_some() {
        anyhow::bail!("--listen-unix is only supported on Unix platforms");
    }

    if let Some(addr) = config.listen_http {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(addr = %local_addr, "MCP Streamable HTTP service listening on TCP");
        let svc = mcp_service.clone();
        let hooks = Arc::clone(&hook_router);
        let ct = cancel.clone();
        tasks.spawn(async move {
            serve_tcp(listener, svc, hooks, ct).await
        });
    }

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                cancel.cancel();
                return Err(e);
            }
            Err(e) => {
                cancel.cancel();
                return Err(anyhow::anyhow!("service listener task failed: {e}"));
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn serve_unix(
    listener: tokio::net::UnixListener,
    socket_path: PathBuf,
    mcp_service: StreamableHttpService<KaijutsuMcp, LocalSessionManager>,
    hook_router: Arc<HookRouter>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let svc = mcp_service.clone();
                        let hooks = Arc::clone(&hook_router);
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req| {
                                route_request(req, svc.clone(), Arc::clone(&hooks))
                            });
                            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                                tracing::debug!("UDS HTTP connection closed: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("UDS accept error: {e}");
                    }
                }
            }
        }
    }

    if let Err(e) = tokio::fs::remove_file(&socket_path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %socket_path.display(), "Failed to unlink service Unix socket: {e}");
    }
    Ok(())
}

async fn serve_tcp(
    listener: TcpListener,
    mcp_service: StreamableHttpService<KaijutsuMcp, LocalSessionManager>,
    hook_router: Arc<HookRouter>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let svc = mcp_service.clone();
                        let hooks = Arc::clone(&hook_router);
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req| {
                                route_request(req, svc.clone(), Arc::clone(&hooks))
                            });
                            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                                tracing::debug!("TCP HTTP connection closed: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("TCP accept error: {e}");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn route_request(
    req: Request<Incoming>,
    mut mcp_service: StreamableHttpService<KaijutsuMcp, LocalSessionManager>,
    hook_router: Arc<HookRouter>,
) -> Result<BoxResponse, Infallible> {
    let path = req.uri().path();
    let method = req.method();

    if method == Method::GET && path == "/health" {
        return Ok(json_response(StatusCode::OK, r#"{"ok":true}"#.to_string()));
    }

    if method == Method::POST && path == "/hook" {
        let limited = Limited::new(req.into_body(), MAX_HOOK_BODY_BYTES);
        let collected = match limited.collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) => {
                let err = serde_json::json!({ "error": format!("Failed to read hook body: {e}") });
                return Ok(json_response(StatusCode::BAD_REQUEST, err.to_string()));
            }
        };
        let (status, body) = hook_router.dispatch_json(&collected).await;
        return Ok(json_response(status, body));
    }

    if path == "/mcp" || path.starts_with("/mcp/") {
        return mcp_service.call(req).await;
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))
        .body(Full::new(Bytes::from_static(b"Not Found")).boxed())
        .expect("valid 404 response"))
}

fn json_response(status: StatusCode, body: String) -> BoxResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(Full::new(Bytes::from(body)).boxed())
        .expect("valid JSON response")
}

/// Send a JSON hook event to a `kaijutsu-mcp` service Unix domain socket via
/// HTTP/1.1 `POST /hook`. Returns `Ok(None)` if the socket does not exist.
#[cfg(unix)]
pub async fn send_hook_http_uds(
    socket_path: &Path,
    body_json: &str,
) -> anyhow::Result<Option<String>> {
    if !socket_path.exists() {
        return Ok(None);
    }

    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

    let conn_task = tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("UDS hook HTTP client connection ended: {e}");
        }
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/hook")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body_json.to_owned())))?;

    let resp = sender.send_request(req).await?;
    let body_bytes = resp.into_body().collect().await?.to_bytes();
    drop(sender);
    let _ = conn_task.await;

    let text = String::from_utf8(body_bytes.to_vec())?;
    Ok(Some(text))
}

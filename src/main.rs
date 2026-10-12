mod attachments;
mod auth;
mod config;
mod mail;
mod markdown;
mod mcp;
mod template;
mod uploads;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware as mw;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;

use config::Config;
use mcp::{AppState, Reply, RpcRequest};
use uploads::UploadErr;

/// legacy HTTP+SSE 会话表：session_id → 事件广播
#[derive(Debug, Default)]
struct Sessions {
    map: Mutex<HashMap<String, broadcast::Sender<Value>>>,
}

impl Sessions {
    async fn register(&self) -> (String, broadcast::Receiver<Value>) {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = broadcast::channel(64);
        self.map.lock().await.insert(id.clone(), tx);
        (id, rx)
    }

    async fn publish(&self, id: &str, value: Value) {
        let mut map = self.map.lock().await;
        if let Some(tx) = map.get(id) {
            if tx.send(value).is_err() {
                map.remove(id); // 接收端已消失，清理会话
            }
        }
    }
}

#[derive(Clone)]
struct ServerState {
    app: Arc<AppState>,
    sessions: Arc<Sessions>,
}

// ---------- 认证 ----------

/// 命中的 API key（明文候选，仅用于本次请求内的核对）。
fn authenticated_key(headers: &HeaderMap, st: &ServerState) -> Option<String> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let candidate = bearer.or(api_key);
    if st.app.auth.verify(candidate.as_deref()) {
        candidate
    } else {
        None
    }
}

/// 命中的 API key 的 SHA-256：作为带外上传条目的归属标识（不驻留明文）。
fn caller_hash(headers: &HeaderMap, st: &ServerState) -> Option<[u8; 32]> {
    authenticated_key(headers, st).map(|k| Sha256::digest(k.as_bytes()).into())
}

async fn auth_middleware(
    State(st): State<ServerState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: mw::Next,
) -> Response {
    if authenticated_key(&headers, &st).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

// ---------- 工具函数 ----------

/// 限量读取请求体；超限返回 413
async fn read_body_limited(body: axum::body::Body, limit: usize) -> Result<Bytes, Box<Response>> {
    axum::body::to_bytes(body, limit).await.map_err(|_| {
        Box::new(
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": "request body too large"})),
            )
                .into_response(),
        )
    })
}

fn parse_error_response(code: i64, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

fn jsonrpc_response(reply: &mcp::RpcResponse) -> Value {
    serde_json::to_value(reply).unwrap_or_else(|_| json!({ "jsonrpc": "2.0", "id": null }))
}

// ---------- Streamable HTTP: POST /mcp ----------

async fn mcp_handler(
    State(st): State<ServerState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let limit = st.app.config.security.max_request_bytes;
    let bytes = match read_body_limited(body, limit).await {
        Ok(b) => b,
        Err(r) => return *r,
    };

    let req: RpcRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => return parse_error_response(-32700, &e.to_string()),
    };

    // 认证中间件已放行，此处必然取到 key
    let Some(caller) = caller_hash(&headers, &st) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let reply = mcp::handle_request(&st.app, caller, req).await;
    let msg = match reply {
        Reply::Response(r) => jsonrpc_response(&r),
        Reply::Silent => return StatusCode::ACCEPTED.into_response(),
    };

    // MCP streamable HTTP：客户端 Accept text/event-stream 时以 SSE 返回
    let wants_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    if wants_sse {
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into())
        );
        let body2 = "event: end_of_stream\ndata: {}\n\n".to_string();
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            format!("{}{}", body, body2),
        )
            .into_response()
    } else {
        (StatusCode::OK, Json(msg)).into_response()
    }
}

// ---------- Legacy HTTP+SSE: GET /sse + POST /messages ----------

async fn sse_handler(
    State(st): State<ServerState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (id, rx) = st.sessions.register().await;
    let endpoint = format!("/messages?session_id={}", id);
    tracing::debug!(session = %id, "legacy SSE 会话已建立");

    let first = Event::default().event("endpoint").data(endpoint);
    let stream = stream::once(async move { Ok::<_, Infallible>(first) }).chain(
        tokio_stream::wrappers::BroadcastStream::new(rx).map(|v| {
            let v = match v {
                Ok(v) => v,
                Err(_) => json!({}), // 滞后(lagged)等内部错误，发空消息保持流健康
            };
            Ok::<_, Infallible>(
                Event::default()
                    .json_data(v)
                    .unwrap_or_else(|_| Event::default().data("{}")),
            )
        }),
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(serde::Deserialize)]
struct MessagesQuery {
    session_id: String,
}

async fn messages_handler(
    State(st): State<ServerState>,
    Query(q): Query<MessagesQuery>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    if q.session_id.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let limit = st.app.config.security.max_request_bytes;
    let bytes = match read_body_limited(body, limit).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let req: RpcRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => return parse_error_response(-32700, &e.to_string()),
    };
    let Some(caller) = caller_hash(&headers, &st) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let reply = mcp::handle_request(&st.app, caller, req).await;
    if let Reply::Response(r) = reply {
        let msg = jsonrpc_response(&r);
        st.sessions.publish(&q.session_id, msg).await;
    }
    StatusCode::ACCEPTED.into_response()
}

// ---------- 带外上传: PUT /upload/:id ----------
//
// 不挂 Bearer 认证中间件：URL 里的 32 位随机 id 本身就是一次性凭据，
// 且条目绑定创建者的 API key（owner），别的 key 无法消费。

fn upload_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// 边收边落盘：不把整个 body 读进内存。
/// 超过 `security.max_attachment_bytes` 立即中止（删文件 + abort 条目）返回 413。
async fn upload_handler(
    State(st): State<ServerState>,
    Path(id): Path<String>,
    body: axum::body::Body,
) -> Response {
    let max_bytes = st.app.config.security.max_attachment_bytes as u64;
    // Pending -> Uploading；未知/已过期 404，状态不对 409
    if let Err(e) = st.app.uploads.begin_upload(&id) {
        return match e {
            UploadErr::NotFound => upload_error(StatusCode::NOT_FOUND, "unknown or expired upload"),
            UploadErr::Conflict => upload_error(StatusCode::CONFLICT, "upload already used"),
        };
    }

    let dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(e) => {
            st.app.uploads.abort(&id);
            return upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("创建临时目录失败: {}", e),
            );
        }
    };
    let path = dir.path().join("payload");

    let mut file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            st.app.uploads.abort(&id);
            return upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("创建临时文件失败: {}", e),
            );
        }
    };

    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;

    loop {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                st.app.uploads.abort(&id);
                return upload_error(
                    StatusCode::BAD_REQUEST,
                    &format!("读取上传数据失败: {}", e),
                );
            }
            None => break,
        };
        size += chunk.len() as u64;
        if size > max_bytes {
            st.app.uploads.abort(&id);
            return upload_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("文件超过单附件上限 {} 字节", max_bytes),
            );
        }
        if st.app.uploads.exceeds_pending_bytes(size) {
            st.app.uploads.abort(&id);
            return upload_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "待消费上传总字节超过上限 {} 字节",
                    st.app.uploads.max_pending_bytes()
                ),
            );
        }
        hasher.update(&chunk);
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
            st.app.uploads.abort(&id);
            return upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("写入临时文件失败: {}", e),
            );
        }
    }
    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
        st.app.uploads.abort(&id);
        return upload_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("写入临时文件失败: {}", e),
        );
    }
    drop(file);

    let sha256 = uploads::digest_hex(&hasher.finalize());
    let filename = st.app.uploads.filename_of(&id).unwrap_or_default();
    if let Err(e) = st.app.uploads.finish_upload(&id, dir, path, size, sha256.clone()) {
        return match e {
            UploadErr::NotFound => upload_error(StatusCode::NOT_FOUND, "unknown or expired upload"),
            UploadErr::Conflict => upload_error(StatusCode::CONFLICT, "upload already used"),
        };
    }

    tracing::info!(
        event = "upload_completed",
        upload_id_prefix = uploads::short_id(&id),
        filename = %filename,
        size = size,
    );

    (
        StatusCode::OK,
        Json(json!({
            "upload_id": id,
            "filename": filename,
            "size": size,
            "sha256": sha256,
        })),
    )
        .into_response()
}

// ---------- 服务启动 ----------

fn build_router(st: ServerState) -> Router {
    let authed = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .route_layer(mw::from_fn_with_state(st.clone(), auth_middleware));
    Router::new()
        .merge(authed)
        // 带外上传：不走 Bearer 认证（id 即一次性凭据）
        .route("/upload/:id", put(upload_handler))
        .route("/healthz", get(healthz))
        .with_state(st)
}

async fn healthz() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let conf_path = std::env::var("SMTP_MCP_CONFIG").unwrap_or_else(|_| {
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "config.toml".to_string())
    });
    let cfg = match Config::load(&PathBuf::from(&conf_path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置文件加载失败: {}", e);
            std::process::exit(1);
        }
    };

    let app = AppState {
        auth: auth::Authenticator::new(&cfg.auth.keys),
        uploads: uploads::UploadStore::new(
            cfg.upload.ttl_secs,
            cfg.upload.max_pending,
            cfg.upload.max_pending_bytes,
        ),
        config: cfg,
    };
    let st = ServerState {
        app: Arc::new(app),
        sessions: Arc::new(Sessions::default()),
    };
    // 后台定期清理过期上传（惰性清理之外的兜底，保证闲置时磁盘文件也会被删）
    tokio::spawn(cleanup_uploads_loop(st.app.clone()));
    let router = build_router(st.clone());

    let addr: SocketAddr = match format!(
        "{}:{}",
        st.app.config.server.addr, st.app.config.server.port
    )
    .parse()
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("监听地址解析失败: {}", e);
            std::process::exit(1);
        }
    };
    tracing::info!(%addr, keys = st.app.config.auth.keys.len(), tls = st.app.config.tls.is_some(), "smtp-mcp-server 启动");

    let result = if let Some(tls_cfg) = &st.app.config.tls {
        match rustls_tls_bind(addr, tls_cfg, router).await {
            Ok(()) => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("绑定 {} 失败: {}", addr, e);
                std::process::exit(1);
            }
        };
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| format!("HTTP 服务异常退出: {}", e))
    };

    if let Err(e) = result {
        eprintln!("启动失败: {}", e);
        std::process::exit(1);
    }
}

async fn rustls_tls_bind(
    addr: SocketAddr,
    tls_cfg: &config::TlsConfig,
    router: Router,
) -> Result<(), String> {
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        PathBuf::from(&tls_cfg.cert),
        PathBuf::from(&tls_cfg.key),
    )
    .await
    .map_err(|e| format!("加载 TLS 证书失败: {}", e))?;
    tracing::info!("启用可选 TLS（直连）");
    axum_server::bind_rustls(addr, tls_config)
        .serve(router.into_make_service())
        .await
        .map_err(|e| format!("TLS 服务异常退出: {}", e))
}

/// 每 60s 清理一次过期上传条目（磁盘文件随条目里的 TempDir drop 删除）。
async fn cleanup_uploads_loop(app: Arc<AppState>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        app.uploads.cleanup_expired();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

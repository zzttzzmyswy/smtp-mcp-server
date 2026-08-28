use crate::attachments::{self, AttachmentInput};
use crate::auth::Authenticator;
use crate::config::Config;
use crate::mail;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVER_NAME: &str = "smtp-mcp-server";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const TOOL_NAME: &str = "send_email";

pub struct AppState {
    pub config: Config,
    pub auth: Authenticator,
}

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

pub enum Reply {
    /// 需要回给客户端的 JSON-RPC 响应
    Response(RpcResponse),
    /// 通知类消息 / 解析前失败，无需响应 body（HTTP 层按需返回）
    Silent,
}

fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Reply {
    Reply::Response(RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
        }),
    })
}

fn method_not_found(id: Option<Value>, method: &str) -> Reply {
    error(id, -32601, format!("方法不存在: {}", method))
}

fn invalid_params(id: Option<Value>, msg: impl Into<String>) -> Reply {
    error(id, -32602, msg)
}

fn send_email_tool_schema() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "通过 SMTP 发送一封通知邮件。支持多收件人与可选附件（附件 base64 编码，单附件与单次总大小上限见服务配置）。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "邮件主题" },
                "body": { "type": "string", "description": "邮件正文（纯文本）" },
                "receiver": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "收件人邮箱列表，至少一个"
                },
                "attachments": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "filename": { "type": "string" },
                            "content": { "type": "string", "description": "base64 编码的文件内容" }
                        },
                        "required": ["filename", "content"]
                    },
                    "description": "可选附件列表（每个 ≤ 10MB）"
                }
            },
            "required": ["subject", "body", "receiver"]
        }
    })
}

/// 处理单个 JSON-RPC 请求。返回值直接决定客户端是否收到响应。
/// 通用性设计为纯逻辑，便于单元测试；HTTP/SSE 传输层在 main.rs。
pub async fn handle_request(state: &AppState, req: RpcRequest) -> Reply {
    let is_notification = req.id.is_none();
    match req.method.as_str() {
        "initialize" => Reply::Response(RpcResponse {
            jsonrpc: "2.0",
            id: req.id,
            result: Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            })),
            error: None,
        }),
        "notifications/initialized" | "notifications/cancelled" => Reply::Silent,
        "ping" => {
            if is_notification {
                Reply::Silent
            } else {
                Reply::Response(RpcResponse {
                    jsonrpc: "2.0",
                    id: req.id,
                    result: Some(json!({})),
                    error: None,
                })
            }
        }
        "tools/list" => Reply::Response(RpcResponse {
            jsonrpc: "2.0",
            id: req.id,
            result: Some(json!({ "tools": [send_email_tool_schema()] })),
            error: None,
        }),
        "tools/call" => handle_tools_call(state, req.id, req.params).await,
        other => method_not_found(req.id, other),
    }
}

#[derive(Debug, Deserialize)]
struct ToolsCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct SendEmailArgs {
    subject: String,
    body: String,
    receiver: Vec<String>,
    #[serde(default)]
    attachments: Vec<AttachmentInput>,
}

async fn handle_tools_call(state: &AppState, id: Option<Value>, params: Option<Value>) -> Reply {
    let Some(params) = params else {
        return invalid_params(id, "tools/call 缺少 params");
    };
    let call: ToolsCallParams = match serde_json::from_value(params) {
        Ok(c) => c,
        Err(e) => return invalid_params(id, format!("tools/call 参数非法: {}", e)),
    };
    if call.name != TOOL_NAME {
        return invalid_params(id, format!("未知工具: {}", call.name));
    }
    let args: SendEmailArgs = match serde_json::from_value(call.arguments) {
        Ok(a) => a,
        Err(e) => return tool_error(id, format!("send_email 参数非法: {}", e)),
    };

    // 附件校验 + 落盘（超出大小/非法类型在此被拒绝，不进入发送流程）
    let staged = match attachments::stage_attachments(&args.attachments, &state.config.security) {
        Ok(s) => s,
        Err(e) => return tool_error(id, e),
    };

    // 审计日志：时间/收件人/主题长度，不记录正文、密钥、授权码
    tracing::info!(
        event = "send_email",
        receivers = ?args.receiver,
        subject_len = args.subject.len(),
        attachments = staged.files.len(),
        total_attachment_bytes = staged.files.iter().map(|f| {
            std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0)
        }).sum::<u64>(),
    );

    let report = match mail::send_email(
        &state.config.smtp,
        &state.config.security.receiver_whitelist,
        args.receiver,
        args.subject,
        args.body,
        &staged,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return tool_error(id, e),
    };

    let text = format!(
        "邮件已发送: 收件人 {}，主题长度 {}，附件 {} 个（{} 字节）",
        report.receivers.join(", "),
        report.subject.chars().count(),
        report.attachment_count,
        report.total_size_bytes
    );
    Reply::Response(RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": false
        })),
        error: None,
    })
}

fn tool_error(id: Option<Value>, msg: String) -> Reply {
    tracing::warn!(event = "send_email_rejected", reason = %msg);
    Reply::Response(RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({
            "content": [ { "type": "text", "text": msg } ],
            "isError": true
        })),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn state() -> AppState {
        let cfg = crate::config::Config::parse(
            r#"
[server]
[auth]
keys = ["test-key"]
[smtp]
host = "smtp.126.com"
port = 465
tls = "ssl"
from = "sender@126.com"
user = "sender@126.com"
pass = "placeholder"
"#,
        )
        .unwrap();
        AppState {
            auth: Authenticator::new(&cfg.auth.keys),
            config: cfg,
        }
    }

    fn make_request(method: &str, params: Value) -> RpcRequest {
        RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: method.into(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn initialize_ok() {
        let s = state();
        let req = make_request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }),
        );
        match handle_request(&s, req).await {
            Reply::Response(r) => {
                let v = r.result.unwrap();
                assert_eq!(v["protocolVersion"], "2025-06-18");
                assert_eq!(v["serverInfo"]["name"], SERVER_NAME);
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn tools_list_contains_send_email() {
        let s = state();
        let req = make_request("tools/list", json!({}));
        match handle_request(&s, req).await {
            Reply::Response(r) => {
                let result = r.result.unwrap();
                let tools = result["tools"].as_array().unwrap();
                assert!(tools.iter().any(|t| t["name"] == TOOL_NAME));
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn unknown_method_error() {
        let s = state();
        let req = make_request("bogus/method", json!({}));
        match handle_request(&s, req).await {
            Reply::Response(r) => {
                let e = r.error.unwrap();
                assert_eq!(e.code, -32601);
            }
            _ => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn notification_tools_call_no_id_is_silent() {
        let s = state();
        let req = RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: None,
            method: "notifications/initialized".into(),
            params: Some(json!({})),
        };
        assert!(matches!(handle_request(&s, req).await, Reply::Silent));
    }

    #[tokio::test]
    async fn tools_call_missing_required_args() {
        let s = state();
        let req = make_request(
            "tools/call",
            json!({ "name": "send_email", "arguments": { "subject": "x" } }),
        );
        match handle_request(&s, req).await {
            Reply::Response(r) => {
                let res = r.result.unwrap();
                assert_eq!(res["isError"], true);
                assert!(res["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("参数非法"));
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn tools_call_rejects_oversized_attachment_without_smtp() {
        let s = state();
        // 构造一个超过 10MB 的 base64 附件（即使 SMTP 不可达，也应在校验阶段被拒）
        let big = vec![b'x'; 10 * 1024 * 1024 + 1];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&big);
        let req = make_request(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["a@b.com"],
                    "attachments": [ { "filename": "x.pdf", "content": b64 } ]
                }
            }),
        );
        match handle_request(&s, req).await {
            Reply::Response(r) => {
                let res = r.result.unwrap();
                assert_eq!(res["isError"], true);
                assert!(
                    res["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("超过单附件上限"),
                    "{:?}",
                    res
                );
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn tools_call_rejects_disallowed_type_before_smtp() {
        let mut st = state();
        st.config.security.allowed_attachment_extensions = vec!["txt".into()];
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"evil");
        let req = make_request(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["a@b.com"],
                    "attachments": [ { "filename": "evil.exe", "content": b64 } ]
                }
            }),
        );
        match handle_request(&st, req).await {
            Reply::Response(r) => {
                let res = r.result.unwrap();
                assert_eq!(res["isError"], true);
                assert!(res["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("不在白名单"));
            }
            _ => panic!("expected response"),
        }
    }
}

use crate::attachments::{self, AttachmentInput};
use crate::auth::Authenticator;
use crate::config::Config;
use crate::mail;
use crate::markdown::BodyFormat;
use crate::uploads::UploadStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVER_NAME: &str = "smtp-mcp-server";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const TOOL_NAME: &str = "send_email";
pub const UPLOAD_TOOL_NAME: &str = "create_upload";

pub struct AppState {
    pub config: Config,
    pub auth: Authenticator,
    pub uploads: UploadStore,
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
        "description": "通过 SMTP 发送一封通知邮件。支持多收件人与可选附件。附件有两条通道：(1) 大文件/二进制走带外上传——先调 create_upload 拿 upload_id 与 upload_url，用 `curl -T` 上传字节，再把 upload_id 放进 attachments；(2) base64 content 内联——字符需由模型逐个生成，仅适合约 10KB 以内的小文件，较大或二进制文件请勿使用。建议为每个附件提供 sha256 以便发现传输损坏。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "邮件主题" },
                "body": { "type": "string", "description": "正文。默认 auto：自动识别 Markdown（标题/列表/引用/表格/加粗等）并按 Markdown 渲染，普通文本按纯文本排版；也可用 body_format 显式指定 text 或 markdown" },
                "receiver": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "收件人邮箱列表，至少一个"
                },
                "html_body": { "type": "string", "description": "可选：AI Agent 自带完整 HTML 正文（可含 <table class=\"mail-table\">/<img> 等），提供后优先级最高，直接作为邮件正文；宽表会被自动包进横向滚动容器，窄屏出现左右滚动条，请勿自行加 overflow 包裹" },
                "body_format": { "type": "string", "enum": ["auto", "text", "markdown"], "description": "可选：body 渲染格式。auto（默认）自动检测是否 Markdown；text 强制按纯文本排版；markdown 强制按 Markdown 转换 HTML" },
                "attachments": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "filename": { "type": "string", "description": "纯文件名。走 upload_id 时可省略（默认取 create_upload 时的文件名），提供则必须与上传时一致" },
                            "content": { "type": "string", "description": "base64 编码的文件内容（与 upload_id 二选一）。解码时容忍空白/URL-safe 字母表/缺失 padding/data URI 前缀。仅适合约 10KB 以内的小文件" },
                            "upload_id": { "type": "string", "description": "create_upload 返回的上传凭据（与 content 二选一）。文件字节须已通过 `curl -fsS -T <本地文件路径> '<upload_url>'` 直传完成" },
                            "sha256": { "type": "string", "description": "可选但强烈建议：原始字节的 SHA-256 hex（64 字符，大小写均可）。用于发现内容在传输中被截断或改写；不匹配将被拒绝" }
                        },
                        "required": []
                    },
                    "description": "可选附件列表，每项必须且只能提供 content 或 upload_id 之一。大文件走 create_upload + curl -T（上限 10MB）；base64 content 仅适合约 10KB 以内的小文件——内容字符需由模型逐个生成，越大越容易丢字符或被截断。建议同时传 sha256 以便发现损坏"
                }
            },
            "required": ["subject", "body", "receiver"]
        }
    })
}

fn create_upload_tool_schema() -> Value {
    json!({
        "name": UPLOAD_TOOL_NAME,
        "description": "为附件申请一个一次性带外上传凭据，用于把大文件（上限 10MB）从 agent 本地直接传给服务，避免 base64 经模型输入。两步流程：(1) 调用本工具得到 upload_id 与 upload_url；(2) 在自己的 shell 里执行 `curl -fsS -T <本地文件路径> '<upload_url>'` 直传字节（PUT，无需认证头，URL 里的 id 即一次性凭据）；(3) 调用 send_email，在 attachments 里传 {\"upload_id\":\"…\",\"sha256\":\"…\"}。upload_id 有有效期（见返回的 expires_in_secs），过期或被 send_email 消费后即失效，发送失败也需重新上传。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "filename": { "type": "string", "description": "附件的纯文件名（不含路径），扩展名须在服务端白名单内。send_email 时可省略 filename，默认使用此处的值" }
            },
            "required": ["filename"]
        }
    })
}

/// 处理单个 JSON-RPC 请求。返回值直接决定客户端是否收到响应。
/// 通用性设计为纯逻辑，便于单元测试；HTTP/SSE 传输层在 main.rs。
/// `caller` 为命中的 API key 的 SHA-256，用作带外上传条目的归属标识。
pub async fn handle_request(state: &AppState, caller: [u8; 32], req: RpcRequest) -> Reply {
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
            result: Some(json!({ "tools": [send_email_tool_schema(), create_upload_tool_schema()] })),
            error: None,
        }),
        "tools/call" => handle_tools_call(state, caller, req.id, req.params).await,
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
    /// AI Agent 自带完整 HTML 正文（最高优先级，未提供时渲染 body）
    #[serde(default)]
    html_body: Option<String>,
    /// body 渲染格式：auto（默认）| text | markdown
    #[serde(default, deserialize_with = "deserialize_body_format")]
    body_format: BodyFormat,
}

fn deserialize_body_format<'de, D>(d: D) -> Result<BodyFormat, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
}

async fn handle_tools_call(
    state: &AppState,
    caller: [u8; 32],
    id: Option<Value>,
    params: Option<Value>,
) -> Reply {
    let Some(params) = params else {
        return invalid_params(id, "tools/call 缺少 params");
    };
    let call: ToolsCallParams = match serde_json::from_value(params) {
        Ok(c) => c,
        Err(e) => return invalid_params(id, format!("tools/call 参数非法: {}", e)),
    };
    match call.name.as_str() {
        TOOL_NAME => send_email(state, caller, id, call.arguments).await,
        UPLOAD_TOOL_NAME => create_upload(state, caller, id, call.arguments),
        other => invalid_params(id, format!("未知工具: {}", other)),
    }
}

/// `create_upload`：申请一次性带外上传凭据，返回 upload_id 与 PUT url。
fn create_upload(state: &AppState, caller: [u8; 32], id: Option<Value>, arguments: Value) -> Reply {
    #[derive(Deserialize)]
    struct CreateUploadArgs {
        filename: String,
    }
    let args: CreateUploadArgs = match serde_json::from_value(arguments) {
        Ok(a) => a,
        Err(e) => return tool_error(id, UPLOAD_TOOL_NAME, format!("create_upload 参数非法: {}", e)),
    };
    let filename = args.filename.trim().to_string();
    if let Err(e) = attachments::validate_filename(&filename, &state.config.security) {
        return tool_error(id, UPLOAD_TOOL_NAME, e);
    }

    // 未配置对外前缀时无法给出可直传的 URL
    let Some(base_url) = state.config.upload.base_url() else {
        return tool_error(
            id,
            UPLOAD_TOOL_NAME,
            "服务端未配置 upload.public_base_url，无法使用上传方式".into(),
        );
    };

    let upload_id = match state.uploads.create(caller, filename.clone()) {
        Ok(u) => u,
        Err(e) => return tool_error(id, UPLOAD_TOOL_NAME, e),
    };
    let upload_url = format!("{}/upload/{}", base_url, upload_id);

    // 审计日志：不记录完整 id
    tracing::info!(
        event = "create_upload",
        upload_id_prefix = crate::uploads::short_id(&upload_id),
        filename = %filename,
        ttl_secs = state.config.upload.ttl_secs,
    );

    let payload = json!({
        "upload_id": upload_id,
        "upload_url": upload_url,
        "expires_in_secs": state.config.upload.ttl_secs,
        "max_bytes": state.config.security.max_attachment_bytes,
        "hint": format!(
            "curl -fsS -T <本地文件路径> '{}' ，成功后在 send_email.attachments 里传 {{\"upload_id\":\"{}\",\"sha256\":\"…\"}}",
            upload_url, upload_id
        ),
    });
    Reply::Response(RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({
            "content": [ { "type": "text", "text": payload.to_string() } ],
            "isError": false
        })),
        error: None,
    })
}

async fn send_email(
    state: &AppState,
    caller: [u8; 32],
    id: Option<Value>,
    arguments: Value,
) -> Reply {
    let args: SendEmailArgs = match serde_json::from_value(arguments) {
        Ok(a) => a,
        Err(e) => return tool_error(id, TOOL_NAME, format!("send_email 参数非法: {}", e)),
    };

    // 附件校验 + 落盘（超出大小/非法类型在此被拒绝，不进入发送流程）
    let resolver = state.uploads.resolver(caller);
    let staged =
        match attachments::stage_attachments(&args.attachments, &state.config.security, &resolver) {
            Ok(s) => s,
            Err(e) => return tool_error(id, TOOL_NAME, e),
        };

    let render_kind = if args.html_body.is_some() {
        "custom"
    } else if args.body_format.use_markdown(&args.body) {
        "markdown"
    } else {
        "text"
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
        html = args.html_body.is_some(),
        template = render_kind,
    );

    let html_content = crate::template::render(&crate::template::RenderOptions {
        subject: args.subject.clone(),
        body: args.body.clone(),
        html_body: args.html_body.clone(),
        body_format: args.body_format,
    });

    let report = match mail::send_email(
        &state.config.smtp,
        &state.config.security.receiver_whitelist,
        args.receiver,
        args.subject,
        html_content,
        &staged,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return tool_error(id, TOOL_NAME, e),
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

fn tool_error(id: Option<Value>, tool: &str, msg: String) -> Reply {
    tracing::warn!(event = "tool_rejected", tool = tool, reason = %msg);
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
        state_with(Some("https://mu.zztweb.top/smtp-mcp"), &[])
    }

    /// 可定制 upload.public_base_url 与扩展名白名单的测试状态
    fn state_with(base_url: Option<&str>, allowed_exts: &[&str]) -> AppState {
        state_full(base_url, allowed_exts, None, None)
    }

    /// 完整可定制：`max_attachment_bytes` / `upload.max_pending`
    fn state_full(
        base_url: Option<&str>,
        allowed_exts: &[&str],
        max_attachment_bytes: Option<usize>,
        max_pending: Option<usize>,
    ) -> AppState {
        let upload_section = match base_url {
            Some(u) => format!("[upload]\npublic_base_url = \"{}\"\n", u),
            None => String::new(),
        };
        let ext_list = allowed_exts
            .iter()
            .map(|e| format!("\"{}\"", e))
            .collect::<Vec<_>>()
            .join(", ");
        let att_cap = max_attachment_bytes.unwrap_or(10 * 1024 * 1024);
        let pending = max_pending.unwrap_or(16);
        let cfg = crate::config::Config::parse(&format!(
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
[security]
max_attachment_bytes = {att_cap}
allowed_attachment_extensions = [{exts}]
[upload]
public_base_url = "{base}"
max_pending = {pending}
"#,
            att_cap = att_cap,
            exts = ext_list,
            base = base_url.unwrap_or(""),
            pending = pending,
        ))
        .unwrap();
        let _ = upload_section;
        AppState {
            auth: Authenticator::new(&cfg.auth.keys),
            uploads: UploadStore::new(
                cfg.upload.ttl_secs,
                cfg.upload.max_pending,
                cfg.upload.max_pending_bytes,
            ),
            config: cfg,
        }
    }

    /// 测试用调用者身份（API key 的 SHA-256）
    const CALLER: [u8; 32] = [1u8; 32];
    /// 另一个 API key 的身份，用于验证 owner 隔离
    const OTHER_CALLER: [u8; 32] = [2u8; 32];

    /// 调 `create_upload` 并返回解析后的返回 JSON（含 upload_id / upload_url）
    async fn create_upload_for(state: &AppState, caller: [u8; 32], filename: &str) -> Value {
        let req = make_request(
            "tools/call",
            json!({ "name": "create_upload", "arguments": { "filename": filename } }),
        );
        let Reply::Response(r) = handle_request(state, caller, req).await else {
            panic!("expected response");
        };
        let res = r.result.unwrap();
        assert_eq!(res["isError"], false, "unexpected error: {}", res);
        serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// 取工具返回的 isError / text
    async fn call_tool(state: &AppState, caller: [u8; 32], args: Value) -> (bool, String) {
        let req = make_request("tools/call", args);
        let Reply::Response(r) = handle_request(state, caller, req).await else {
            panic!("expected response");
        };
        let res = r.result.unwrap();
        (
            res["isError"].as_bool().unwrap(),
            res["content"][0]["text"].as_str().unwrap().to_string(),
        )
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
        match handle_request(&s, CALLER, req).await {
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
        match handle_request(&s, CALLER, req).await {
            Reply::Response(r) => {
                let result = r.result.unwrap();
                let tools = result["tools"].as_array().unwrap();
                assert!(tools.iter().any(|t| t["name"] == TOOL_NAME));
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn tools_list_schema_exposes_sha256() {
        let s = state();
        let req = make_request("tools/list", json!({}));
        let Reply::Response(r) = handle_request(&s, CALLER, req).await else {
            panic!("expected response");
        };
        let result = r.result.unwrap();
        let tool = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == TOOL_NAME)
            .expect("send_email tool present");

        let props = &tool["inputSchema"]["properties"]["attachments"]["items"]["properties"];
        assert!(
            props["sha256"].is_object(),
            "attachments.items.properties 应含 sha256，实际: {}",
            props
        );
        let desc = props["sha256"]["description"].as_str().unwrap();
        assert!(desc.contains("SHA-256"), "sha256 description: {}", desc);

        // description 应写明 base64 仅适合小文件
        let tool_desc = tool["description"].as_str().unwrap();
        assert!(
            tool_desc.contains("10KB"),
            "tool description: {}",
            tool_desc
        );
        let att_desc = tool["inputSchema"]["properties"]["attachments"]["description"]
            .as_str()
            .unwrap();
        assert!(
            att_desc.contains("10KB"),
            "attachments description: {}",
            att_desc
        );

        // 原有必填字段（filename/content）改为二选一，schema 里不再有 required
        let required = tool["inputSchema"]["properties"]["attachments"]["items"]["required"]
            .as_array()
            .unwrap();
        assert!(
            required.is_empty(),
            "content 与 upload_id 二选一，items 不应有 required: {:?}",
            required
        );
        // 两条通道都出现在 schema 里
        assert!(props["content"].is_object(), "props: {}", props);
        assert!(props["upload_id"].is_object(), "props: {}", props);
    }

    #[tokio::test]
    async fn tools_list_returns_both_tools() {
        let s = state();
        let req = make_request("tools/list", json!({}));
        let Reply::Response(r) = handle_request(&s, CALLER, req).await else {
            panic!("expected response");
        };
        let result = r.result.unwrap();
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![TOOL_NAME, UPLOAD_TOOL_NAME]);
    }

    #[tokio::test]
    async fn unknown_tool_name_rejected() {
        let s = state();
        let req = make_request(
            "tools/call",
            json!({ "name": "no_such_tool", "arguments": {} }),
        );
        let Reply::Response(r) = handle_request(&s, CALLER, req).await else {
            panic!("expected response");
        };
        let e = r.error.unwrap();
        assert_eq!(e.code, -32602);
        assert!(e.message.contains("未知工具"), "message: {}", e.message);
    }

    // ---- create_upload ----

    #[tokio::test]
    async fn create_upload_returns_url_and_id() {
        let s = state();
        let v = create_upload_for(&s, CALLER, "report.pdf").await;
        let id = v["upload_id"].as_str().unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(
            v["upload_url"].as_str().unwrap(),
            format!("https://mu.zztweb.top/smtp-mcp/upload/{}", id)
        );
        assert_eq!(v["expires_in_secs"], 900);
        assert_eq!(v["max_bytes"], 10 * 1024 * 1024);
        assert!(v["hint"].as_str().unwrap().contains("curl -fsS -T"));
    }

    #[tokio::test]
    async fn create_upload_strips_trailing_slash_from_base_url() {
        let s = state_with(Some("https://mu.zztweb.top/smtp-mcp/"), &[]);
        let v = create_upload_for(&s, CALLER, "a.pdf").await;
        let url = v["upload_url"].as_str().unwrap();
        assert!(
            url.starts_with("https://mu.zztweb.top/smtp-mcp/upload/"),
            "url: {}",
            url
        );
        assert!(!url.contains("//upload"), "url: {}", url);
    }

    #[tokio::test]
    async fn create_upload_without_public_base_url_is_error() {
        let s = state_with(None, &[]);
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({ "name": "create_upload", "arguments": { "filename": "a.pdf" } }),
        )
        .await;
        assert!(is_error);
        assert_eq!(text, "服务端未配置 upload.public_base_url，无法使用上传方式");
        // 未创建任何条目
        assert_eq!(s.uploads.uploaded_bytes(), 0);
    }

    #[tokio::test]
    async fn create_upload_rejects_bad_filename() {
        let s = state_with(Some("https://x/y"), &["pdf"]);
        for (name, needle) in [
            ("", "缺少文件名"),
            ("../etc/passwd", "非法附件文件名"),
            ("dir/a.pdf", "非法附件文件名"),
            ("evil.exe", "不在白名单"),
        ] {
            let (is_error, text) = call_tool(
                &s,
                CALLER,
                json!({ "name": "create_upload", "arguments": { "filename": name } }),
            )
            .await;
            assert!(is_error, "filename {:?} should be rejected", name);
            assert!(text.contains(needle), "filename {:?} text: {}", name, text);
        }
    }

    #[tokio::test]
    async fn create_upload_rejects_missing_filename_arg() {
        let s = state();
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({ "name": "create_upload", "arguments": {} }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("参数非法"), "text: {}", text);
    }

    #[tokio::test]
    async fn create_upload_rejects_over_max_pending() {
        // max_pending 在构造 Store 时就固定，因此用 state_full 直接给到 2
        let s = state_full(Some("https://x/y"), &[], None, Some(2));
        let _a = create_upload_for(&s, CALLER, "a.pdf").await;
        let _b = create_upload_for(&s, CALLER, "b.pdf").await;
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({ "name": "create_upload", "arguments": { "filename": "c.pdf" } }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("上限 2"), "text: {}", text);
    }

    // ---- send_email 的 upload_id 路径 ----

    #[tokio::test]
    async fn send_email_rejects_content_and_upload_id_together() {
        let s = state();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hi");
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "filename": "a.pdf", "content": b64, "upload_id": "deadbeef" } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("必须且只能提供 content 或 upload_id 之一"), "text: {}", text);
    }

    #[tokio::test]
    async fn send_email_rejects_neither_content_nor_upload_id() {
        let s = state();
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "filename": "a.pdf" } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("必须且只能提供 content 或 upload_id 之一"), "text: {}", text);
    }

    #[tokio::test]
    async fn send_email_rejects_upload_id_of_other_caller() {
        let s = state();
        let v = create_upload_for(&s, CALLER, "a.pdf").await;
        let id = v["upload_id"].as_str().unwrap();

        // 另一个 key 用同一个 upload_id：owner 不一致
        let (is_error, text) = call_tool(
            &s,
            OTHER_CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("不属于当前 API key"), "text: {}", text);
    }

    #[tokio::test]
    async fn send_email_rejects_pending_upload_id() {
        // 只创建、未上传字节
        let s = state();
        let v = create_upload_for(&s, CALLER, "a.pdf").await;
        let id = v["upload_id"].as_str().unwrap();
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("尚未完成上传"), "text: {}", text);
    }

    #[tokio::test]
    async fn send_email_with_consumed_upload_id_is_rejected() {
        let s = state();
        let v = create_upload_for(&s, CALLER, "a.pdf").await;
        let id = v["upload_id"].as_str().unwrap();
        // 直接把它推成已上传状态（真实 PUT 由集成测试覆盖）
        s.uploads.begin_upload(id).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("payload");
        std::fs::write(&path, b"payload").unwrap();
        s.uploads
            .finish_upload(id, dir, path, 7, crate::uploads::sha256_hex(b"payload"))
            .unwrap();

        // 第一次消费成功（收件人白名单未配置，故进入 SMTP 阶段并因地址非法/网络失败而报错——
        // 这里只验证条目已被取走）
        let _ = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id } ]
                }
            }),
        )
        .await;

        // 已消费：再次使用被拒
        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("不存在或已过期"), "text: {}", text);
    }

    #[tokio::test]
    async fn send_email_rejects_upload_sha256_mismatch() {
        let s = state();
        let v = create_upload_for(&s, CALLER, "a.pdf").await;
        let id = v["upload_id"].as_str().unwrap();
        s.uploads.begin_upload(id).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("payload");
        std::fs::write(&path, b"real bytes").unwrap();
        s.uploads
            .finish_upload(id, dir, path, 10, crate::uploads::sha256_hex(b"real bytes"))
            .unwrap();

        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id, "sha256": "0".repeat(64) } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("sha256 校验失败"), "text: {}", text);
        // 校验失败未消费条目：仍可被再次引用
        assert!(s.uploads.filename_of(id).is_some());
    }

    #[tokio::test]
    async fn send_email_rejects_upload_filename_mismatch() {
        let s = state();
        let v = create_upload_for(&s, CALLER, "real.pdf").await;
        let id = v["upload_id"].as_str().unwrap();
        s.uploads.begin_upload(id).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("payload");
        std::fs::write(&path, b"x").unwrap();
        s.uploads
            .finish_upload(id, dir, path, 1, crate::uploads::sha256_hex(b"x"))
            .unwrap();

        let (is_error, text) = call_tool(
            &s,
            CALLER,
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t", "body": "b", "receiver": ["a@b.com"],
                    "attachments": [ { "upload_id": id, "filename": "other.pdf" } ]
                }
            }),
        )
        .await;
        assert!(is_error);
        assert!(text.contains("与上传时不一致"), "text: {}", text);
    }

    #[tokio::test]
    async fn tools_call_rejects_bad_sha256_without_smtp() {
        let s = state();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"payload");
        let req = make_request(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["a@b.com"],
                    "attachments": [ {
                        "filename": "x.pdf",
                        "content": b64,
                        "sha256": "0".repeat(64)
                    } ]
                }
            }),
        );
        match handle_request(&s, CALLER, req).await {
            Reply::Response(r) => {
                let res = r.result.unwrap();
                assert_eq!(res["isError"], true);
                assert!(
                    res["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("sha256 校验失败"),
                    "{:?}",
                    res
                );
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn unknown_method_error() {
        let s = state();
        let req = make_request("bogus/method", json!({}));
        match handle_request(&s, CALLER, req).await {
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
        assert!(matches!(handle_request(&s, CALLER, req).await, Reply::Silent));
    }

    #[tokio::test]
    async fn tools_call_missing_required_args() {
        let s = state();
        let req = make_request(
            "tools/call",
            json!({ "name": "send_email", "arguments": { "subject": "x" } }),
        );
        match handle_request(&s, CALLER, req).await {
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
        // 单附件上限调到 1KB，验证超限在校验阶段被拒（不依赖默认的 10MB）
        let s = state_full(Some("https://x/y"), &[], Some(1024), None);
        let big = vec![b'x'; 1025];
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
        match handle_request(&s, CALLER, req).await {
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
        let st = state_with(Some("https://x/y"), &["txt"]);
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
        match handle_request(&st, CALLER, req).await {
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

//! HTTP 层集成测试：spawn 真实二进制，验证认证 / MCP 流 / 附件拒绝逻辑。
//! 每个测试使用独立端口（并行安全）；测试用例不触发真实 SMTP 发送
//! （拒发路径都被前置校验挡住）。

use base64::Engine;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::json;
use std::fs;
use std::process::{Child, Command};
use std::time::Duration;

const KEY: &str = "integration-test-key";

fn test_config(port: u16) -> String {
    format!(
        r#"
[server]
addr = "127.0.0.1"
port = {port}

[auth]
keys = ["{key}", "second-key"]

[smtp]
host = "smtp.126.com"
port = 465
tls = "ssl"
from = "sender@126.com"
user = "sender@126.com"
pass = "${{SMTP_PASS}}"

[security]
receiver_whitelist = ["allowed.com"]
max_attachment_bytes = 1048576
max_total_attachment_bytes = 1048576
max_request_bytes = 2097152
allowed_attachment_extensions = ["pdf", "txt"]
"#,
        port = port,
        key = KEY,
    )
}

fn base(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

struct Server {
    child: Child,
    cfg_path: String,
}

impl Server {
    async fn start(port: u16) -> Server {
        let dir = std::env::temp_dir().join(format!("smtp-mcp-it-{}-{}", std::process::id(), port));
        fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        fs::write(&cfg_path, test_config(port)).unwrap();

        let bin = env!("CARGO_BIN_EXE_smtp-mcp-server");
        let child = Command::new(bin)
            .arg(&cfg_path)
            .env("SMTP_PASS", "smoke-test")
            .spawn()
            .expect("spawn server");

        let server = Server {
            child,
            cfg_path: cfg_path.to_string_lossy().into_owned(),
        };

        // 等待就绪
        let client = reqwest::Client::new();
        for _ in 0..50 {
            if let Ok(resp) = client
                .get(format!("{}/healthz", base(port)))
                .timeout(Duration::from_millis(300))
                .send()
                .await
            {
                if resp.status() == StatusCode::OK {
                    return server;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("server(port {}) did not become ready", port);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.cfg_path);
    }
}

fn spawn_runtime_sync<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Runtime::new().unwrap().block_on(fut)
}

fn mcp_req(method: &str, params: serde_json::Value, id: u64) -> serde_json::Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
}

#[test]
fn healthz_ok() {
    spawn_runtime_sync(async {
        let port = 18202;
        let _server = Server::start(port).await;
        let resp = client()
            .get(format!("{}/healthz", base(port)))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    });
}

#[test]
fn auth_missing_and_wrong_key_same_error() {
    spawn_runtime_sync(async {
        let port = 18203;
        let _server = Server::start(port).await;
        let body = mcp_req("tools/list", json!({}), 1);

        let missing = client()
            .post(format!("{}/mcp", base(port)))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let wrong = client()
            .post(format!("{}/mcp", base(port)))
            .header("authorization", "Bearer wrong-key")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        // 统一错误体，不泄露任何差异
        assert_eq!(
            missing.json::<serde_json::Value>().await.unwrap(),
            wrong.json::<serde_json::Value>().await.unwrap()
        );
    });
}

#[test]
fn initialize_and_tools_list_with_valid_key() {
    spawn_runtime_sync(async {
        let port = 18204;
        let _server = Server::start(port).await;
        let c = client();

        let init = c
            .post(format!("{}/mcp", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&mcp_req(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name":"it","version":"1"}}),
                1,
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(init.status(), StatusCode::OK);
        let init_json = init.json::<serde_json::Value>().await.unwrap();
        assert_eq!(init_json["result"]["serverInfo"]["name"], "smtp-mcp-server");

        let tools = c
            .post(format!("{}/mcp", base(port)))
            .header("x-api-key", KEY)
            .json(&mcp_req("tools/list", json!({}), 2))
            .send()
            .await
            .unwrap();
        let tools_json = tools.json::<serde_json::Value>().await.unwrap();
        let names: Vec<&str> = tools_json["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"send_email"));
    });
}

#[test]
fn sse_transport_receives_response() {
    spawn_runtime_sync(async {
        let port = 18205;
        let _server = Server::start(port).await;
        let c = client();

        // 建立 legacy SSE 会话，读流式响应直到拿到 endpoint 事件
        let sse = c
            .get(format!("{}/sse", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .send()
            .await
            .unwrap();
        assert_eq!(sse.status(), StatusCode::OK);

        let mut stream = sse.bytes_stream();
        let mut buf = Vec::new();
        let mut endpoint = None;
        for _ in 0..8 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("sse stream timeout")
                .expect("sse closed early")
                .unwrap();
            buf.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&buf);
            if let Some(line) = text.lines().find(|l| l.starts_with("data:")) {
                let candidate = line.trim_start_matches("data:").trim();
                if candidate.starts_with("/messages") {
                    endpoint = Some(candidate.to_string());
                    break;
                }
            }
        }
        let endpoint = endpoint.expect("endpoint event not received");
        assert!(text_contains(&buf, "event: endpoint"));

        // 向 endpoint 发 tools/list，结果为 202，结果应由服务端广播到 SSE 流
        let resp = c
            .post(format!("{}{}", base(port), endpoint))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&mcp_req("tools/list", json!({}), 42))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    });
}

fn text_contains(buf: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(buf).contains(needle)
}

#[test]
fn tools_call_oversized_attachment_rejected() {
    spawn_runtime_sync(async {
        let port = 18206;
        let _server = Server::start(port).await;
        let big = vec![b'x'; 1024 * 1024 + 1];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&big);
        let call = mcp_req(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["u@allowed.com"],
                    "attachments": [{"filename": "big.pdf", "content": b64}]
                }
            }),
            1,
        );
        let resp = client()
            .post(format!("{}/mcp", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&call)
            .send()
            .await
            .unwrap();
        let json = resp.json::<serde_json::Value>().await.unwrap();
        assert_eq!(json["result"]["isError"], true);
        assert!(json["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("超过单附件上限"));
    });
}

#[test]
fn tools_call_disallowed_extension_rejected() {
    spawn_runtime_sync(async {
        let port = 18207;
        let _server = Server::start(port).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"evil");
        let call = mcp_req(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["u@allowed.com"],
                    "attachments": [{"filename": "evil.exe", "content": b64}]
                }
            }),
            1,
        );
        let resp = client()
            .post(format!("{}/mcp", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&call)
            .send()
            .await
            .unwrap();
        let json = resp.json::<serde_json::Value>().await.unwrap();
        assert_eq!(json["result"]["isError"], true);
        assert!(json["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("不在白名单"));
    });
}

#[test]
fn tools_call_whitelist_violation_rejected() {
    spawn_runtime_sync(async {
        let port = 18208;
        let _server = Server::start(port).await;
        let call = mcp_req(
            "tools/call",
            json!({
                "name": "send_email",
                "arguments": {
                    "subject": "t",
                    "body": "b",
                    "receiver": ["u@evil.com"]
                }
            }),
            1,
        );
        let resp = client()
            .post(format!("{}/mcp", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&call)
            .send()
            .await
            .unwrap();
        let json = resp.json::<serde_json::Value>().await.unwrap();
        assert_eq!(json["result"]["isError"], true);
        assert!(json["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("不在白名单内"));
    });
}

#[test]
fn unknown_method_returns_jsonrpc_error() {
    spawn_runtime_sync(async {
        let port = 18209;
        let _server = Server::start(port).await;
        let resp = client()
            .post(format!("{}/mcp", base(port)))
            .header("authorization", format!("Bearer {}", KEY))
            .json(&mcp_req("bogus/method", json!({}), 1))
            .send()
            .await
            .unwrap();
        let json = resp.json::<serde_json::Value>().await.unwrap();
        assert_eq!(json["error"]["code"], -32601);
    });
}

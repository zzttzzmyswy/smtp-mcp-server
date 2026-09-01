# smtp-mcp-server

基于 **Rust** 的 **MCP（HTTP/SSE transport）邮件发送服务**：提供 `send_email` 工具，经 SMTP（网易 126）发送通知邮件。设计定位为**跨 runtime 的 Web 服务**——各机器上的 Multica agent 通过 MCP 远程传输调用它，对外由 **nginx 反向代理**统一入口并终结 TLS。

**核心指标**：空闲常驻内存约 **6MB**（目标 <30MB）；二进制 ~4.2MB（strip + LTO）；支持静态/精简构建。

## 功能

- **MCP 两种传输协议**：
  - Streamable HTTP：`POST /mcp`（现代标准，Python/TS SDK 默认）
  - Legacy HTTP+SSE：`GET /sse` + `POST /messages?session_id=...`
- **`send_email` 工具**：`subject`、`body`、`receiver[]`（多收件人）、`attachments[]`（可选，base64 内容 + 文件名）
- **密钥认证**：配置多个 key，请求携带任一（`Authorization: Bearer <key>` 或 `X-API-Key: <key>`）；恒定时间比较（SHA-256 摘要 + 恒定时间异或累积），错误/缺失 key 返回统一 `401`，无数据泄露
- **SMTP**：支持 126 `smtp.126.com` 465/SSL、587/STARTTLS，用 **SMTP 授权码**（非登录密码）认证
- **附件安全**：单个附件 ≤1MB、单次总量 ≤1MB（默认）、扩展名白名单、大小/类型非法在落盘前拒绝；通过校验的附件写入**临时目录**，发送完成后自动清理
- **收件人白名单**（空 = 不限；可配完整邮箱或域后缀）
- **可选服务端 TLS**（cert/key，直连场景）——默认架构下 TLS 由 nginx 终结
- **日志安全**：不打印 key / 授权码 / 邮件正文；`send_email` 记录审计级日志（收件人、主题长度、附件数与大小）

## 架构

```
Multica agent (任意 runtime, 任意机器)
        │  MCP over HTTP/SSE  (Authorization: Bearer <key>)
        ▼
nginx 反代 (TLS 终结, 如 https://<host>/smtp-mcp/mcp)
        │  proxy_pass http://127.0.0.1:8202/
        ▼
smtp-mcp-server  [systemd 常驻, 仅监听内网 127.0.0.1:8202]
        │  SMTP (465/SSL, 授权码)
        ▼
smtp.126.com
```

## 快速开始

### 1. 构建

```bash
# 原生构建（本机测试）
cargo build --release
# 产物：target/release/smtp-mcp-server

# 静态/精简构建（可选，适合低配或容器）
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# 产物：target/x86_64-unknown-linux-musl/release/smtp-mcp-server（静态链接）
```

### 2. 配置

```bash
cp config.example.toml /etc/smtp-mcp/config.toml
# 编辑 smtp.pass（支持 ${ENV} 占位），或用环境变量 SMTP_PASS 注入
```

参数一览：

| 段 | 字段 | 说明 | 默认 |
|---|---|---|---|
| `server` | `addr` / `port` | 监听地址与端口 | `127.0.0.1` / `8202` |
| `auth` | `keys` (数组) | 访问密钥列表，任意一个可过鉴权 | **必填** |
| `smtp` | `host` | SMTP 服务器 | `smtp.126.com` |
| | `port` / `tls` | 端口与加密（`ssl`\|`starttls`\|`none`） | `465` / `ssl` |
| | `from` / `user` | 发件人 / SMTP 用户名 | 必填 |
| | `pass` | SMTP 授权码（支持 `${ENV}`；缺省回退读 `SMTP_PASS`） | 必填（运行时） |
| `security` | `receiver_whitelist` | 收件人白名单（空=不限；条目可为完整邮箱或域后缀） | `[]` |
| | `max_attachment_bytes` | 单附件上限 | `1048576` (1MB) |
| | `max_total_attachment_bytes` | 单次所有附件总上限 | `1048576` (1MB) |
| | `max_request_bytes` | 单请求体上限（含 base64 膨胀） | `2097152` (~2MB) |
| | `allowed_attachment_extensions` | 附件扩展名白名单（小写不带点，空=不限） | 见示例 |
| `tls`（可选） | `cert` / `key` | PEM 证书/私钥路径，启用后服务自带 TLS | 未启用 |

### 3. 运行

```bash
SMTP_PASS=<真实授权码> ./smtp-mcp-server /etc/smtp-mcp/config.toml
# 或其他方式：直接写 smtp.pass（勿提交到仓库）
```

### 4. systemd 常驻

```bash
sudo cp deploy/smtp-mcp.service /etc/systemd/system/
# 授权码走独立 env 文件（不含在仓库内）
echo "SMTP_PASS=<真实授权码>" | sudo tee /etc/smtp-mcp/smtp.env
sudo chmod 600 /etc/smtp-mcp/smtp.env
sudo systemctl daemon-reload && sudo systemctl enable --now smtp-mcp
systemctl status smtp-mcp
```

## nginx 反代

完整示例见 `deploy/nginx-smtp-mcp.conf.example`。要点：

```nginx
client_max_body_size 16m;               # ≥ max_request_bytes
location /smtp-mcp/ {
    proxy_pass http://127.0.0.1:8202/;  # 结尾斜杠去掉前缀
    proxy_http_version 1.1;
    proxy_set_header Connection "";
    proxy_buffering off;                # SSE 必须关闭缓冲
    proxy_read_timeout 3600s;           # SSE 长会话
    chunked_transfer_encoding on;
}
```

TLS 请在 443 server 块配置证书后复用同一段 location。

## MCP 接入（供 Multica agent 使用）

- **Streamable HTTP Base URL**：`http://127.0.0.1:8202/mcp`（或经 nginx：`https://<host>/smtp-mcp/mcp`）
- **Legacy SSE**：`GET http://127.0.0.1:8202/sse` → 获得 `/messages?session_id=...` 后 `POST` 消息
- **认证头**：`Authorization: Bearer <某个配置的 key>` 或 `X-API-Key: <key>`
- **支持的 MCP 方法**：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`；`send_email` 输入：

```json
{
  "subject": "主题（支持中文，自动 RFC2047 编码）",
  "body": "纯文本正文",
  "receiver": ["a@example.com", "b@example.com"],
  "html_body": "可选：AI Agent 自带完整 HTML 正文（可含 <table>/<img> 等），提供后优先级最高，不使用默认模板",
  "brand": "可选：页眉品牌名（默认 Multica MCP）",
  "greeting": "可选：问候语（默认“您好：”，空串不显示问候行）",
  "sign_name": "可选：落款人名（默认沿用品牌名，空串不显示签名区）",
  "attachments": [
    { "filename": "report.pdf",
      "content": "<base64 内容，解码后 ≤1MB>" }
  ]
}
```

### 默认 HTML 模板

- **未提供 `html_body`** 时，自动使用内置默认模板渲染邮件：响应式（桌面/手机自适应）、经典大气 + 中国元素（藏青主色、金色回纹、朱红印章、衬线宋体标题）
- `body` 纯文本按空行分段转成 HTML 段落，HTML 全部转义防注入；`subject` 作标题、`brand` 作品牌（页眉/印章）、`sign_name` 作落款
- **提供 `html_body`** 时（AI Agent 自带模板），优先使用它，其余参数忽略
- 支持正文区块：`<p class="mail-p">` 段落、`<table class="mail-table">` 表格、`<img class="mail-img">` 图片、`.mail-callout` 高亮、`.mail-quote` 引用、`.mail-list` 列表、`<a class="mail-btn">` 按钮
- **宽表自动横向滚动**：`html_body` 中的 `<table class="mail-table">` 会被自动包进 `.mail-table-wrap` 滚动容器——窄屏（手机）下表格保持可读最小宽度，超宽时容器内置左右滚动条，可左右滑动查看，而非被压缩导致列内容截断。若 AI 已自行使用 `mail-table-wrap` 则不重复包裹
- **排版随页面宽度自适应**：桌面端 640px 居中卡片，`≤620px` 视口自动切换为全宽 + 收窄内边距（`.pad` 18px）+ 标题缩小（`.titlex` 26px），正文区块与图片均 `max-width:100%`
- **字体跟随设备默认**：正文（段落/表格/列表/引用/按钮等）采用系统字体栈 `-apple-system`/`BlinkMacSystemFont`/`Segoe UI`/`Roboto`/苹方/微软雅黑，各设备优先使用自己的默认 UI 字体（iOS→苹方、Android→Roboto、Windows→雅黑），而非固定某套字体；仅品牌装饰元素（大标题衬线、朱红印章楷体）保留设计字体并带跨平台中文回退

返回值示例（成功）：

```json
{
  "content": [{"type": "text", "text": "邮件已发送: 收件人 a@example.com，主题长度 4，附件 1 个（128 字节）"}],
  "isError": false
}
```

被拒（大小/类型/白名单/参数非法）时 `isError: true`，`text` 为明确原因（中文）。

## 性能与安全说明

- **低内存**：tokio + axum + lettre(rustls)，不引入 native-tls/openssl；空闲 RSS 实测 **~6MB**（`ps -o rss`）
- **密钥认证**：密钥仅以 SHA-256 摘要驻留内存；比较走恒定时间路径，未命中不会泄露差异
- **日志**：审计日志含 时间/收件人/主题长度/附件信息；不含密钥、授权码、正文。拒绝事件以 WARN 记录原因
- **附件**：非法 base64、超限、类型不在白名单在写盘前直接拒绝并报错；合法附件写临时目录、发送完随 `StagedAttachments` drop 自动清理

## 测试

```bash
cargo test                 # 单元测试：鉴权/恒定时间/附件超限与类型/配置解析/白名单/MCP 协议
cargo test --test integration   # HTTP 层集成测试：spawn 真实二进制验证 401 统一错误、initialize、tools/list、
                               # SSE 链路、超限/白名单拒绝、未知方法错误
```

## 授权码安全约定

- 仓库内**不保存任何真实授权码**（`config.example.toml` 只有占位）
- 运行时通过 `EnvironmentFile` / 环境变量注入，文件权限 `600`
- 真实授权码与发件人地址由邮箱属主线下提供后，仅写入服务器本地配置文件

## 目录结构

```
src/
  main.rs         HTTP 层：axum 路由 /mcp、/sse、/messages、认证中间件、限量读 body、可选 TLS
  mcp.rs          MCP JSON-RPC 处理：initialize/tools/list/tools/call、send_email 编排与审计
  mail.rs         lettre SMTP 发送、收件人校验/白名单、MIME/附件构建、mime 推断
  template.rs     默认 HTML 模板渲染（include_str! 内嵌）、纯文本转 HTML、转义
  attachments.rs  附件 base64 解码/大小与类型校验/临时目录落盘与清理
  auth.rs         密钥认证：SHA-256 摘要 + 恒定时间比较
  config.rs       TOML 配置解析、env 插值、默认值
tests/integration.rs  HTTP 层端到端测试
deploy/            systemd 单元、nginx 反代示例
config.example.toml 配置模板
```
# smtp-mcp-server

基于 **Rust** 的 **MCP（HTTP/SSE transport）邮件发送服务**：提供 `send_email` 工具，经 SMTP（网易 126）发送通知邮件。设计定位为**跨 runtime 的 Web 服务**——各机器上的 Multica agent 通过 MCP 远程传输调用它，对外由 **nginx 反向代理**统一入口并终结 TLS。

**核心指标**：空闲常驻内存约 **9MB**（目标 <30MB）；二进制 ~4.6MB（strip + LTO）；支持静态/精简构建。

## 功能

- **MCP 两种传输协议**：
  - Streamable HTTP：`POST /mcp`（现代标准，Python/TS SDK 默认）
  - Legacy HTTP+SSE：`GET /sse` + `POST /messages?session_id=...`
- **`send_email` 工具**：`subject`、`body`、`receiver[]`（多收件人）、`attachments[]`（可选，两种来源：带外 `upload_id` 或 base64 `content` + 可选 `sha256` 校验和）
- **`create_upload` 工具 + 带外上传通道**：大文件不再走 base64。`create_upload` 发一次性 `upload_id`/`upload_url` → agent 用自己的 shell `curl -T` 把字节直传服务 → `send_email` 只引用 `upload_id`。上限 **10MB**，流式落盘（内存占用不随文件大小增长）
- **正文自动格式化**：`body` 默认 `auto` —— 自动识别 Markdown（标题/列表/引用/表格/代码块/加粗等）并转换为 HTML 正文；普通纯文本保持既有排版；也可用 `body_format`（`auto` / `text` / `markdown`）显式指定，或传 `html_body` 直接使用 AI Agent 自带 HTML。**无外包装模板**：邮件即原始正文，宽度/字体完全跟随邮件阅读器默认行为
- **密钥认证**：配置多个 key，请求携带任一（`Authorization: Bearer <key>` 或 `X-API-Key: <key>`）；恒定时间比较（SHA-256 摘要 + 恒定时间异或累积），错误/缺失 key 返回统一 `401`，无数据泄露
- **SMTP**：支持 126 `smtp.126.com` 465/SSL、587/STARTTLS，用 **SMTP 授权码**（非登录密码）认证
- **附件安全**：单个附件 ≤10MB、单次总量 ≤10MB（默认）、扩展名白名单、大小/类型非法在落盘前拒绝；写入**临时目录**，发送完成后自动清理
- **附件完整性**：附件可带可选 `sha256`（原始字节的 SHA-256 hex），不匹配即拒绝——用于发现内容在传输中被截断或改写
- **上传通道隔离**：`upload_id` 绑定创建它的 API key（`owner` = key 的 SHA-256），别的 key 无法消费；`PUT /upload/:id` 本身就是一次性凭据（不挂 Bearer 认证）；同一 id 并发 PUT 只有一个成功（第二个 `409`）；`ttl_secs` 到期后惰性 + 后台双路清理，磁盘文件随之删除
- **宽容 base64 解码**（仅 base64 内联通道）：自动去掉 `data:` URI 前缀、忽略全部 ASCII 空白、兼容 URL-safe 字母表（`-` `_`）、补齐缺失的 `=` padding；真正非法的内容仍然拒绝
- **收件人白名单**（空 = 不限；可配完整邮箱或域后缀）
- **可选服务端 TLS**（cert/key，直连场景）——默认架构下 TLS 由 nginx 终结
- **日志安全**：不打印 key / 授权码 / 邮件正文；`send_email`、`create_upload`、上传完成各记审计级日志（上传日志只记 id 前 8 位）

## 架构

```
Multica agent (任意 runtime, 任意机器)
        │  ① MCP over HTTP/SSE  (Authorization: Bearer <key>)   ← 建 upload / 发信
        │  ② PUT /upload/<id>   (无认证头, id 即一次性凭据)     ← curl -T 直传附件字节
        ▼
nginx 反代 (TLS 终结, 如 https://<host>/smtp-mcp/)
        │  proxy_pass http://127.0.0.1:8202/
        ▼
smtp-mcp-server  [systemd 常驻, 仅监听内网 127.0.0.1:8202]
        │  附件字节流式落盘临时目录（内存不随文件增长）
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
| | `max_attachment_bytes` | 单附件上限 | `10485760` (10MB) |
| | `max_total_attachment_bytes` | 单次所有附件总上限 | `10485760` (10MB) |
| | `max_request_bytes` | 单请求体上限（仅约束 JSON-RPC 请求体，即 base64 内联通道） | `2097152` (~2MB) |
| | `allowed_attachment_extensions` | 附件扩展名白名单（小写不带点，空=不限） | 见示例 |
| `upload` | `public_base_url` | 对外 URL 前缀，拼出 `create_upload` 的直传地址；留空 = 禁用上传方式 | `""`（禁用） |
| | `ttl_secs` | upload_id 从创建到被消费的有效期（秒） | `900` |
| | `max_pending` | 同时存在的未消费 upload 数量上限 | `16` |
| | `max_pending_bytes` | 已上传未消费的总字节上限 | `52428800` (50MB) |
| `tls`（可选） | `cert` / `key` | PEM 证书/私钥路径，启用后服务自带 TLS | 未启用 |

> `upload.public_base_url` 必须是**外部实际可访问**的前缀（含 nginx 的 `/smtp-mcp` 路径），
> 因为它会原样拼进返回给 agent 的 `upload_url`。配错会导致 `curl -T` 打到 404。
> `max_request_bytes` 保持 2MB 是刻意的：它只约束 JSON-RPC 请求体（base64 内联通道），
> 大文件请走带外上传，不受此限制。

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
client_max_body_size 16m;               # 须 ≥ max_attachment_bytes（10m）
location /smtp-mcp/ {
    proxy_pass http://127.0.0.1:8202/;  # 结尾斜杠去掉前缀
    proxy_http_version 1.1;
    proxy_set_header Connection "";
    proxy_buffering off;                # SSE 必须关闭缓冲
    proxy_read_timeout 3600s;           # SSE 长会话
    chunked_transfer_encoding on;
}
# 可选：上传路径关闭请求体缓冲，边收边转发（省掉 nginx 侧磁盘中转）
location /smtp-mcp/upload/ {
    proxy_pass http://127.0.0.1:8202/upload/;
    proxy_http_version 1.1;
    proxy_request_buffering off;
    proxy_read_timeout 3600s;
}
```

TLS 请在 443 server 块配置证书后复用同一段 location。
注意 `upload.public_base_url` 要写成这里对外暴露的前缀（如 `https://<host>/smtp-mcp`）。

## MCP 接入（供 Multica agent 使用）

- **Streamable HTTP Base URL**：`http://127.0.0.1:8202/mcp`（或经 nginx：`https://<host>/smtp-mcp/mcp`）
- **Legacy SSE**：`GET http://127.0.0.1:8202/sse` → 获得 `/messages?session_id=...` 后 `POST` 消息
- **认证头**：`Authorization: Bearer <某个配置的 key>` 或 `X-API-Key: <key>`
- **支持的 MCP 方法**：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`
- **工具**：`send_email`、`create_upload`

### 附件两条通道

| 通道 | 适用 | 上限 | 说明 |
|---|---|---|---|
| **带外上传**（`create_upload` + `curl -T` + `upload_id`） | 大文件、二进制 | `security.max_attachment_bytes`（默认 10MB） | **推荐**。字节由 agent 的 shell 直传，不经过模型，无 base64 编解码、无逐 token 生成导致的损坏 |
| **base64 内联**（`attachments[].content`） | 约 10KB 以内的小文本 | 受 `max_request_bytes`（2MB）与 `max_attachment_bytes` 双重约束 | 内容字符需由模型逐个生成，越大越容易丢字符或被截断 |

### 两步流程示例（带外上传）

**第 1 步：申请上传凭据**（MCP `tools/call`）

```json
{ "name": "create_upload", "arguments": { "filename": "report.pdf" } }
```

返回（`content[0].text` 是一段 JSON 字符串）：

```json
{
  "upload_id": "9f2c1ab4e7d84f0c9b3e5a6d1c8f7e20",
  "upload_url": "https://mu.zztweb.top/smtp-mcp/upload/9f2c1ab4e7d84f0c9b3e5a6d1c8f7e20",
  "expires_in_secs": 900,
  "max_bytes": 10485760,
  "hint": "curl -fsS -T <本地文件路径> 'https://…/upload/9f2c…' ，成功后在 send_email.attachments 里传 {\"upload_id\":\"9f2c…\",\"sha256\":\"…\"}"
}
```

**第 2 步：在 agent 自己的 shell 里直传字节**

```bash
# 无需认证头：URL 里的 32 位随机 id 就是一次性凭据
# 用 -T 走 PUT（自动分块流式发送，不把文件读进内存）
curl -fsS -T /path/to/report.pdf \
  'https://mu.zztweb.top/smtp-mcp/upload/9f2c1ab4e7d84f0c9b3e5a6d1c8f7e20'
```

成功返回（服务端边收边落盘并计算摘要，可直接作为 `send_email` 的 `sha256`）：

```json
{ "upload_id": "9f2c1ab4e7d84f0c9b3e5a6d1c8f7e20",
  "filename": "report.pdf", "size": 5242880,
  "sha256": "a1b2c3…（64 位 hex）" }
```

**第 3 步：发送时引用 `upload_id`**（MCP `tools/call`）

```json
{
  "name": "send_email",
  "arguments": {
    "subject": "报告",
    "body": "见附件",
    "receiver": ["a@example.com"],
    "attachments": [
      { "upload_id": "9f2c1ab4e7d84f0c9b3e5a6d1c8f7e20",
        "sha256": "a1b2c3…（64 位 hex，建议直接用上传返回的那个）" }
    ]
  }
}
```

上传失败或超限时返回：

| 情况 | HTTP | body |
|---|---|---|
| id 不存在 / 已过期 | `404` | `{"error":"unknown or expired upload"}` |
| 该 id 已在用或已完成（同一 id 并发 PUT 只有一个成功） | `409` | `{"error":"upload already used"}` |
| 超过 `max_attachment_bytes` 或 `max_pending_bytes` | `413` | `{"error":"…"}`（条目随即作废，需重新 `create_upload`） |
| 非 PUT 方法 | `405` | — |

`upload_id` 的**消费策略**：`send_email` 一旦取走该上传（通过全部附件校验之后），
无论邮件最终发送成功还是失败，该 `upload_id` 都**不再保留**——发送失败需要**重新上传**。
因此发送前请确认收件人、主题等都已就绪。（好处是不会因为一次失败的发送而留下可疑的残留数据。）

附件校验（文件名、扩展名、sha256、大小）全部在消费之前完成：
校验失败**不会**消耗掉上传，改对参数后同一个 `upload_id` 仍可用。

### `send_email` 输入

```json
{
  "subject": "主题（支持中文，自动 RFC2047 编码）",
  "body": "正文：普通纯文本或 Markdown（默认 auto 自动识别；标题/列表/引用/管道表格/代码块/加粗等均支持）",
  "receiver": ["a@example.com", "b@example.com"],
  "body_format": "可选：auto（默认）| text（强制纯文本）| markdown（强制 Markdown 转换）",
  "html_body": "可选：AI Agent 自带 HTML 正文（可含 <table>/<img> 等），提供后直接作为邮件正文",
  "attachments": [
    { "upload_id": "带外上传返回的 id",
      "sha256": "<可选但强烈建议：原始字节的 SHA-256 hex>" },
    { "filename": "tiny.txt",
      "content": "<base64 内容，解码后 ≤10KB 为宜>",
      "sha256": "<可选>" }
  ]
}
```

附件字段说明：

| 字段 | 必填 | 说明 |
|---|---|---|
| `upload_id` | 与 `content` 二选一 | `create_upload` 返回的上传凭据；字节须已 `curl -T` 上传完成 |
| `content` | 与 `upload_id` 二选一 | base64 内容。**仅适合约 10KB 以内的小文件**——这些字符需由模型逐个生成，越大越容易丢字符或被截断 |
| `filename` | 视情况 | 纯文件名（不含路径），扩展名须在服务端白名单内。走 `upload_id` 时可省略（默认取 `create_upload` 时的名字）；提供则必须与上传时一致 |
| `sha256` | 否（强烈建议） | **原始字节**的 SHA-256 hex（64 字符，大小写均可）。内容被截断/改写时即使仍是合法 base64 也会被此校验发现并拒绝 |

`content` 与 `upload_id` **必须且只能提供一个**，否则 `isError`：`附件必须且只能提供 content 或 upload_id 之一`。

`content` 采用**宽容解码**，容忍模型逐 token 书写时的常见偏差：`data:<mediatype>;base64,` 前缀、任意 ASCII 空白（含折行）、URL-safe 字母表（`-` `_`）、缺失的 `=` padding 都会被自动处理；长度 `%4 == 1`（无法补齐）或含字母表外字符的内容仍判为非法 base64。

拒绝时的错误文案（`isError: true`）：
- sha256 不匹配：`附件 <文件名> 的 sha256 校验失败：期望 <expected>，实际 <actual>（内容可能在传输中被截断或改写）`
- sha256 非 64 位 hex：`附件 <文件名> 的 sha256 格式非法`
- 非法 base64：`附件 <文件名> 内容不是合法 base64`
- 来源冲突：`附件必须且只能提供 content 或 upload_id 之一`
- 上传不可用：`附件 upload_id 不存在或已过期…` / `附件 upload_id 不属于当前 API key` / `附件 upload_id 尚未完成上传（请先 curl -T 上传字节）`
- 文件名不符：`附件文件名与上传时不一致：上传时为 <a>，请求为 <b>`
- 未配置上传：`服务端未配置 upload.public_base_url，无法使用上传方式`

### 正文渲染（无外包装模板）

- **不再使用外包装 HTML 模板**：v0.5.0 起删除了整个邮件外壳（品牌页眉/日期/问候/落款/页脚/预览文字），邮件即原始正文，宽度/字体/字号完全跟随各邮件阅读器默认行为，不发生内容被框住或固定宽度的问题
- `body` 默认 `auto`：命中 Markdown 特征（`# ` 标题、`- `/`1. ` 列表、`- [ ]` 任务列表、`>` 引用、```` ``` ```` 代码块、`|` 管道表格、`**` 加粗、`` ` ` `` 行内代码、链接/图片）即按 **Markdown 自动转换 HTML**；普通文本按纯文本排版（空行分段、转义防注入）。`body_format=text` 强制纯文本、`=markdown` 强制转换
- Markdown 正文片段自带内联样式：标题 `.mail-h2`、段落 `.mail-p`、引用 `.mail-quote`、列表 `.mail-list`、表格 `.mail-table`（宽表自动包横向滚动容器）、图片 `.mail-img`；文本自动转义，Agent 内嵌原始 HTML（`<div>/<img>` 等）与 `html_body` 同信任模型放行
- **提供 `html_body`** 时直接作为邮件正文（AI Agent 自带 HTML，可含 `<table class="mail-table">` / `<img>` 等），不加任何包裹
- **宽表自动横向滚动**：正文中出现 `<table class="mail-table">` 会被自动包进 `.mail-table-wrap` 滚动容器——窄屏（手机）下表格保持可读最小宽度，超宽时容器内置左右滚动条，而非被压缩导致列内容截断。若 AI 已自行使用 `mail-table-wrap` 则不重复包裹

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
- **上传流式落盘**：`PUT /upload/:id` 逐块读取 body 写入临时文件，边写边累计大小与 SHA-256，**不把整个 body 读进内存**。实测上传 10MB 文件时 RSS 仅增长约 **0.6MB**（`VmRSS` 15900KB → 16512KB）。发送阶段 lettre 会把附件读入内存构造 MIME，峰值会升高，属预期
- **密钥认证**：密钥仅以 SHA-256 摘要驻留内存；比较走恒定时间路径，未命中不会泄露差异。上传条目的 owner 也是 key 的 SHA-256，不驻留明文
- **上传通道**：`PUT /upload/:id` 不挂 Bearer 认证——URL 里的 32 位随机 id 本身就是一次性凭据（`OsRng` 取自 16 字节密码学随机数）；条目绑定创建者身份，其他 key 无法消费；超过 `max_attachment_bytes` / `max_pending_bytes` 立即中止并作废该条目；`ttl_secs`（默认 900s）到期后惰性清理 + 每 60s 后台清理，磁盘文件随条目 `TempDir` drop 删除
- **日志**：审计日志含 时间/收件人/主题长度/附件信息；不含密钥、授权码、正文。上传完成日志只记 `upload_id` 前 8 位。拒绝事件以 WARN 记录原因
- **附件**：非法 base64、sha256 校验失败、超限、类型不在白名单在写盘前直接拒绝并报错；合法附件写临时目录、发送完随 `StagedAttachments` drop 自动清理（带外上传的临时目录同样被接管，发送结束一并删除）

## 测试

```bash
cargo test                        # 全部：单元 + HTTP 集成 + QP 回归
cargo test --bin smtp-mcp-server  # 单元：鉴权/上传状态机/过期与 owner 隔离/附件校验/配置解析/MCP 协议
cargo test --test integration     # HTTP 层：spawn 真实二进制，覆盖 401 统一错误、initialize、tools/list、
                                  # SSE 链路、上传通道（成功/409/404/过期/413/RSS 流式）、
                                  # upload_id 消费与拒绝路径、base64 内联回归
cargo clippy --all-targets -- -D warnings
```

## 授权码安全约定

- 仓库内**不保存任何真实授权码**（`config.example.toml` 只有占位）
- 运行时通过 `EnvironmentFile` / 环境变量注入，文件权限 `600`
- 真实授权码与发件人地址由邮箱属主线下提供后，仅写入服务器本地配置文件

## 目录结构

```
src/
  main.rs         HTTP 层：axum 路由 /mcp、/sse、/messages、PUT /upload/:id、认证中间件、限量读 body、上传流式落盘、可选 TLS
  mcp.rs          MCP JSON-RPC 处理：initialize/tools/list/tools/call、send_email 与 create_upload 编排与审计
  uploads.rs      带外上传存储：id 生成、状态机(Pending/Uploading/Uploaded)、owner 隔离、TTL 与惰性+后台清理
  mail.rs         lettre SMTP 发送、收件人校验/白名单、MIME/附件构建、mime 推断
  template.rs     正文 HTML 组装（html_body 原样 / Markdown 转换 / 纯文本分段）+ 宽表滚动容器 + 转义
  markdown.rs     Markdown → HTML 正文片段（pulldown-cmark 自定义渲染，自带内联样式）、Markdown 自动识别
  attachments.rs  附件双通道校验（文件名/扩展名/sha256/大小）、base64 宽容解码、临时目录落盘与清理
  auth.rs         密钥认证：SHA-256 摘要 + 恒定时间比较
  config.rs       TOML 配置解析、env 插值、默认值
tests/integration.rs  HTTP 层端到端测试（含上传通道与 RSS 流式断言）
deploy/            systemd 单元、nginx 反代示例
config.example.toml 配置模板
```
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// MCP tools/call 中 send_email 的单个附件描述。
///
/// `content`（base64 内联）与 `upload_id`（带外上传后引用）**二选一**；
/// `sha256` 为可选的完整性校验和。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AttachmentInput {
    /// 可选：`upload_id` 路径下缺省取创建上传时的文件名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// base64 编码的文件内容（与 `upload_id` 二选一）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 带外上传返回的 upload_id（与 `content` 二选一）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    /// 可选：解码/读取后**原始字节**的 SHA-256 hex（64 字符，大小写均可）。
    /// 由调用方先算好；服务端据此发现内容在传输中被截断或改写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl AttachmentInput {
    /// 文件名与内容来源互斥校验：`content` 与 `upload_id` 必须且只能提供一个。
    pub fn source(&self) -> Result<Source<'_>, String> {
        match (self.content.as_deref(), self.upload_id.as_deref()) {
            (Some(_), Some(_)) | (None, None) => {
                Err("附件必须且只能提供 content 或 upload_id 之一".into())
            }
            (Some(c), None) => Ok(Source::Inline(c)),
            (None, Some(u)) => Ok(Source::Upload(u)),
        }
    }
}

/// 附件字节的来源
#[derive(Debug, PartialEq, Eq)]
pub enum Source<'a> {
    /// base64 内联内容
    Inline(&'a str),
    /// 带外上传的 upload_id
    Upload(&'a str),
}

/// 宽容 base64 解码：容忍 LLM 逐 token 书写时常见的几种偏差。
///
/// 依次处理：
/// 1. `data:` URI 前缀（取第一个 `,` 之后的部分）；
/// 2. 去掉全部 ASCII 空白（空格/换行/制表，LLM 常按行折行）；
/// 3. URL-safe 字母表（`-` `_`）转标准字母表（`+` `/`）；
/// 4. 补齐缺失的 `=` padding（长度 %4==1 无法补齐，视为非法）；
/// 5. 标准字母表解码。
///
/// 仍然拒绝真正非法的内容（如含 `%` 等非字母表字符）。
pub fn decode_base64_lenient(content: &str) -> Result<Vec<u8>, ()> {
    let mut s = content.trim_start();

    // 1. data URI：data:<mediatype>[;base64],<payload>
    if let Some(rest) = s.strip_prefix("data:") {
        match rest.find(',') {
            Some(idx) => s = &rest[idx + 1..],
            None => return Err(()),
        }
    }

    // 2. 去掉全部 ASCII 空白
    // 3. URL-safe 字母表转标准字母表
    let mut normalized: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .map(|b| match b {
            b'-' => b'+',
            b'_' => b'/',
            other => other,
        })
        .collect();

    // 4. 补齐 padding
    match normalized.len() % 4 {
        0 => {}
        1 => return Err(()),
        rem => normalized.resize(normalized.len() + (4 - rem), b'='),
    }

    // 5. 解码
    base64::engine::general_purpose::STANDARD
        .decode(&normalized)
        .map_err(|_| ())
}

/// 小写 hex（用于展示与比较 SHA-256）
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 校验附件 sha256：格式必须为 64 位 hex；值必须等于解码后字节的 SHA-256（忽略大小写）。
pub(crate) fn verify_sha256(filename: &str, decoded: &[u8], expected: &str) -> Result<(), String> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("附件 {} 的 sha256 格式非法", filename));
    }
    let actual = hex_lower(&Sha256::digest(decoded));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "附件 {} 的 sha256 校验失败：期望 {}，实际 {}（内容可能在传输中被截断或改写）",
            filename, expected, actual
        ));
    }
    Ok(())
}

/// 文件名合法性校验：非空、纯文件名（不含路径分隔符与 `..`）、扩展名在白名单内（空 = 不限）。
/// `create_upload` 在创建时即调用，避免上传完才发现类型不允许。
pub fn validate_filename(filename: &str, security: &crate::config::Security) -> Result<(), String> {
    if filename.is_empty() {
        return Err("附件缺少文件名".into());
    }
    // 防止路径穿越：只允许纯文件名
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(format!("非法附件文件名: {}", filename));
    }
    if !security.allowed_attachment_extensions.is_empty() {
        let ext = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !security
            .allowed_attachment_extensions
            .iter()
            .any(|a| a.trim_start_matches('.').eq_ignore_ascii_case(&ext))
        {
            return Err(format!(
                "附件类型 .{} 不在白名单中（允许: {}）",
                ext,
                security.allowed_attachment_extensions.join(", ")
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct StagedFile {
    pub filename: String,
    pub path: PathBuf,
}

/// 校验通过的附件已落到临时目录；Drop 时自动清理整个临时目录。
///
/// `_upload_dirs` 持有被 `send_email` 消费掉的带外上传条目所属的 `TempDir`：
/// 把它们的生命周期绑到本次发送上，发送结束（成功或失败）随本结构 drop 一并删除磁盘文件。
#[derive(Debug)]
pub struct StagedAttachments {
    _dir: TempDir,
    _upload_dirs: Vec<TempDir>,
    pub files: Vec<StagedFile>,
}

impl StagedAttachments {
    /// 本次发送接管了几个带外上传的临时目录（测试用）
    #[cfg(test)]
    pub fn upload_dir_count(&self) -> usize {
        self._upload_dirs.len()
    }
}

/// 附件校验与落盘。任何校验失败返回带明确原因的 Err，且不会留下临时文件。
///
/// 两种来源：
/// - `content`（base64 内联）：宽容解码 → 可选 sha256 校验 → 大小校验 → 写入本次发送专属临时目录；
/// - `upload_id`（带外上传）：先 `peek` 校验（不消费），**全部校验通过后才 `take`**，
///   其 `TempDir` 被 `StagedAttachments` 接管，发送结束随 Drop 清理磁盘文件。
///
/// 分两阶段（先全量校验、再取用）的原因：一条附件校验失败不应消耗掉别人已上传的字节——
/// 否则调用方改个 sha256 重试就得重新上传整个文件。
///
/// 两条路径共用同一份文件名/扩展名/去重校验，因此两种来源混用时行为一致。
pub fn stage_attachments(
    attachments: &[AttachmentInput],
    security: &crate::config::Security,
    resolver: &crate::uploads::UploadResolver<'_>,
) -> Result<StagedAttachments, String> {
    // ---- 阶段 1：全量校验（不消费任何 upload 条目）----
    /// 校验通过、待落盘的附件来源
    enum Validated {
        Inline { filename: String, decoded: Vec<u8> },
        Upload { filename: String, id: String },
    }

    let mut total = 0usize;
    let mut seen = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(attachments.len());

    for att in attachments {
        // base64 内联与带外上传必须二选一
        let source = att.source()?;

        // 带外上传：peek 只读校验，未完成/已过期/owner 不符在此报错
        let meta = match source {
            Source::Inline(_) => None,
            Source::Upload(id) => Some(resolver.peek(id)?),
        };

        // 文件名：内联路径必填；上传路径缺省取创建时的文件名，提供则须一致
        let given = att
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty());
        let filename = match (&meta, given) {
            (Some(m), None) => m.filename.clone(),
            (Some(m), Some(g)) => {
                if g != m.filename {
                    return Err(format!(
                        "附件文件名与上传时不一致：上传时为 {}，请求为 {}",
                        m.filename, g
                    ));
                }
                m.filename.clone()
            }
            (None, Some(g)) => g.to_string(),
            (None, None) => return Err("附件缺少文件名".into()),
        };
        validate_filename(&filename, security)?;
        if !seen.insert(filename.clone()) {
            return Err(format!("重复附件文件名: {}", filename));
        }

        match (meta, source) {
            // ---- 带外上传：字节已在磁盘，只做校验和记账 ----
            (Some(m), Source::Upload(id)) => {
                if let Some(expected) = att.sha256.as_deref() {
                    verify_sha256_hex(&filename, &m.sha256, expected)?;
                }
                total += usize::try_from(m.size).unwrap_or(usize::MAX);
                if total > security.max_total_attachment_bytes {
                    return Err(format!(
                        "附件总大小 {} 字节，超过单次上限 {} 字节",
                        total, security.max_total_attachment_bytes
                    ));
                }
                validated.push(Validated::Upload {
                    filename,
                    id: id.to_string(),
                });
            }
            // ---- base64 内联 ----
            (None, Source::Inline(content)) => {
                // base64 解码（宽容：容忍空白/URL-safe/缺 padding/data URI；仍拒绝非法内容）
                let decoded = decode_base64_lenient(content)
                    .map_err(|_| format!("附件 {} 内容不是合法 base64", filename))?;

                // 可选的完整性校验：内容被截断/改写时（即使仍是合法 base64）在此暴露
                if let Some(expected) = att.sha256.as_deref() {
                    verify_sha256(&filename, &decoded, expected)?;
                }

                if decoded.len() > security.max_attachment_bytes {
                    return Err(format!(
                        "附件 {} 大小为 {} 字节，超过单附件上限 {} 字节",
                        filename,
                        decoded.len(),
                        security.max_attachment_bytes
                    ));
                }
                total += decoded.len();
                if total > security.max_total_attachment_bytes {
                    return Err(format!(
                        "附件总大小 {} 字节，超过单次上限 {} 字节",
                        total, security.max_total_attachment_bytes
                    ));
                }
                validated.push(Validated::Inline { filename, decoded });
            }
            // source 已在上方 match 中排除
            (Some(_), Source::Inline(_)) | (None, Source::Upload(_)) => {
                unreachable!("source matched above")
            }
        }
    }

    // ---- 阶段 2：落盘 / 消费上传 ----
    let dir = TempDir::new().map_err(|e| format!("创建临时目录失败: {}", e))?;
    let mut files = Vec::with_capacity(validated.len());
    let mut upload_dirs = Vec::new();

    for v in validated {
        match v {
            Validated::Inline { filename, decoded } => {
                // 写入临时目录（命名冲突由唯一前缀避免）
                let path = dir
                    .path()
                    .join(format!("{:08x}-{}", rand::random::<u32>(), filename));
                std::fs::write(&path, &decoded)
                    .map_err(|e| format!("写入临时附件 {} 失败: {}", filename, e))?;
                files.push(StagedFile { filename, path });
            }
            Validated::Upload { filename, id } => {
                // 校验已全部通过，至此才消费条目
                let taken = resolver.take(&id)?;
                let crate::uploads::ConsumedUpload {
                    path,
                    dir: upload_dir,
                } = taken;
                files.push(StagedFile { filename, path });
                // 磁盘文件的生命周期交给本次发送：StagedAttachments drop 时一并清理
                upload_dirs.push(upload_dir);
            }
        }
    }

    Ok(StagedAttachments {
        _dir: dir,
        _upload_dirs: upload_dirs,
        files,
    })
}

/// 带外上传路径的 sha256 比对：服务端在上传时已算好摘要，这里只比对调用方给的值。
fn verify_sha256_hex(filename: &str, actual: &str, expected: &str) -> Result<(), String> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("附件 {} 的 sha256 格式非法", filename));
    }
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "附件 {} 的 sha256 校验失败：期望 {}，实际 {}（内容可能在传输中被截断或改写）",
            filename, expected, actual
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Security;
    use base64::Engine;

    fn enc(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn security(max_att: usize, max_total: usize, exts: &[&str]) -> Security {
        Security {
            receiver_whitelist: vec![],
            max_attachment_bytes: max_att,
            max_total_attachment_bytes: max_total,
            max_request_bytes: 2 * 1024 * 1024,
            allowed_attachment_extensions: exts.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// base64 内联附件
    fn inline(filename: &str, content: String, sha256: Option<String>) -> AttachmentInput {
        AttachmentInput {
            filename: Some(filename.to_string()),
            content: Some(content),
            upload_id: None,
            sha256,
        }
    }

    /// 带外上传引用附件
    fn by_upload(upload_id: &str, filename: Option<&str>, sha256: Option<String>) -> AttachmentInput {
        AttachmentInput {
            filename: filename.map(str::to_string),
            content: None,
            upload_id: Some(upload_id.to_string()),
            sha256,
        }
    }

    /// 无带外上传的 stage 入口（内联路径测试用）
    fn stage(
        attachments: &[AttachmentInput],
        security: &Security,
    ) -> Result<StagedAttachments, String> {
        let store = crate::uploads::UploadStore::new(900, 16, 1024);
        stage_attachments(attachments, security, &store.resolver([0u8; 32]))
    }

    /// 用真实 UploadStore 走完 create → begin → finish，返回 (store, id)
    fn stored_upload(owner: [u8; 32], filename: &str, data: &[u8]) -> (crate::uploads::UploadStore, String) {
        let store = crate::uploads::UploadStore::new(900, 16, 64 * 1024 * 1024);
        let id = store.create(owner, filename.to_string()).unwrap();
        store.begin_upload(&id).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, data).unwrap();
        store
            .finish_upload(&id, dir, path, data.len() as u64, sha256_hex(data))
            .unwrap();
        (store, id)
    }

    #[test]
    fn empty_ok() {
        let s = security(10, 10, &[]);
        let st = stage(&[], &s).unwrap();
        assert!(st.files.is_empty());
    }

    #[test]
    fn stages_valid_attachment() {
        let s = security(1024, 2048, &["txt"]);
        let st = stage(
            &[inline("a.txt", enc(b"hello"), None)],
            &s,
        )
        .unwrap();
        assert_eq!(st.files.len(), 1);
        assert_eq!(std::fs::read_to_string(&st.files[0].path).unwrap(), "hello");
    }

    #[test]
    fn rejects_oversized_attachment() {
        let s = security(4, 4, &[]);
        let err = stage(
            &[inline("big.bin", enc(&[0u8; 5]), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("超过单附件上限"), "err: {}", err);
    }

    #[test]
    fn default_1mb_boundary() {
        let s = security(1024 * 1024, 1024 * 1024, &[]);
        // 恰好 1MB：通过
        let ok = stage(
            &[inline("a.bin", enc(&[0u8; 1024 * 1024]), None)],
            &s,
        )
        .unwrap();
        assert_eq!(ok.files.len(), 1);
        // 1MB + 1：拒绝
        let err = stage(
            &[inline("b.bin", enc(&[0u8; 1024 * 1024 + 1]), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("超过单附件上限"), "err: {}", err);
    }

    #[test]
    fn rejects_total_oversize() {
        let s = security(1024, 6, &[]);
        let err = stage(
            &[
                inline("a.bin", enc(&[0u8; 4]), None),
                inline("b.bin", enc(&[0u8; 4]), None),
            ],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("总大小"), "err: {}", err);
    }

    #[test]
    fn rejects_disallowed_type() {
        let s = security(1024, 2048, &["txt", "pdf"]);
        let err = stage(
            &[inline("evil.exe", enc(b"x"), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不在白名单"), "err: {}", err);
    }

    #[test]
    fn rejects_bad_base64() {
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[inline("a.bin", "%%%not-base64%%%".into(), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不是合法 base64"), "err: {}", err);
    }

    #[test]
    fn rejects_path_traversal() {
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[inline("../../etc/passwd", enc(b"x"), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("非法附件文件名"), "err: {}", err);
    }

    // ---- 宽容 base64 解码 ----

    #[test]
    fn lenient_decodes_whitespace() {
        // LLM 常把长 base64 按行折行
        let raw = b"hello world";
        let b64 = enc(raw);
        let wrapped = format!("  {}\n\t{}\r\n ", &b64[..8], &b64[8..]);
        assert_eq!(decode_base64_lenient(&wrapped).unwrap(), raw);
        // 解出的字节确实能通过 stage_attachments
        let s = security(1024, 2048, &["txt"]);
        let st = stage(
            &[inline("a.txt", wrapped, None)],
            &s,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&st.files[0].path).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn lenient_decodes_url_safe_alphabet() {
        // 0xfb 0xff 0xbf -> 标准字母表含 '+' '/'，URL-safe 为 '-' '_'
        let raw: Vec<u8> = vec![0xfb, 0xff, 0xbf, 0x10];
        let std_b64 = enc(&raw);
        assert!(std_b64.contains('+') || std_b64.contains('/'), "{std_b64}");
        let url_safe = std_b64.replace('+', "-").replace('/', "_");
        assert_eq!(decode_base64_lenient(&url_safe).unwrap(), raw);
    }

    #[test]
    fn lenient_decodes_missing_padding() {
        // "hello" 的 base64 带 '=' padding
        let raw = b"hello";
        let b64 = enc(raw);
        assert!(b64.ends_with('='), "{b64}");
        let unpadded = b64.trim_end_matches('=');
        assert_eq!(decode_base64_lenient(unpadded).unwrap(), raw);
    }

    #[test]
    fn lenient_rejects_impossible_length() {
        // 长度 %4 == 1：无论怎么补 padding 都不可能是合法 base64
        assert!(decode_base64_lenient("QUJD").is_ok()); // 4 的倍数：合法
        assert!(decode_base64_lenient("QUJDR").is_err()); // %4 == 1
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[inline("a.bin", "QUJDR".into(), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不是合法 base64"), "err: {}", err);
    }

    #[test]
    fn lenient_strips_data_uri_prefix() {
        let raw = b"%PDF-1.4 fake pdf";
        let uri = format!("data:application/pdf;base64,{}", enc(raw));
        assert_eq!(decode_base64_lenient(&uri).unwrap(), raw);
        // 无逗号的 data: 前缀视为非法
        assert!(decode_base64_lenient("data:application/pdf;base64").is_err());
    }

    // ---- sha256 完整性校验 ----

    fn sha256_hex(data: &[u8]) -> String {
        hex_lower(&Sha256::digest(data))
    }

    #[test]
    fn accepts_correct_sha256_lower_and_upper() {
        let raw = b"attachment payload";
        let digest = sha256_hex(raw);
        for expected in [digest.clone(), digest.to_uppercase()] {
            let s = security(1024, 2048, &["txt"]);
            let st = stage(&[inline("a.txt", enc(raw), Some(expected))], &s)
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(&st.files[0].path).unwrap(),
                "attachment payload"
            );
        }
    }

    #[test]
    fn rejects_wrong_sha256() {
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[inline("a.bin", enc(b"actual"), Some(sha256_hex(b"something else")))],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("sha256 校验失败"), "err: {}", err);
    }

    #[test]
    fn rejects_malformed_sha256() {
        let s = security(1024, 2048, &[]);
        for bad in [
            "deadbeef".to_string(),
            "z".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ] {
            let err = stage(&[inline("a.bin", enc(b"x"), Some(bad.clone()))], &s)
            .unwrap_err();
            assert!(
                err.contains("sha256 格式非法"),
                "input {} err: {}",
                bad,
                err
            );
        }
    }

    #[test]
    fn detects_truncation_on_4char_boundary() {
        // 模拟 LLM 输出被截断：在 4 字符边界截断后仍是合法 base64，
        // 无 sha256 时无声通过，带 sha256 时被发现。
        let raw = b"this is a longer payload that gets truncated in transit 0123456789";
        let b64 = enc(raw);
        let truncated = &b64[..(b64.len() / 4 - 2) * 4]; // 砍掉末尾两组 4 字符
        let s = security(1024, 2048, &[]);

        // 无 sha256：合法 base64，静默通过（demo 现状缺陷）
        let ok = stage(
            &[inline("a.bin", truncated.to_string(), None)],
            &s,
        )
        .unwrap();
        assert!(std::fs::metadata(&ok.files[0].path).unwrap().len() < raw.len() as u64);

        // 带原文件 sha256：被拒
        let err = stage(
            &[inline("a.bin", truncated.to_string(), Some(sha256_hex(raw)))],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("sha256 校验失败"), "err: {}", err);
    }

    #[test]
    fn lenient_bad_base64_still_rejected() {
        // 原有"非法内容"路径在宽容解码后仍必须拒绝
        assert!(decode_base64_lenient("%%%not-base64%%%").is_err());
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[inline("a.bin", "%%%not-base64%%%".into(), None)],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不是合法 base64"), "err: {}", err);
    }

    #[test]
    fn sha256_field_is_optional_and_ignored_for_absent() {
        // 不传 sha256 时字段为 None，且行为与加固前一致（成功落盘）
        let v: AttachmentInput =
            serde_json::from_str(r#"{"filename":"a.txt","content":"aGVsbG8="}"#).unwrap();
        assert!(v.sha256.is_none());
        let s = security(1024, 2048, &[]);
        let st = stage(&[v], &s).unwrap();
        assert_eq!(st.files.len(), 1);
    }

    // ---- 带外上传（upload_id）路径 ----

    const OWNER: [u8; 32] = [3u8; 32];

    #[test]
    fn content_and_upload_id_are_mutually_exclusive() {
        let both: AttachmentInput = serde_json::from_str(
            r#"{"filename":"a.txt","content":"aGk=","upload_id":"deadbeef"}"#,
        )
        .unwrap();
        assert_eq!(
            both.source().unwrap_err(),
            "附件必须且只能提供 content 或 upload_id 之一"
        );

        let neither: AttachmentInput = serde_json::from_str(r#"{"filename":"a.txt"}"#).unwrap();
        assert_eq!(
            neither.source().unwrap_err(),
            "附件必须且只能提供 content 或 upload_id 之一"
        );

        let s = security(1024, 2048, &[]);
        let err = stage(&[both], &s).unwrap_err();
        assert!(err.contains("必须且只能提供"), "err: {}", err);
    }

    #[test]
    fn staged_upload_uses_stored_bytes_and_consumes_entry() {
        let data = b"\x00\x01binary upload payload";
        let (store, id) = stored_upload(OWNER, "report.bin", data);
        let s = security(10 * 1024 * 1024, 10 * 1024 * 1024, &["bin"]);

        let st = stage_attachments(
            &[by_upload(&id, None, Some(sha256_hex(data)))],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap();

        // 文件名缺省取上传时的名字，内容即上传的字节
        assert_eq!(st.files.len(), 1);
        assert_eq!(st.files[0].filename, "report.bin");
        assert_eq!(std::fs::read(&st.files[0].path).unwrap(), data.to_vec());
        // 上传条目的临时目录已交给本次发送托管
        assert_eq!(st.upload_dir_count(), 1);

        // 条目已被消费：再次使用同一 id 失败
        assert!(store.take(&id, &OWNER).is_err());
    }

    #[test]
    fn staged_upload_releases_disk_file_on_drop() {
        let data = b"cleanup me";
        let (store, id) = stored_upload(OWNER, "a.bin", data);
        let s = security(1024, 2048, &["bin"]);
        let st = stage_attachments(&[by_upload(&id, None, None)], &s, &store.resolver(OWNER))
        .unwrap();
        let path = st.files[0].path.clone();
        assert!(path.exists());
        // 发送结束（成功或失败）随 StagedAttachments drop 清理磁盘文件
        drop(st);
        assert!(!path.exists(), "upload temp file should be removed on drop");
    }

    #[test]
    fn staged_upload_rejects_filename_mismatch() {
        let (store, id) = stored_upload(OWNER, "real.bin", b"data");
        let s = security(1024, 2048, &["bin"]);
        let err = stage_attachments(
            &[by_upload(&id, Some("other.bin"), None)],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap_err();
        assert!(err.contains("与上传时不一致"), "err: {}", err);

        // 同名提供则通过
        let ok = stage_attachments(
            &[by_upload(&id, Some("real.bin"), None)],
            &s,
            &store.resolver(OWNER),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn staged_upload_rejects_wrong_sha256() {
        let data = b"actual bytes";
        let (store, id) = stored_upload(OWNER, "a.bin", data);
        let s = security(1024, 2048, &["bin"]);
        let err = stage_attachments(
            &[by_upload(&id, None, Some(sha256_hex(b"different bytes")))],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap_err();
        assert!(err.contains("sha256 校验失败"), "err: {}", err);
        // 校验失败不应消费条目
        assert!(store.take(&id, &OWNER).is_ok());
    }

    #[test]
    fn staged_upload_rejects_malformed_sha256() {
        let (store, id) = stored_upload(OWNER, "a.bin", b"x");
        let s = security(1024, 2048, &["bin"]);
        let err = stage_attachments(
            &[by_upload(&id, None, Some("not-a-digest".into()))],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap_err();
        assert!(err.contains("sha256 格式非法"), "err: {}", err);
    }

    #[test]
    fn staged_upload_rejects_foreign_owner() {
        let (store, id) = stored_upload(OWNER, "a.bin", b"x");
        let s = security(1024, 2048, &["bin"]);
        let err = stage_attachments(&[by_upload(&id, None, None)], &s, &store.resolver([9u8; 32]))
        .unwrap_err();
        assert!(err.contains("不属于当前 API key"), "err: {}", err);
    }

    #[test]
    fn staged_upload_rejects_pending_and_unknown_id() {
        let store = crate::uploads::UploadStore::new(900, 16, 1024);
        let id = store.create(OWNER, "a.bin".into()).unwrap();
        let s = security(1024, 2048, &["bin"]);

        // 尚未上传
        let err = stage_attachments(&[by_upload(&id, None, None)], &s, &store.resolver(OWNER))
        .unwrap_err();
        assert!(err.contains("尚未完成上传"), "err: {}", err);

        // 未知 id
        let err = stage_attachments(&[by_upload("ff".repeat(16).as_str(), None, None)], &s, &store.resolver(OWNER))
        .unwrap_err();
        assert!(err.contains("不存在或已过期"), "err: {}", err);
    }

    #[test]
    fn staged_upload_enforces_extension_and_total_size() {
        // 扩展名白名单在上传路径同样生效（文件名来自上传时）
        let (store, id) = stored_upload(OWNER, "evil.exe", b"x");
        let s = security(1024, 2048, &["txt"]);
        let err = stage_attachments(&[by_upload(&id, None, None)], &s, &store.resolver(OWNER))
        .unwrap_err();
        assert!(err.contains("不在白名单"), "err: {}", err);

        // 总量上限按上传文件实际大小计算
        let (store2, id2) = stored_upload(OWNER, "a.txt", &[0u8; 100]);
        let s2 = security(1024, 50, &["txt"]);
        let err = stage_attachments(
            &[by_upload(&id2, None, None)],
            &s2,
            &store2.resolver(OWNER),
        )
        .unwrap_err();
        assert!(err.contains("总大小"), "err: {}", err);
    }

    #[test]
    fn upload_path_accepts_file_over_1mb() {
        // 突破旧的 1MB 内联上限：上传路径按新默认 10MB 校验
        let data = vec![0x5au8; 3 * 1024 * 1024];
        let (store, id) = stored_upload(OWNER, "big.bin", &data);
        let s = security(10 * 1024 * 1024, 10 * 1024 * 1024, &["bin"]);
        let st = stage_attachments(
            &[by_upload(&id, None, Some(sha256_hex(&data)))],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&st.files[0].path).unwrap().len(), 3 * 1024 * 1024);
    }

    #[test]
    fn mixed_inline_and_upload_deduplicates_filenames() {
        let (store, id) = stored_upload(OWNER, "a.txt", b"uploaded");
        let s = security(1024, 4096, &["txt"]);
        let err = stage_attachments(
            &[
                inline("a.txt", enc(b"inline"), None),
                by_upload(&id, None, None),
            ],
            &s,
            &store.resolver(OWNER),
        )
        .unwrap_err();
        assert!(err.contains("重复附件文件名"), "err: {}", err);
    }

    #[test]
    fn upload_attachment_requires_filename_inline_style() {
        // 内联路径仍必须显式提供 filename
        let s = security(1024, 2048, &[]);
        let err = stage(
            &[AttachmentInput {
                filename: None,
                content: Some(enc(b"x")),
                upload_id: None,
                sha256: None,
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("缺少文件名"), "err: {}", err);
    }

    #[test]
    fn validate_filename_rules() {
        let s = security(1024, 2048, &["txt"]);
        assert!(validate_filename("a.txt", &s).is_ok());
        assert!(validate_filename("", &s).unwrap_err().contains("缺少文件名"));
        assert!(validate_filename("../x.txt", &s)
            .unwrap_err()
            .contains("非法附件文件名"));
        assert!(validate_filename("dir/x.txt", &s)
            .unwrap_err()
            .contains("非法附件文件名"));
        assert!(validate_filename("a.exe", &s).unwrap_err().contains("不在白名单"));
    }
}

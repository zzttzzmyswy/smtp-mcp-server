use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// MCP tools/call 中 send_email 的单个附件描述（文件名 + base64 内容 + 可选校验和）
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AttachmentInput {
    pub filename: String,
    /// base64 编码的文件内容
    pub content: String,
    /// 可选：解码后**原始字节**的 SHA-256 hex（64 字符，大小写均可）。
    /// 由调用方先算好；服务端据此发现内容在传输中被截断或改写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 校验附件 sha256：格式必须为 64 位 hex；值必须等于解码后字节的 SHA-256（忽略大小写）。
fn verify_sha256(filename: &str, decoded: &[u8], expected: &str) -> Result<(), String> {
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

#[derive(Debug)]
pub struct StagedFile {
    pub filename: String,
    pub path: PathBuf,
}

/// 校验通过的附件已落到临时目录；Drop 时自动清理整个临时目录。
#[derive(Debug)]
pub struct StagedAttachments {
    _dir: TempDir,
    pub files: Vec<StagedFile>,
}

/// 附件校验与落盘。任何校验失败返回带明确原因的 Err，且不会留下临时文件。
pub fn stage_attachments(
    attachments: &[AttachmentInput],
    security: &crate::config::Security,
) -> Result<StagedAttachments, String> {
    if attachments.is_empty() {
        return Ok(StagedAttachments {
            _dir: TempDir::new().map_err(|e| format!("创建临时目录失败: {}", e))?,
            files: Vec::new(),
        });
    }

    let dir = TempDir::new().map_err(|e| format!("创建临时目录失败: {}", e))?;

    let mut total = 0usize;
    let mut files = Vec::with_capacity(attachments.len());
    let mut seen = std::collections::HashSet::new();

    for att in attachments {
        let filename = att.filename.trim().to_string();
        if filename.is_empty() {
            return Err("附件缺少文件名".into());
        }
        // 防止路径穿越：只允许纯文件名
        if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
            return Err(format!("非法附件文件名: {}", filename));
        }
        if !seen.insert(filename.clone()) {
            return Err(format!("重复附件文件名: {}", filename));
        }

        // 校验扩展名白名单
        let ext = Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !security.allowed_attachment_extensions.is_empty()
            && !security
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

        // base64 解码（宽容：容忍空白/URL-safe/缺 padding/data URI；仍拒绝非法内容）
        let decoded = decode_base64_lenient(&att.content)
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

        // 写入临时目录（命名冲突由唯一前缀避免）
        let path = dir
            .path()
            .join(format!("{:08x}-{}", rand::random::<u32>(), filename));
        std::fs::write(&path, &decoded)
            .map_err(|e| format!("写入临时附件 {} 失败: {}", filename, e))?;

        files.push(StagedFile { filename, path });
    }

    Ok(StagedAttachments { _dir: dir, files })
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

    #[test]
    fn empty_ok() {
        let s = security(10, 10, &[]);
        let st = stage_attachments(&[], &s).unwrap();
        assert!(st.files.is_empty());
    }

    #[test]
    fn stages_valid_attachment() {
        let s = security(1024, 2048, &["txt"]);
        let st = stage_attachments(
            &[AttachmentInput {
                filename: "a.txt".into(),
                content: enc(b"hello"),
                sha256: None,
            }],
            &s,
        )
        .unwrap();
        assert_eq!(st.files.len(), 1);
        assert_eq!(std::fs::read_to_string(&st.files[0].path).unwrap(), "hello");
    }

    #[test]
    fn rejects_oversized_attachment() {
        let s = security(4, 4, &[]);
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "big.bin".into(),
                content: enc(&[0u8; 5]),
                sha256: None,
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("超过单附件上限"), "err: {}", err);
    }

    #[test]
    fn default_1mb_boundary() {
        let s = security(1024 * 1024, 1024 * 1024, &[]);
        // 恰好 1MB：通过
        let ok = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: enc(&[0u8; 1024 * 1024]),
                sha256: None,
            }],
            &s,
        )
        .unwrap();
        assert_eq!(ok.files.len(), 1);
        // 1MB + 1：拒绝
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "b.bin".into(),
                content: enc(&[0u8; 1024 * 1024 + 1]),
                sha256: None,
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("超过单附件上限"), "err: {}", err);
    }

    #[test]
    fn rejects_total_oversize() {
        let s = security(1024, 6, &[]);
        let err = stage_attachments(
            &[
                AttachmentInput {
                    filename: "a.bin".into(),
                    content: enc(&[0u8; 4]),
                    sha256: None,
                },
                AttachmentInput {
                    filename: "b.bin".into(),
                    content: enc(&[0u8; 4]),
                    sha256: None,
                },
            ],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("总大小"), "err: {}", err);
    }

    #[test]
    fn rejects_disallowed_type() {
        let s = security(1024, 2048, &["txt", "pdf"]);
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "evil.exe".into(),
                content: enc(b"x"),
                sha256: None,
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不在白名单"), "err: {}", err);
    }

    #[test]
    fn rejects_bad_base64() {
        let s = security(1024, 2048, &[]);
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: "%%%not-base64%%%".into(),
                sha256: None,
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("不是合法 base64"), "err: {}", err);
    }

    #[test]
    fn rejects_path_traversal() {
        let s = security(1024, 2048, &[]);
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "../../etc/passwd".into(),
                content: enc(b"x"),
                sha256: None,
            }],
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
        let st = stage_attachments(
            &[AttachmentInput {
                filename: "a.txt".into(),
                content: wrapped,
                sha256: None,
            }],
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
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: "QUJDR".into(),
                sha256: None,
            }],
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
            let st = stage_attachments(
                &[AttachmentInput {
                    filename: "a.txt".into(),
                    content: enc(raw),
                    sha256: Some(expected),
                }],
                &s,
            )
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
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: enc(b"actual"),
                sha256: Some(sha256_hex(b"something else")),
            }],
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
            let err = stage_attachments(
                &[AttachmentInput {
                    filename: "a.bin".into(),
                    content: enc(b"x"),
                    sha256: Some(bad.clone()),
                }],
                &s,
            )
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
        let ok = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: truncated.to_string(),
                sha256: None,
            }],
            &s,
        )
        .unwrap();
        assert!(std::fs::metadata(&ok.files[0].path).unwrap().len() < raw.len() as u64);

        // 带原文件 sha256：被拒
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: truncated.to_string(),
                sha256: Some(sha256_hex(raw)),
            }],
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
        let err = stage_attachments(
            &[AttachmentInput {
                filename: "a.bin".into(),
                content: "%%%not-base64%%%".into(),
                sha256: None,
            }],
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
        let st = stage_attachments(&[v], &s).unwrap();
        assert_eq!(st.files.len(), 1);
    }
}

use base64::Engine;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// MCP tools/call 中 send_email 的单个附件描述（文件名 + base64 内容）
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AttachmentInput {
    pub filename: String,
    /// base64 编码的文件内容
    pub content: String,
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

        // base64 解码（拒绝非法 base64）
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(att.content.as_bytes())
            .map_err(|_| format!("附件 {} 内容不是合法 base64", filename))?;

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
            max_request_bytes: 14 * 1024 * 1024,
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
                },
                AttachmentInput {
                    filename: "b.bin".into(),
                    content: enc(&[0u8; 4]),
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
            }],
            &s,
        )
        .unwrap_err();
        assert!(err.contains("非法附件文件名"), "err: {}", err);
    }
}

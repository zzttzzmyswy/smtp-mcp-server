use crate::attachments::StagedAttachments;
use crate::config::Smtp;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

#[derive(Debug)]
pub struct SendReport {
    pub receivers: Vec<String>,
    pub subject: String,
    pub attachment_count: usize,
    pub total_size_bytes: usize,
}

/// 简单邮箱格式校验（够用的边界检查，不做完整 RFC 5322 解析）
fn valid_email(addr: &str) -> bool {
    let addr = addr.trim();
    if addr.len() > 320 || addr.len() < 3 || !addr.contains('@') {
        return false;
    }
    let (local, domain) = addr.rsplit_once('@').unwrap();
    !local.is_empty()
        && !local.contains(' ')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.split('.').all(|p| !p.is_empty())
}

/// 扩展名 → MIME，兜底 application/octet-stream
fn mime_for(filename: &str) -> mime::Mime {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" | "md" | "log" | "csv" | "yml" | "yaml" | "toml" | "json" => mime::TEXT_PLAIN,
        "html" | "htm" => mime::TEXT_HTML,
        "pdf" => "application/pdf".parse().unwrap(),
        "png" => mime::IMAGE_PNG,
        "jpg" | "jpeg" => mime::IMAGE_JPEG,
        "gif" => mime::IMAGE_GIF,
        "bmp" => mime::IMAGE_BMP,
        "webp" => "image/webp".parse().unwrap(),
        "svg" => mime::IMAGE_SVG,
        "doc" => "application/msword".parse().unwrap(),
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            .parse()
            .unwrap(),
        "xls" => "application/vnd.ms-excel".parse().unwrap(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            .parse()
            .unwrap(),
        "ppt" => "application/vnd.ms-powerpoint".parse().unwrap(),
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            .parse()
            .unwrap(),
        "zip" => "application/zip".parse().unwrap(),
        "gz" => "application/gzip".parse().unwrap(),
        "tar" => "application/x-tar".parse().unwrap(),
        "7z" => "application/x-7z-compressed".parse().unwrap(),
        "mp4" => "video/mp4".parse().unwrap(),
        "mov" => "video/quicktime".parse().unwrap(),
        "mp3" => "audio/mpeg".parse().unwrap(),
        _ => mime::APPLICATION_OCTET_STREAM,
    }
}

/// 发送一封电子邮件。`html_body` 为最终邮件正文（HTML），已由调用方完成模板渲染；
/// 附件内容从 StagedAttachments 指向的临时文件读取（已通过校验），
/// 发送完成后由调用方负责 Drop 清理。
pub async fn send_email(
    smtp: &Smtp,
    receiver_whitelist: &[String],
    receivers: Vec<String>,
    subject: String,
    html_body: String,
    staged: &StagedAttachments,
) -> Result<SendReport, String> {
    if receivers.is_empty() {
        return Err("receiver 至少需要一个收件人".into());
    }

    // 收件人校验 + 白名单（空 = 不限）
    let whitelist: Vec<String> = receiver_whitelist
        .iter()
        .map(|w| w.trim().to_ascii_lowercase())
        .collect();

    let mut parsed_receivers = Vec::new();
    for r in &receivers {
        let r = r.trim();
        if !valid_email(r) {
            return Err(format!("收件人邮箱格式非法: {}", r));
        }
        let lower = r.to_ascii_lowercase();
        if !whitelist.is_empty() {
            let allowed = whitelist.iter().any(|w| {
                lower == *w
                    || lower.ends_with(&format!(".{}", w))
                    || lower.ends_with(&format!("@{}", w))
            });
            if !allowed {
                return Err(format!("收件人 {} 不在白名单内", r));
            }
        }
        parsed_receivers.push(parse_mailbox(r)?);
    }

    let from: Mailbox = smtp
        .from
        .parse()
        .map_err(|_| format!("smtp.from 不是合法的邮箱地址: {}", smtp.from))?;

    // 组装 MIME：HTML 正文 + 附件
    let html_part = SinglePart::html(html_body.clone());
    let mut parts: Vec<lettre::message::SinglePart> = Vec::new();
    let mut total_size = 0usize;
    for f in &staged.files {
        let bytes = std::fs::read(&f.path)
            .map_err(|e| format!("读取临时附件 {} 失败: {}", f.filename, e))?;
        total_size += bytes.len();
        let mime = mime_for(&f.filename);
        let ct = ContentType::parse(mime.to_string().as_str())
            .map_err(|e| format!("附件 MIME 解析失败: {}", e))?;
        parts.push(Attachment::new(f.filename.clone()).body(bytes, ct));
    }

    let mut builder = Message::builder().from(from).subject(subject.clone());
    for t in &parsed_receivers {
        builder = builder.to(t.clone());
    }
    let message = if parts.is_empty() {
        builder.singlepart(html_part).map_err(|e| e.to_string())?
    } else {
        let mut mp = MultiPart::mixed().singlepart(html_part);
        for p in parts {
            mp = mp.singlepart(p);
        }
        builder.multipart(mp).map_err(|e| e.to_string())?
    };

    // 构建 SMTP transport（每次调用现建，避免常驻连接提升内存占用）
    let tb = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host)
        .port(smtp.port)
        .credentials(Credentials::new(
            smtp.user.clone(),
            smtp.pass.clone().unwrap_or_default(),
        ));
    let tb = match smtp.tls.as_str() {
        "ssl" => {
            let params = TlsParameters::new_rustls(smtp.host.clone())
                .map_err(|e| format!("初始化 TLS 参数失败: {}", e))?;
            tb.tls(Tls::Wrapper(params))
        }
        "starttls" => {
            let params = TlsParameters::new_rustls(smtp.host.clone())
                .map_err(|e| format!("初始化 TLS 参数失败: {}", e))?;
            tb.tls(Tls::Required(params))
        }
        _ => tb.tls(Tls::None),
    };
    let transport = tb.build();

    transport
        .send(message)
        .await
        .map_err(|e| format!("SMTP 发送失败: {}", e))?;

    Ok(SendReport {
        receivers: receivers.iter().map(|r| r.trim().to_string()).collect(),
        subject,
        attachment_count: staged.files.len(),
        total_size_bytes: total_size,
    })
}

fn parse_mailbox(r: &str) -> Result<Mailbox, String> {
    r.parse().map_err(|_| format!("收件人解析失败: {}", r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation() {
        assert!(valid_email("a@b.com"));
        assert!(valid_email("user.name+tag@sub.example.cn"));
        assert!(!valid_email("not-an-email"));
        assert!(!valid_email("a@b"));
        assert!(!valid_email("@b.com"));
        assert!(!valid_email("a@.com"));
        assert!(!valid_email("a b@c.com"));
    }

    #[test]
    fn mime_mapping() {
        assert_eq!(
            mime_for("a.PDF"),
            "application/pdf".parse::<mime::Mime>().unwrap()
        );
        assert_eq!(mime_for("a.txt"), mime::TEXT_PLAIN);
        assert_eq!(mime_for("a.png"), mime::IMAGE_PNG);
        assert_eq!(mime_for("a.unknown"), mime::APPLICATION_OCTET_STREAM);
    }

    #[test]
    fn receivers_whitelist_logic_accepts() {
        let whitelist = vec!["example.com".to_string()];
        let r = "user@example.com".to_ascii_lowercase();
        assert!(whitelist.iter().any(|w| r == *w
            || r.ends_with(&format!(".{}", w))
            || r.ends_with(&format!("@{}", w))));
        let r2 = "user@sub.example.com".to_ascii_lowercase();
        assert!(whitelist.iter().any(|w| r2 == *w
            || r2.ends_with(&format!(".{}", w))
            || r2.ends_with(&format!("@{}", w))));
    }

    #[test]
    fn receivers_whitelist_rejects() {
        let whitelist = vec!["allowed.com".to_string()];
        let r = "user@evil.com".to_string();
        assert!(!whitelist.iter().any(|w| r == *w
            || r.ends_with(&format!(".{}", w))
            || r.ends_with(&format!("@{}", w))));
    }
}

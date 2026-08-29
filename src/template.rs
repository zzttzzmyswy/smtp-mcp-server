//! 默认邮件 HTML 模板渲染。
//! - AI Agent 未提供自定义 `html_body` 时，用此模板渲染邮件正文；
//! - 模板为桌面/移动端响应式，支持文本 / 表格 / 图片 / 引用 / 列表 / 按钮等区块（class 见模板）。

pub const DEFAULT_BRAND: &str = "Multica MCP";

pub const TEMPLATE: &str = include_str!("../templates/mail.html");

/// 渲染参数
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    pub subject: String,
    /// 纯文本正文（无 html_body 时的内容来源）
    pub body: String,
    /// AI Agent 自带完整 HTML（最高优先级）
    pub html_body: Option<String>,
    /// 页眉品牌名（缺省 DEFAULT_BRAND）
    pub brand: Option<String>,
    /// 问候语（缺省"您好："，空串则不渲染问候行）
    pub greeting: Option<String>,
    /// 落款人名（缺省沿用品牌名；空串则不渲染签名区）
    pub sign_name: Option<String>,
}

pub fn render(opts: &RenderOptions) -> String {
    let brand = opts
        .brand
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_BRAND);
    let greeting = opts.greeting.as_deref().unwrap_or("您好：");
    // 签名区：None（未提供）→ 沿用品牌名显示；Some("") → 不显示签名区
    let sign_name = match &opts.sign_name {
        None => Some(brand),
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
    };

    // 正文 HTML：html_body 优先（AI Agent 自带模板）；否则纯文本 body 转 HTML
    let (body_html, raw_for_preheader) = match &opts.html_body {
        Some(html) => {
            let trimmed = html.trim();
            (trimmed.to_string(), trimmed.to_string())
        }
        None => (plain_to_html(&opts.body), opts.body.clone()),
    };
    let preheader = make_preheader(&raw_for_preheader);

    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{:04} 年 {:02} 月 {:02} 日",
        now.year(),
        now.month() as u8,
        now.day()
    );

    let kicker_cell = "<div style=\"margin-top:26px;font-family:Georgia,'Songti SC','STSong',serif;font-size:14px;color:#c9a35c;letter-spacing:4px;\">NOTIFICATION · 通知</div>".to_string();
    let greeting_cell = if greeting.is_empty() {
        String::new()
    } else {
        format!(
            "<p style=\"margin:0 0 18px 0;font-family:'Helvetica Neue',Helvetica,Arial,'Microsoft YaHei',sans-serif;font-size:15px;line-height:1.9;color:#3a4556;\">{}</p>",
            escape_html(greeting)
        )
    };
    let seal_text = format!("{} 印", brand);
    let sign_cell = sign_name
        .map(|name| {
            let initial = name
                .chars()
                .next()
                .map(|c| c.to_uppercase().collect::<String>())
                .unwrap_or_default();
            let role = "Multica · 自动通知";
            format!(
                concat!(
                    "<table role=\"presentation\" width=\"100%\" cellspacing=\"0\" cellpadding=\"0\" border=\"0\" style=\"margin-top:34px;\">",
                    "<tr><td style=\"border-top:1px solid #e6e9ef;padding:18px 0 6px 0;\">",
                    "<table role=\"presentation\" cellspacing=\"0\" cellpadding=\"0\" border=\"0\"><tr>",
                    "<td valign=\"middle\" style=\"padding-right:14px;\">",
                    "<table role=\"presentation\" width=\"40\" height=\"40\" cellspacing=\"0\" cellpadding=\"0\" border=\"0\" style=\"background-color:#eef1f6;border-radius:50%;\">",
                    "<tr><td align=\"center\" valign=\"middle\" style=\"font-family:Arial,sans-serif;font-size:15px;font-weight:700;color:#1f3a5f;\">{}</td></tr></table></td>",
                    "<td valign=\"middle\"><div style=\"font-family:'Helvetica Neue',Helvetica,Arial,'Microsoft YaHei',sans-serif;font-size:15px;font-weight:600;color:#1f3a5f;\">{}</div>",
                    "<div style=\"font-family:'Helvetica Neue',Helvetica,Arial,'Microsoft YaHei',sans-serif;font-size:12px;color:#8792a3;margin-top:2px;\">{}</div></td>",
                    "<td valign=\"middle\" align=\"right\" style=\"padding-left:24px;\">",
                    "<span style=\"border:2px solid #c0392b;border-radius:3px;padding:3px 8px;font-family:KaiTi,'STKaiti','Kaiti SC',serif;font-size:13px;color:#c0392b;letter-spacing:3px;\">已阅</span>",
                    "</td></tr></table></td></tr></table>"
                ),
                initial,
                escape_html(name),
                role
            )
        })
        .unwrap_or_default();

    let footer = "本邮件由 smtp-mcp-server · MCP 邮件服务自动发出";

    TEMPLATE
        .replace("{{PREHEADER}}", &preheader)
        .replace("{{BRAND}}", escape_html(brand).as_str())
        .replace("{{DATE}}", &date)
        .replace("{{KICKER_CELL}}", &kicker_cell)
        .replace("{{TITLE}}", escape_html(&opts.subject).as_str())
        .replace("{{SEAL}}", escape_html(&seal_text).as_str())
        .replace("{{GREETING_CELL}}", &greeting_cell)
        .replace("{{BODY}}", &body_html)
        .replace("{{SIGN_CELL}}", &sign_cell)
        .replace("{{FOOTER}}", footer)
}

/// 纯文本正文 → 模板正文片段：按空行分段，段内换行转 <br>，全部转义防注入
pub fn plain_to_html(text: &str) -> String {
    let mut out = String::new();
    for para in text.split("\n\n") {
        let para_trimmed = para.trim();
        if para_trimmed.is_empty() {
            continue;
        }
        let inner = escape_html(para_trimmed).replace('\n', "<br>");
        out.push_str(&format!("<p class=\"mail-p\">{}</p>\n", inner));
    }
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 邮件预览文字：去除 HTML 标签后取前 ~90 字符
fn make_preheader(raw: &str) -> String {
    let text = strip_tags(raw);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = text.chars().take(88).collect();
    if text.chars().count() > 88 {
        out.push('…');
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_body_is_wrapped_and_escaped() {
        let html = plain_to_html("第一行\n第二行\n\n<b>危险</b> & <script>");
        assert!(html.contains("<p class=\"mail-p\">第一行<br>第二行</p>"));
        assert!(
            html.contains("&lt;b&gt;危险&lt;/b&gt; &amp; &lt;script&gt;"),
            "{}",
            html
        );
    }

    #[test]
    fn render_uses_html_body_when_provided() {
        let r = render(&RenderOptions {
            subject: "S".into(),
            body: "plain".into(),
            html_body: Some("<div class=\"mail-callout\">自定义</div>".into()),
            ..Default::default()
        });
        assert!(r.contains("mail-callout"), "自定义 html 应原样嵌入");
        assert!(!plain_to_html("plain").is_empty());
    }

    #[test]
    fn render_all_placeholders_filled() {
        let r = render(&RenderOptions {
            subject: "测试主题 <ok>".into(),
            body: "正文第一段。\n\n第二段。".into(),
            ..Default::default()
        });
        assert!(!r.contains("{{"), "不应残留模板占位符: {:?}", r);
        assert!(r.contains("测试主题 &lt;ok&gt;"));
        assert!(r.contains("Multica MCP"));
        assert!(r.contains("NOTIFICATION"));
        assert!(r.contains("您好"));
        assert!(r.contains("已阅"));
        assert!(r.contains("mail-p"));
        assert!(r.contains("<!-- 邮件预览文字 -->"));
    }

    #[test]
    fn brand_and_greeting_take_effect() {
        let r = render(&RenderOptions {
            subject: "S".into(),
            body: "B".into(),
            brand: Some("Aurora".into()),
            greeting: Some("尊敬的伙伴：".into()),
            sign_name: Some("张三".into()),
            ..Default::default()
        });
        assert!(r.contains(">Aurora<"));
        assert!(r.contains("Aurora 印"));
        assert!(r.contains("尊敬的伙伴"));
        assert!(r.contains("张三"));
        assert!(r.contains("张"));
    }

    #[test]
    fn empty_sign_name_omits_sign_block() {
        let r = render(&RenderOptions {
            subject: "S".into(),
            body: "B".into(),
            sign_name: Some("".into()),
            ..Default::default()
        });
        assert!(!r.contains("已阅"));
    }

    #[test]
    fn subject_and_body_escaped_no_injection() {
        let r = render(&RenderOptions {
            subject: "x <script>alert(1)</script>".into(),
            body: "<img src=x onerror=alert(1)>".into(),
            ..Default::default()
        });
        assert!(!r.contains("<script>alert"), "注入应被转义");
        assert!(r.contains("&lt;script&gt;"));
        assert!(!r.contains("<img src=x onerror"), "img 注入应被转义");
    }
}

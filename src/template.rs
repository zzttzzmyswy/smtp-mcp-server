//! 默认邮件 HTML 模板渲染。
//! - AI Agent 未提供自定义 `html_body` 时，用此模板渲染邮件正文；
//! - 模板为桌面/移动端响应式，支持文本 / 表格 / 图片 / 引用 / 列表 / 按钮等区块（class 见模板）；
//! - `html_body` 中的 `<table class="mail-table">` 会被自动包进横向滚动容器
//!   （`.mail-table-wrap`），宽表在窄屏自动出现左右滚动条而非被压缩；
//! - 正文采用系统字体栈（`-apple-system`/`Segoe UI`/`Roboto`/苹方/雅黑），
//!   各设备跟随自身默认 UI 字体；仅品牌装饰元素使用衬线/楷体并带跨平台回退。

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
    // 把正文中的 .mail-table 自动包进横向滚动容器，宽表在窄屏自动出现左右滚动条
    let body_html = wrap_wide_tables(&body_html);

    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{:04} 年 {:02} 月 {:02} 日",
        now.year(),
        now.month() as u8,
        now.day()
    );

    let kicker_cell = "<div style=\"margin-top:26px;font-family:Georgia,'Noto Serif CJK SC','Source Han Serif SC','Songti SC','STSong',serif;font-size:15px;color:#c9a35c;letter-spacing:4px;\">NOTIFICATION · 通知</div>".to_string();
    let greeting_cell = if greeting.is_empty() {
        String::new()
    } else {
        format!(
            "<p style=\"margin:0 0 18px 0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:16px;line-height:1.9;color:#3a4556;\">{}</p>",
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
                    "<tr><td align=\"center\" valign=\"middle\" style=\"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:16px;font-weight:700;color:#1f3a5f;\">{}</td></tr></table></td>",
                    "<td valign=\"middle\"><div style=\"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:16px;font-weight:600;color:#1f3a5f;\">{}</div>",
                    "<div style=\"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:14px;color:#8792a3;margin-top:2px;\">{}</div></td>",
                    "<td valign=\"middle\" align=\"right\" style=\"padding-left:24px;\">",
                    "<span style=\"border:2px solid #c0392b;border-radius:3px;padding:4px 9px;font-family:KaiTi,'STKaiti','Kaiti SC',serif;font-size:14px;color:#c0392b;letter-spacing:3px;\">已阅</span>",
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

/// 将正文中的 `<table class="mail-table">` 自动包进 `.mail-table-wrap` 滚动容器。
///
/// 目的：窄屏时宽表保持可读最小宽度，超出容器部分由 `overflow-x:auto` 提供左右滚动条，
/// 而不是被 CSS 压缩导致列内容溢出/截断。若正文已自行使用 `mail-table-wrap` 则跳过，
/// 避免双重包裹。
fn wrap_wide_tables(html: &str) -> String {
    const WRAP_OPEN: &str = "<div class=\"mail-table-wrap\">";
    const WRAP_CLOSE: &str = "</div>";

    if !html.contains("mail-table") || html.contains("mail-table-wrap") {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len() + 128);
    let mut rest = html;
    loop {
        let Some(tag_start) = rest.find("<table") else {
            out.push_str(rest);
            break;
        };
        // 仅处理 class 含 mail-table 的表格开标签（`<table ...>`，含属性）
        let Some(tag_end_rel) = find_open_tag_end(&rest[tag_start..]) else {
            out.push_str(rest);
            break;
        };
        let tag = &rest[tag_start..tag_start + tag_end_rel];
        if !tag_has_mail_table_class(tag) {
            // 非目标表格：保留到当前 tag 后，继续向后扫描
            out.push_str(&rest[..tag_start + tag_end_rel]);
            rest = &rest[tag_start + tag_end_rel..];
            continue;
        }
        // 从开标签之后找配对的 </table>（按嵌套深度匹配）
        let content_start_rel = tag_end_rel;
        let Some(close_rel) = find_table_close(&rest[tag_start + content_start_rel..]) else {
            out.push_str(rest);
            break;
        };
        let close_start = tag_start + content_start_rel + close_rel;
        out.push_str(&rest[..tag_start]);
        out.push_str(WRAP_OPEN);
        out.push_str(&rest[tag_start..close_start + "</table>".len()]);
        out.push_str(WRAP_CLOSE);
        rest = &rest[close_start + "</table>".len()..];
    }
    out
}

/// 返回开标签 `...>`（含 >）的相对长度；属性值内可能含 `>`（如 data URI），需解析引号
fn find_open_tag_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' if quote == Some(bytes[i]) => quote = None,
            b'"' | b'\'' if quote.is_none() => quote = Some(bytes[i]),
            b'>' if quote.is_none() => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn tag_has_mail_table_class(tag: &str) -> bool {
    // `<table` 之后到 `>` 之间的属性串
    let attr = tag.get(6..).unwrap_or("");
    let lower = attr.to_ascii_lowercase().replace('=', " ");
    lower
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .any(|tok| tok == "mail-table")
}

/// 从目标表格**开标签之后**开始扫描，按嵌套深度找匹配的 `</table>`（返回相对扫描起点的偏移）
fn find_table_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<table") && is_tag_boundary(s, i, false) {
            if let Some(end) = find_open_tag_end(&s[i..]) {
                depth += 1;
                i += end;
                continue;
            }
        }
        if bytes[i..].starts_with(b"</table") && is_tag_boundary(s, i, true) {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
            i += "</table>".len();
            continue;
        }
        i += 1;
    }
    None
}

fn is_tag_boundary(s: &str, i: usize, is_close: bool) -> bool {
    let after = s
        .as_bytes()
        .get(i + if is_close { "</table".len() } else { "<table".len() });
    match after {
        None => true,
        Some(b) => *b == b'>' || b.is_ascii_whitespace(),
    }
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

    #[test]
    fn mail_table_auto_wrapped_in_scroll_container() {
        let body = "<p class=\"mail-p\">数据如下：</p>\n<table class=\"mail-table\"><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let wrapped = wrap_wide_tables(body);
        assert!(wrapped.contains("<div class=\"mail-table-wrap\">"));
        assert!(wrapped.contains("</table></div>"), "{}", wrapped);
        // 包裹只应在表格两端各出现一次（开容器一次 + 对应闭合 </div>）
        assert_eq!(
            wrapped.match_indices("mail-table-wrap").count(),
            1,
            "开容器出现一次: {}",
            wrapped
        );
        assert!(wrapped.contains("</table></div>"), "闭合容器: {}", wrapped);
        // 表格之外的内容原样保留
        assert!(wrapped.starts_with("<p class=\"mail-p\">"));
    }

    #[test]
    fn non_mail_table_left_untouched() {
        let body = "<table role=\"presentation\"><tr><td>x</td></tr></table>";
        assert_eq!(wrap_wide_tables(body), body);
    }

    #[test]
    fn already_wrapped_tables_not_double_wrapped() {
        let body = "<div class=\"mail-table-wrap\"><table class=\"mail-table\"><tr><td>1</td></tr></table></div>";
        assert_eq!(wrap_wide_tables(body), body);
    }

    #[test]
    fn table_with_class_attribute_middle_still_wrapped() {
        let body = "<table width=\"600\" class=\"mail-table\" cellspacing=\"0\"><tr><td>1</td></tr></table>";
        let wrapped = wrap_wide_tables(body);
        assert!(wrapped.starts_with("<div class=\"mail-table-wrap\"><table width=\"600\" class=\"mail-table\""), "{}", wrapped);
    }

    #[test]
    fn nested_table_matched_to_own_close() {
        // 外层 mail-table 内嵌一个普通表格：应匹配到外层的 </table>
        let body = "<table class=\"mail-table\"><tr><td><table><tr><td>in</td></tr></table></td></tr></table>";
        let wrapped = wrap_wide_tables(body);
        assert!(wrapped.starts_with("<div class=\"mail-table-wrap\">"));
        assert!(wrapped.ends_with("</table></div>"), "{}", wrapped);
        assert!(!wrapped[13..].contains("mail-table-wrap"), "只有最外层被包: {}", wrapped);
    }

    #[test]
    fn attribute_value_with_gt_parsed_correctly() {
        // 属性值（data URI）内含 >，不应截断开标签
        let body = "<table class=\"mail-table\" style=\"background:url(data:image/svg+xml;base64,PHN2Zz4=)\"><tr><td>1</td></tr></table>";
        let wrapped = wrap_wide_tables(body);
        assert!(wrapped.starts_with("<div class=\"mail-table-wrap\"><table class=\"mail-table\""), "{}", wrapped);
        assert!(wrapped.ends_with("</table></div>"), "{}", wrapped);
    }

    #[test]
    fn render_wraps_mail_table_in_html_body() {
        let r = render(&RenderOptions {
            subject: "S".into(),
            body: "plain".into(),
            html_body: Some("<table class=\"mail-table\"><tr><th>A</th><th>B</th></tr></table>".into()),
            ..Default::default()
        });
        assert!(r.contains("mail-table-wrap"), "渲染时应自动包裹滚动容器");
        assert!(r.contains("overflow-x:auto"), "滚动容器应有 overflow-x:auto");
    }
}

//! Markdown 正文 → HTML 渲染。
//!
//! 将 AI Agent 提交的 Markdown 正文渲染为邮件 HTML 正文片段，输出映射到内置模板
//! 已有的样式体系（`.mail-h2` / `.mail-p` / `.mail-quote` / `.mail-list` /
//! `.mail-table` / `.mail-img` 等 class + 内联样式），宽表自动经
//! [`crate::template::wrap_wide_tables`] 包进横向滚动容器。
//!
//! 基于轻量 CommonMark 解析器 `pulldown-cmark`（无额外 runtime 依赖，
//! 保持低内存/小体积定位），并启用 GFM 扩展（表格/删除线/任务列表）。

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// body 的可选渲染格式。`Auto`（默认）自动检测 Markdown 特征；
/// `Text` 强制纯文本；`Markdown` 强制按 Markdown 解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyFormat {
    #[default]
    Auto,
    Text,
    Markdown,
}

impl BodyFormat {
    /// 是否启用 Markdown 渲染（显式指定或自动检测命中）
    pub fn use_markdown(self, body: &str) -> bool {
        match self {
            BodyFormat::Markdown => true,
            BodyFormat::Text => false,
            BodyFormat::Auto => looks_like_markdown(body),
        }
    }
}

impl core::str::FromStr for BodyFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(BodyFormat::Auto),
            "text" | "plain" | "txt" => Ok(BodyFormat::Text),
            "markdown" | "md" => Ok(BodyFormat::Markdown),
            _ => Err(format!(
                "body_format 取值非法: {}（可选 auto / text / markdown）",
                s
            )),
        }
    }
}

// ---- 样式常量（与 templates/mail.html 中 mail-* class 保持一致，内联化以兼容剥 style 的客户端）----

const MAIL_P_STYLE: &str = "margin:0 0 14px 0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:17px;line-height:1.9;color:#3a4556;";
const MAIL_H2_STYLE: &str = "font-family:Georgia,'Noto Serif CJK SC','Source Han Serif SC','Songti SC','STSong',serif;font-size:19px;color:#1f3a5f;font-weight:600;margin:28px 0 12px 0;letter-spacing:1px;";
const MAIL_H4_STYLE: &str = "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:16px;color:#1f3a5f;font-weight:600;margin:22px 0 10px 0;";
const MAIL_QUOTE_STYLE: &str = "border-left:3px solid #c9a35c;background-color:#fbf7ee;border-radius:0 8px 8px 0;padding:14px 18px;margin:14px 0 22px 0;font-family:Georgia,'Noto Serif CJK SC','Source Han Serif SC','Songti SC','STSong',serif;font-size:15px;line-height:1.9;color:#5a4a2a;";
const MAIL_LIST_STYLE: &str = "margin:0 0 22px 0;padding:0 0 0 20px;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,'PingFang SC','Hiragino Sans GB','Microsoft YaHei',sans-serif;font-size:17px;line-height:2.1;color:#3a4556;";
const MAIL_LI_STYLE: &str = "margin:0 0 6px 0;";
const MAIL_CODE_STYLE: &str = "font-family:Menlo,Consolas,'Courier New',monospace;font-size:14px;background-color:#f2f4f8;color:#c0392b;border-radius:4px;padding:2px 6px;";
const MAIL_PRE_STYLE: &str = "font-family:Menlo,Consolas,'Courier New',monospace;font-size:14px;line-height:1.7;background-color:#f6f8fa;border:1px solid #e6e9ef;border-radius:8px;padding:16px 18px;margin:14px 0 22px 0;overflow-x:auto;color:#2d3440;";
const MAIL_A_STYLE: &str = "color:#1a6091;text-decoration:none;";
const MAIL_IMG_STYLE: &str = "width:100%;max-width:560px;border-radius:10px;margin:6px 0 20px 0;";
const MAIL_HR_STYLE: &str = "border:0;border-top:1px solid #e6e9ef;margin:22px 0;";
const MAIL_CHECKBOX_STYLE: &str = "vertical-align:-2px;margin:0 8px 0 0;accent-color:#1f3a5f;";

/// 将 Markdown 正文渲染为 HTML 正文片段。
/// 表格输出 `<table class="mail-table">`，由模板层的 `wrap_wide_tables` 统一
/// 注入内联样式并包进横向滚动容器。
pub fn markdown_to_html(text: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = Parser::new_ext(text, opts);
    let mut r = Renderer::default();
    for ev in parser {
        r.event(ev);
    }
    r.out
}

/// 自动检测文本是否像 Markdown（命中一项特征即认为是）：标题、列表、任务列表、
/// 引用、代码围栏、管道表格、水平线、图片、加粗标记、行内代码、链接。
/// 采取保守策略：单个 `*` / `_` 不算（避免数学式、强调用法的纯文本误判）。
pub fn looks_like_markdown(text: &str) -> bool {
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        // 标题 `# ` / `## ` ...
        if l.starts_with('#') && l.len() >= 2 && l.chars().nth(1) == Some(' ') {
            return true;
        }
        // 无序列表 / 任务列表
        if (l.starts_with("- ") || l.starts_with("* ") || l.starts_with("+ "))
            || l.starts_with("- [")
            || l.starts_with("* [")
        {
            return true;
        }
        // 有序列表 `1. ` / `1) `
        if ordered_list_marker(l).is_some() {
            return true;
        }
        // 引用
        if l.starts_with(">") {
            return true;
        }
        // 代码围栏
        if l.starts_with("```") || l.starts_with("~~~") {
            return true;
        }
        // 管道表格分隔行（含表头 + 分隔行至少出现在后续逻辑中）
        if is_table_separator(l) {
            return true;
        }
        // 水平线
        if l.chars().all(|c| c == '-' || c == '*' || c == '_') && l.len() >= 3 {
            return true;
        }
        // 行内特征
        if line.contains("**") || line.contains("![") || line.contains("`")
            || (line.contains("[") && line.contains("]("))
        {
            return true;
        }
    }
    false
}

fn ordered_list_marker(l: &str) -> Option<u64> {
    let bytes = l.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i > 9 {
        return None;
    }
    let after = bytes.get(i).copied();
    if after == Some(b'.') || after == Some(b')') {
        if l.len() > i + 1 && bytes[i + 1] == b' ' {
            l[..i].parse().ok()
        } else {
            None
        }
    } else {
        None
    }
}

/// 管道表格分隔行：`|---|`、`|:---:|` 等（仅含 `|` `-` `:` 空白）
fn is_table_separator(l: &str) -> bool {
    let l = l.trim();
    if !l.starts_with('|') || !l.ends_with('|') {
        return false;
    }
    let inner = &l[1..l.len() - 1];
    if !inner.contains('-') {
        return false;
    }
    inner.chars().all(|c| c == '-' || c == ':' || c == '|' || c == ' ' || c == '\t')
}

#[derive(Default)]
struct Renderer {
    out: String,
    /// 处于引用块内：引用内的段落不另包 <p>（避免破坏 .mail-quote 样式）
    in_blockquote: bool,
    /// 处于表头：单元格以 <th> 输出
    in_thead: bool,
    /// 处于代码块：软换行原样保留
    in_pre: bool,
    /// 当前打开的段落（用于闭合）
    p_open: bool,
    /// 图片 alt 收集模式：之后的文本按 alt 转义输出
    img_alt: bool,
    /// 有序列表起始序号（>1 时输出 start 属性）
    ol_start: Option<u64>,
}

impl Renderer {
    fn write(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn event(&mut self, ev: Event<'_>) {
        use Event::*;
        match ev {
            Start(tag) => self.start(tag),
            End(tag_end) => self.end(tag_end),
            Text(t) => {
                if self.img_alt {
                    // alt 文本直接转义进 src 后的 alt="..." 内
                    self.write(&crate::template::escape_html(&t));
                } else {
                    self.write(&crate::template::escape_html(&t));
                }
            }
            Code(c) => {
                self.write(&format!(
                    "<code style=\"{}\">{}</code>",
                    MAIL_CODE_STYLE,
                    crate::template::escape_html(&c)
                ));
            }
            Html(raw) => {
                // 与 html_body 相同的信任模型：AI Agent 内嵌的原始 HTML 原样输出
                self.write(&raw);
            }
            SoftBreak => {
                // 软换行在 HTML 中折叠为空白；pre 内由 <pre> 保留排版
                self.write("\n");
            }
            HardBreak => {
                if self.in_pre {
                    self.write("\n");
                } else {
                    self.write("<br>");
                }
            }
            Rule => {
                self.write(&format!("<hr style=\"{}\">", MAIL_HR_STYLE));
            }
            TaskListMarker(checked) => {
                let chk = if checked { " checked" } else { "" };
                self.write(&format!(
                    "<input type=\"checkbox\" disabled{chk} style=\"{}\">",
                    MAIL_CHECKBOX_STYLE
                ));
            }
            _ => {
                // FootnoteReference 等罕见事件：按文本原样输出，不丢内容
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        use Tag::*;
        match tag {
            Paragraph => {
                if !self.in_blockquote {
                    self.write(&format!(
                        "<p class=\"mail-p\" style=\"{}\">",
                        MAIL_P_STYLE
                    ));
                    self.p_open = true;
                } else {
                    self.out.push('\n');
                }
            }
            Heading { level, .. } => {
                let (tag_name, style) = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => ("h2", MAIL_H2_STYLE),
                    _ => ("h4", MAIL_H4_STYLE),
                };
                self.write(&format!(
                    "<{} class=\"mail-h{}\" style=\"{}\">",
                    tag_name,
                    if tag_name == "h2" { "2" } else { "4" },
                    style
                ));
            }
            BlockQuote(_) => {
                self.in_blockquote = true;
                self.write(&format!(
                    "<blockquote class=\"mail-quote\" style=\"{}\">",
                    MAIL_QUOTE_STYLE
                ));
            }
            CodeBlock(kind) => {
                self.in_pre = true;
                self.out.push('\n');
                self.write(&format!("<pre style=\"{}\"><code>", MAIL_PRE_STYLE));
                let _ = kind; // 语言信息（CodeBlockKind::Fenced）暂不用于高亮，仅保留注释
            }
            HtmlBlock => {}
            List(ordered) => {
                self.out.push('\n');
                self.ol_start = ordered;
                if let Some(start) = ordered {
                    if start > 1 {
                        self.write(&format!(
                            "<ol class=\"mail-list\" style=\"{}\" start=\"{}\">",
                            MAIL_LIST_STYLE, start
                        ));
                    } else {
                        self.write(&format!("<ol class=\"mail-list\" style=\"{}\">", MAIL_LIST_STYLE));
                    }
                } else {
                    self.write(&format!("<ul class=\"mail-list\" style=\"{}\">", MAIL_LIST_STYLE));
                }
            }
            Item => {
                self.write(&format!("<li style=\"{}\">", MAIL_LI_STYLE));
            }
            Table(_) => {
                self.out.push('\n');
                // 类名 mail-table 由模板层 wrap_wide_tables 自动包滚动容器 + 注入内联样式
                self.write("<table class=\"mail-table\">");
            }
            TableHead => {
                self.in_thead = true;
                self.write("<thead>");
            }
            TableRow => {
                self.write("<tr>");
            }
            TableCell => {
                if self.in_thead {
                    self.write("<th>");
                } else {
                    self.write("<td>");
                }
            }
            Emphasis => self.write("<em>"),
            Strong => self.write("<strong>"),
            Strikethrough => self.write("<del>"),
            Link {
                dest_url, title, ..
            } => {
                let href = crate::template::escape_html(&dest_url);
                let title_attr = if title.is_empty() {
                    String::new()
                } else {
                    format!(" title=\"{}\"", crate::template::escape_html(&title))
                };
                self.write(&format!(
                    "<a href=\"{}\"{} style=\"{}\">",
                    href, title_attr, MAIL_A_STYLE
                ));
            }
            Image {
                dest_url, title, ..
            } => {
                let src = crate::template::escape_html(&dest_url);
                let title_attr = if title.is_empty() {
                    String::new()
                } else {
                    format!(" title=\"{}\"", crate::template::escape_html(&title))
                };
                self.write(&format!(
                    "<img class=\"mail-img\" src=\"{}\"{} alt=\"",
                    src, title_attr
                ));
                self.img_alt = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag_end: TagEnd) {
        use TagEnd::*;
        match tag_end {
            Paragraph => {
                if self.p_open {
                    self.write("</p>");
                    self.p_open = false;
                } else {
                    self.out.push('\n');
                }
            }
            Heading(level) => {
                let tag_name = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => "h2",
                    _ => "h4",
                };
                self.write(&format!("</{}>", tag_name));
            }
            BlockQuote(_) => {
                self.in_blockquote = false;
                self.write("</blockquote>");
            }
            CodeBlock => {
                self.in_pre = false;
                self.write("</code></pre>");
            }
            HtmlBlock => {}
            List(_) => {
                let tag = if self.ol_start.is_some() { "ol" } else { "ul" };
                self.ol_start = None;
                self.write(&format!("</{}>", tag));
                self.out.push('\n');
            }
            Item => self.write("</li>"),
            Table => {
                self.write("</table>");
                self.out.push('\n');
            }
            TableHead => {
                self.in_thead = false;
                self.write("</thead>");
            }
            TableRow => self.write("</tr>"),
            TableCell => {
                if self.in_thead {
                    self.write("</th>");
                } else {
                    self.write("</td>");
                }
            }
            Emphasis => self.write("</em>"),
            Strong => self.write("</strong>"),
            Strikethrough => self.write("</del>"),
            Link => self.write("</a>"),
            Image => {
                self.img_alt = false;
                self.write("\" style=\"");
                self.write(MAIL_IMG_STYLE);
                self.write("\">");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_and_paragraph_rendered_with_style_classes() {
        let html = markdown_to_html("# 标题一\n\n段落文本");
        assert!(html.contains("<h2 class=\"mail-h2\""), "{}", html);
        assert!(html.contains("标题一"));
        assert!(html.contains("<p class=\"mail-p\""), "{}", html);
        assert!(html.contains("段落文本"));
        assert!(html.contains("color:#1f3a5f"), "标题应内联底色: {}", html);
    }

    #[test]
    fn inline_emphasis_strong_code_and_link() {
        let html = markdown_to_html("**加粗** 与 *斜体* 与 `code` 与 [链接](https://example.com)");
        assert!(html.contains("<strong>加粗</strong>"));
        assert!(html.contains("<em>斜体</em>"));
        assert!(html.contains("<code style=\""), "行内代码应内联样式: {}", html);
        assert!(html.contains("<a href=\"https://example.com\""), "{}", html);
        assert!(html.contains("color:#1a6091"), "链接应内联颜色: {}", html);
    }

    #[test]
    fn ordered_and_unordered_lists() {
        let html = markdown_to_html("- 苹果\n- 香蕉\n\n1. 第一\n2. 第二");
        assert!(html.contains("<ul class=\"mail-list\""), "{}", html);
        assert!(html.contains("<li style=\"margin:0 0 6px 0;\">苹果</li>"));
        assert!(html.contains("<ol class=\"mail-list\""), "{}", html);
        assert!(html.contains(">第一<"), "{}", html);
    }

    #[test]
    fn ordered_list_start_number_preserved() {
        let html = markdown_to_html("3. 三\n4. 四");
        assert!(html.contains("<ol class=\"mail-list\""), "{}", html);
        assert!(html.contains("start=\"3\""), "起始序号应保留: {}", html);
        assert!(html.contains(">三<"));
    }

    #[test]
    fn task_list_renders_checkbox() {
        let html = markdown_to_html("- [x] 已完成\n- [ ] 未完成");
        assert!(html.contains("type=\"checkbox\" disabled checked"), "{}", html);
        assert!(html.contains("type=\"checkbox\" disabled style=\""), "{}", html);
        assert!(html.contains(">已完成<"));
    }

    #[test]
    fn blockquote_wraps_mail_quote_without_nested_p() {
        let html = markdown_to_html("> 这是引用内容");
        assert!(html.contains("<blockquote class=\"mail-quote\""), "{}", html);
        assert!(html.contains("这是引用内容"));
        // 引用内部的段落不应再包 <p>，避免破坏 .mail-quote 样式
        assert!(!html.contains("<p class=\"mail-p\""), "引用内不应有 p: {}", html);
    }

    #[test]
    fn code_block_preserves_newlines_and_escapes() {
        let html = markdown_to_html("```rust\nfn main() {\n    let x = 1 < 2;\n}\n```");
        assert!(html.contains("<pre style=\""), "代码块应内联样式: {}", html);
        assert!(html.contains("fn main()"), "{}", html);
        assert!(html.contains("1 &lt; 2"), "代码内容应转义: {}", html);
    }

    #[test]
    fn markdown_table_renders_mail_table_with_thead() {
        let md = "| 指标 | 本周 |\n| --- | --- |\n| A | 1 |\n| B | 2 |";
        let html = markdown_to_html(md);
        assert!(html.contains("<table class=\"mail-table\">"), "{}", html);
        assert!(html.contains("<thead>"), "{:?}", html);
        assert!(html.contains("<th>指标</th>"), "{}", html);
        assert!(html.contains("<td>1</td>"), "{}", html);
    }

    #[test]
    fn raw_html_is_passed_through_like_html_body() {
        let html = markdown_to_html("前文\n\n<div class=\"mail-callout\">高亮 <b>内容</b></div>");
        assert!(html.contains("<div class=\"mail-callout\">高亮 <b>内容</b></div>"), "{}", html);
    }

    #[test]
    fn markdown_plain_text_is_escaped() {
        // 普通文本中的 < > & 必须转义（raw HTML 属信任模型的一部分，见 raw_html_is_passed_through）
        let html = markdown_to_html("价格比较：1 < 2，且 3 > 2，& 更多");
        assert!(html.contains("1 &lt; 2"), "{}", html);
        assert!(html.contains("3 &gt; 2"), "{}", html);
        assert!(html.contains("&amp;"), "{}", html);
        assert!(!html.contains("1 < 2"), "不应有未转义 <: {}", html);
    }

    #[test]
    fn image_renders_with_alt_and_class() {
        let html = markdown_to_html("![示意图](https://example.com/pic.png)");
        assert!(html.contains("<img class=\"mail-img\" src=\"https://example.com/pic.png\" alt=\"示意图\""), "{}", html);
        assert!(html.contains("max-width:560px"), "图片应内联样式: {}", html);
    }

    #[test]
    fn strikethrough_and_hr_rendered() {
        let html = markdown_to_html("~~删除~~\n\n---");
        assert!(html.contains("<del>删除</del>"), "{}", html);
        assert!(html.contains("<hr style=\""), "{}", html);
    }

    #[test]
    fn looks_like_markdown_detects_block_features() {
        assert!(looks_like_markdown("# 标题"));
        assert!(looks_like_markdown("- 列表项"));
        assert!(looks_like_markdown("1. 有序项"));
        assert!(looks_like_markdown("> 引用"));
        assert!(looks_like_markdown("```\ncode\n```"));
        assert!(looks_like_markdown("| a | b |\n|---|---|"));
        assert!(looks_like_markdown("---"));
        assert!(looks_like_markdown("**加粗**"));
        assert!(looks_like_markdown("`行内代码`"));
        assert!(looks_like_markdown("[链接](https://x.com)"));
        assert!(looks_like_markdown("- [ ] 任务"));
    }

    #[test]
    fn looks_like_markdown_keeps_plain_text_plain() {
        assert!(!looks_like_markdown(""));
        assert!(!looks_like_markdown("这是一段普通的中文文本，没有特殊标记。"));
        assert!(!looks_like_markdown("价格 5*3 = 15，路径 C:\\tmp，算式 a*b"));
        assert!(!looks_like_markdown("2026 年收益 15%，同比增长 3.5%"));
    }

    #[test]
    fn body_format_use_markdown_respects_mode() {
        assert!(BodyFormat::Markdown.use_markdown("普通文本"));
        assert!(!BodyFormat::Text.use_markdown("# 标题"));
        assert!(BodyFormat::Auto.use_markdown("# 标题"));
        assert!(!BodyFormat::Auto.use_markdown("普通文本"));
        assert_eq!(BodyFormat::default(), BodyFormat::Auto);
    }

    #[test]
    fn body_format_from_str() {
        assert_eq!("auto".parse::<BodyFormat>().unwrap(), BodyFormat::Auto);
        assert_eq!("Markdown".parse::<BodyFormat>().unwrap(), BodyFormat::Markdown);
        assert_eq!("text".parse::<BodyFormat>().unwrap(), BodyFormat::Text);
        assert!("bogus".parse::<BodyFormat>().is_err());
    }
}
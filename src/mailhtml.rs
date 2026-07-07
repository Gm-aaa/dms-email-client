//! 邮件正文 → 可读文本 / 可渲染 HTML。
//!
//! 与守护进程生命周期无关，纯内容处理：从已解析邮件里提取可读正文（纯文本优先、
//! 占位符时回退 HTML），以及把纯文本正文转成前端 `TextEdit(RichText)` 能直接渲染的
//! HTML（URL/验证码可点击）。URL 与验证码的切分复用 [`crate::segment`]。

use crate::segment::{segment, Segment};
use std::sync::OnceLock;

/// 从已解析邮件中提取可读正文：优先纯文本，但当纯文本缺失或只是「请看 HTML 版」
/// 之类占位符（很多营销邮件如此，真正内容全在 text/html）时，回退到 HTML 提取。
pub fn extract_body(msg: &mail_parser::Message) -> String {
    let text = msg.body_text(0);
    let html = msg.body_html(0);

    if let Some(t) = &text {
        let s = t.trim();
        // 纯文本可用，且不是占位符（或压根没有 HTML 备选）→ 直接用纯文本
        if !s.is_empty() && !(html.is_some() && is_placeholder_text(s)) {
            return s.to_string();
        }
    }

    // 回退：从 HTML 提取可读文本
    if let Some(h) = &html {
        let txt = html_to_text(h);
        if !txt.trim().is_empty() {
            return txt;
        }
    }

    // 兜底：即便是占位符，也好过完全空白
    text.map(|t| t.trim().to_string()).unwrap_or_default()
}

/// 解码常见 HTML 实体（命名 + 十进制/十六进制数字引用）。未知实体原样保留。
fn decode_entities(s: &str) -> String {
    static ENT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = ENT_RE.get_or_init(|| {
        regex::Regex::new(r"&(#[xX][0-9A-Fa-f]+|#[0-9]+|[A-Za-z][A-Za-z0-9]*);").unwrap()
    });
    re.replace_all(s, |c: &regex::Captures| {
        let e = &c[1];
        if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
            return u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| c[0].to_string());
        }
        if let Some(dec) = e.strip_prefix('#') {
            return dec
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| c[0].to_string());
        }
        let rep = match e {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "nbsp" => " ",
            "mdash" => "—",
            "ndash" => "–",
            "hellip" => "…",
            "lsquo" | "rsquo" => "'",
            "ldquo" | "rdquo" => "\"",
            "trade" => "™",
            "reg" => "®",
            "copy" => "©",
            // 零宽/软连字符：营销邮件常用大量 &zwnj;&nbsp; 撑预览，直接丢弃
            "zwnj" | "zwj" | "shy" => "",
            _ => return c[0].to_string(), // 未知实体保持原样
        };
        rep.to_string()
    })
    .into_owned()
}

/// 极简 HTML → 纯文本：先剥掉不可见块（style/script/head/注释），再把块级标签转
/// 换行、去掉其余标签、解码实体、折叠空白。用于纯文本缺失/为占位符时的回退。
fn html_to_text(html: &str) -> String {
    // 1. 移除 <style>/<script>/<head> 整块（含内容）与 HTML 注释——否则 CSS/脚本
    //    文本会被当成正文吐出来
    static BLOCK_RE: OnceLock<regex::Regex> = OnceLock::new();
    let block_re = BLOCK_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?is)<(style|script|head)\b[^>]*>.*?</\s*(style|script|head)\s*>|<!--.*?-->",
        )
        .unwrap()
    });
    let mut s = block_re.replace_all(html, " ").into_owned();

    // 2. 块级标签转换行，保留段落结构
    for tag in [
        "<br>", "<br/>", "<br />", "</p>", "</div>", "</tr>", "</li>", "</h1>", "</h2>", "</h3>",
        "</h4>",
    ] {
        s = s.replace(tag, "\n");
    }

    // 3. 去掉其余标签
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    // 4. 解码 HTML 实体（此时标签已去除，&lt; 之类不会被误判为标签），并滤掉
    //    零宽/BOM/软连字符等不可见字符（源码里直接出现的那种）
    let out: String = decode_entities(&out)
        .chars()
        .filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}'))
        .collect();

    // 5. 折叠多余空行/空白，但保留换行结构
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断一段 text/plain 是否只是「请看 HTML 版」之类占位符，没有实际内容。
/// 命中则说明真正正文在 HTML 部分，应回退到 HTML 提取。
fn is_placeholder_text(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return true;
    }
    let l = t.to_lowercase();
    // 明确的占位符短语（足够具体，无需长度限制）
    if l.contains("plain text version not available")
        || l.contains("plain text version is not available")
    {
        return true;
    }
    // 「请用/启用 HTML 客户端查看」这类——仅当整段很短（没有其它实质内容）才算占位符，
    // 否则一封正常正文里顺带提到 "view ... in HTML" 会被误判
    t.chars().count() < 120
        && l.contains("html")
        && (l.contains("view")
            || l.contains("requires")
            || l.contains("enable")
            || l.contains("capable"))
}

/// 转义 HTML 特殊字符
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 渲染一段普通散文：转义 HTML，换行转 `<br>`。（验证码已由 [`crate::segment`] 单独切出，
/// 不会出现在这里。）
fn render_text(seg: &str) -> String {
    html_escape(seg).replace('\n', "<br>")
}

/// 渲染验证码：点击复制（`href="copy:..."`）。
fn render_code(code: &str) -> String {
    format!("<a href=\"copy:{code}\">{code} ⧉</a>")
}

/// 渲染链接：「🔗 域名」(点击打开) +「⧉」(复制完整链接)。
fn render_url(u: &str) -> String {
    // 可读域名：去掉协议头与路径
    let dom = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let dom = dom.split('/').next().unwrap_or(dom);
    // href 属性里需转义 & 和 "
    let attr = u.replace('&', "&amp;").replace('"', "&quot;");
    format!(
        "<a href=\"{attr}\">🔗 {dom}</a><a href=\"copy:{attr}\"> ⧉</a>",
        attr = attr,
        dom = html_escape(dom)
    )
}

/// 把纯文本正文转成可渲染的 HTML：URL → 可点击「🔗 域名 + ⧉ 复制」，4–8 位验证码 →
/// 可点击复制，其余散文转义后按 `<br>` 断行。前端 `TextEdit(RichText)` 直接显示。
pub fn body_to_html(plain: &str) -> String {
    let mut out = String::new();
    for seg in segment(plain) {
        match seg {
            Segment::Text(t) => out.push_str(&render_text(t)),
            Segment::Url(u) => out.push_str(&render_url(u)),
            Segment::Code(c) => out.push_str(&render_code(c)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
        mail_parser::MessageParser::default().parse(raw).unwrap()
    }

    /// 复现 GOG「无法渲染」邮件：text/plain 是占位符，真内容在 text/html。
    /// 修复后应回退到 HTML 提取，且剥掉 style、解码实体。
    #[test]
    fn placeholder_plain_falls_back_to_html() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nPlain text version not available\r\n\
--b\r\nContent-Type: text/html\r\n\r\n\
<html><head><style>.x{color:red}</style></head><body><p>Real&nbsp;deal&mdash;here</p></body></html>\r\n\
--b--\r\n";
        let body = extract_body(&parse(raw));
        assert!(body.contains("Real deal—here"), "got: {:?}", body);
        assert!(!body.contains("not available"), "placeholder leaked: {:?}", body);
        assert!(!body.to_lowercase().contains("color:red"), "css leaked: {:?}", body);
    }

    /// 正常的实质纯文本正文必须原样保留（不因含 "html"/"view" 等词被误判为占位符）。
    #[test]
    fn substantive_plain_text_is_kept() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\n\
You can view this report online in HTML, but here is the full plain summary with all the numbers and details you asked for so nothing is missing at all.\r\n\
--b\r\nContent-Type: text/html\r\n\r\n<p>short</p>\r\n--b--\r\n";
        let body = extract_body(&parse(raw));
        assert!(body.contains("full plain summary"), "substantive text dropped: {:?}", body);
    }

    #[test]
    fn html_to_text_strips_style_and_decodes_entities() {
        let t = html_to_text("<head><style>.a{x:1}</style></head><body>Hi&nbsp;there &amp; more&zwnj;&zwnj;</body>");
        assert!(!t.contains("x:1"), "style leaked: {:?}", t);
        assert!(t.contains("Hi there & more"), "got: {:?}", t);
    }

    /// body_to_html：URL 渲染成可点击域名、验证码渲染成复制链接、其余文本转义。
    #[test]
    fn body_to_html_renders_url_code_and_escapes() {
        let html = body_to_html("a & b, code 482913, see https://ex.com/p?x=1&y=2");
        assert!(html.contains("a &amp; b"), "text not escaped: {html}");
        assert!(html.contains("copy:482913"), "code not linkified: {html}");
        assert!(html.contains("🔗 ex.com"), "url domain missing: {html}");
        assert!(html.contains("x=1&amp;y=2"), "url attr not escaped: {html}");
    }
}

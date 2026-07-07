//! 正文切分：把纯文本切成有序的 [散文 | URL | 验证码] 片段。
//!
//! daemon 的正文渲染（`mailhtml::body_to_html`，切完渲染成 HTML）与翻译
//! （`translate::translate_prose`，切完批量翻译）共用同一套规则。以前两处各写一份
//! URL / “4–8 位验证码” 正则、只能靠注释提醒“必须一致”，现在统一由本模块产出片段。

use regex::Regex;
use std::sync::OnceLock;

/// 正文片段（借用原文切片，零拷贝）。按在原文中的出现顺序返回：
/// - `Text`：普通散文（可能仍含**非** 4–8 位的数字，那些不算验证码）
/// - `Url`：完整链接（去掉可能包裹的尖括号 `<...>`）
/// - `Code`：4–8 位独立数字串（验证码）
#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Url(&'a str),
    Code(&'a str),
}

/// URL 匹配：允许被 `<...>` 包裹（邮件里常见），捕获组 1 为纯链接本体。
fn url_re() -> &'static Regex {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    URL_RE.get_or_init(|| Regex::new(r#"<?(https?://[^\s<>"']+)>?"#).unwrap())
}

/// 连续数字串；长度在 4..=8 时视为验证码。
fn code_re() -> &'static Regex {
    static DIGIT_RE: OnceLock<Regex> = OnceLock::new();
    DIGIT_RE.get_or_init(|| Regex::new(r"[0-9]+").unwrap())
}

/// 把 `input` 切成有序片段：先按 URL 分割，URL 之间的普通文本再挖出 4–8 位数字串作为
/// `Code`，其余为 `Text`。空片段不产出。URL 与验证码在此被隔离，绝不会混进散文。
pub fn segment(input: &str) -> Vec<Segment<'_>> {
    let mut segs = Vec::new();
    let mut last = 0;
    for caps in url_re().captures_iter(input) {
        let whole = caps.get(0).unwrap();
        let url = caps.get(1).unwrap();
        push_prose(&input[last..whole.start()], &mut segs);
        segs.push(Segment::Url(url.as_str()));
        last = whole.end();
    }
    push_prose(&input[last..], &mut segs);
    segs
}

/// 把一段不含 URL 的文本切成 `Code` / `Text` 片段。
fn push_prose<'a>(text: &'a str, segs: &mut Vec<Segment<'a>>) {
    let mut last = 0;
    for m in code_re().find_iter(text) {
        if (4..=8).contains(&(m.end() - m.start())) {
            push_text(&text[last..m.start()], segs);
            segs.push(Segment::Code(m.as_str()));
            last = m.end();
        }
    }
    push_text(&text[last..], segs);
}

/// 非空文本才作为 `Text` 片段加入。
fn push_text<'a>(text: &'a str, segs: &mut Vec<Segment<'a>>) {
    if !text.is_empty() {
        segs.push(Segment::Text(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_url_code_and_prose_in_order() {
        let got = segment("Click https://gog.com/deal now, code 482913 ok");
        assert_eq!(
            got,
            vec![
                Segment::Text("Click "),
                Segment::Url("https://gog.com/deal"),
                Segment::Text(" now, code "),
                Segment::Code("482913"),
                Segment::Text(" ok"),
            ]
        );
    }

    #[test]
    fn angle_wrapped_url_body_only() {
        assert_eq!(segment("<https://a.com/x>"), vec![Segment::Url("https://a.com/x")]);
    }

    #[test]
    fn adjacent_codes_keep_separator_text() {
        // 两个相邻验证码之间的空白作为独立 Text 片段保留，不会被并进任何 Code。
        assert_eq!(
            segment("482913 123456"),
            vec![Segment::Code("482913"), Segment::Text(" "), Segment::Code("123456")]
        );
    }

    #[test]
    fn non_4_8_digit_runs_stay_prose() {
        // 3 位和 9 位数字串都不是验证码，留在散文里。
        assert_eq!(segment("id 123 num 123456789"), vec![Segment::Text("id 123 num 123456789")]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert_eq!(segment(""), Vec::<Segment>::new());
    }
}

//! 离线翻译：CTranslate2 (ct2rs) 跑 NLLB-200-distilled-600M。
//!
//! ct2rs 0.9 vendors its own CTranslate2 C++ source and builds it via cmake at
//! `cargo build` time (no system libctranslate2/pkg-config needed).
//!
//! NLLB 的 HuggingFace `tokenizer.json` 是"静态"文件：Python 侧
//! `NllbTokenizerFast` 会在运行时动态改写 post-processor 把源语言 token
//! 追加到序列末尾（`X </s> src_lang`），但序列化到磁盘的 `tokenizer.json`
//! 里这段模板是通用占位符（`<unk>`），并不包含真正的语言码。因此这里禁用
//! ct2rs 自带的自动 special-token 后处理（`disable_spacial_token`），改为
//! 手动在源文本末尾拼接字面量 `</s>` 和源语言 token —— 这两个 token 都在
//! 词表的 added-tokens 中注册为 special，HuggingFace `tokenizers` 库在做
//! pre-tokenization 时始终会按 added-vocab 词表切分它们（与
//! add_special_tokens 开关无关），所以字面量文本能被正确切成对应的 token id。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ct2rs::tokenizers::hf::Tokenizer as HfTokenizer;
use ct2rs::{Config, TranslationOptions, Translator as Ct2Translator};

/// 本地 NLLB 翻译器。持有已加载的 CTranslate2 模型 + 分词器。
pub struct NllbLocal {
    translator: Ct2Translator<HfTokenizer>,
}

impl NllbLocal {
    /// 从模型目录加载（目录内含 CT2 模型 + tokenizer.json / sentencepiece.bpe.model）。
    pub fn load(model_dir: &Path) -> Result<NllbLocal, String> {
        let mut tokenizer = HfTokenizer::new(model_dir)
            .map_err(|e| format!("加载分词器失败: {e}"))?;
        // 关闭自动 special-token 后处理，改由 translate_one 手动拼接
        // "</s> <src_lang>" 后缀，见上方模块说明。
        tokenizer.disable_spacial_token();

        let translator = Ct2Translator::with_tokenizer(model_dir, tokenizer, &Config::default())
            .map_err(|e| format!("加载 NLLB 模型失败: {e}"))?;
        Ok(NllbLocal { translator })
    }

    /// 翻译一段文本。src/tgt 为 NLLB(FLORES-200) 语言码，如 eng_Latn / zho_Hans。
    /// NLLB 约定：源句末尾追加 "</s> 源语言token"，target_prefix 为目标语言 token。
    pub fn translate_one(&self, text: &str, src: &str, tgt: &str) -> Result<String, String> {
        // 手动构造 NLLB 期望的源序列: "<text> </s> <src_lang>"
        let source = format!("{text} </s> {src}");
        let target_prefixes = vec![vec![tgt.to_string()]];

        let results = self
            .translator
            .translate_batch_with_target_prefix(
                &[source],
                &target_prefixes,
                &TranslationOptions::default(),
                None,
            )
            .map_err(|e| format!("翻译失败: {e}"))?;

        Ok(results.into_iter().next().map(|(text, _score)| text).unwrap_or_default())
    }
}

/// 翻译后端抽象（薄）。仅为可测试性与接缝，本期只有 NllbLocal 一个实现。
pub trait Translator {
    fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String>;
}

impl Translator for NllbLocal {
    fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String> {
        self.translate_one(text, src, tgt)
    }
}

/// NLLB-200-distilled-600M 对单次输入长度有硬上限；超长的整段散文一次性送进
/// t.translate() 会被模型静默截断（只翻出前一部分）。实测约 741 字符的一段
/// 完全没问题，所以这里保守起见只在明显超长时才分批：按行边界切分并贪心
/// 合并相邻行到批次（每批字符数 <= MAX_TRANSLATE_CHARS），逐批翻译后用 '\n'
/// 拼回——单行本身超过阈值时不再往下拆（罕见边界情况，宁可不拆也不能丢内容）。
const MAX_TRANSLATE_CHARS: usize = 1200;

fn translate_chunked(t: &dyn Translator, text: &str, src: &str, tgt: &str) -> Result<String, String> {
    if text.chars().count() <= MAX_TRANSLATE_CHARS {
        return t.translate(text, src, tgt);
    }

    let mut batches: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.split('\n') {
        let extra = if cur.is_empty() { line.chars().count() } else { line.chars().count() + 1 };
        if !cur.is_empty() && cur.chars().count() + extra > MAX_TRANSLATE_CHARS {
            batches.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        batches.push(cur);
    }

    let mut translated_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        translated_batches.push(t.translate(&batch, src, tgt)?);
    }
    Ok(translated_batches.join("\n"))
}

/// 翻译一段散文文本，同时把其中 4–8 位的独立数字串（验证码）挖出来保持原样，
/// 不送进 translate()——与 daemon.rs 里 process_text_segment 用的是同一条
/// `[0-9]+` 正则 + 4..=8 长度判断，两处规则必须一致。
fn translate_text_preserving_codes(
    t: &dyn Translator,
    text: &str,
    src: &str,
    tgt: &str,
) -> Result<String, String> {
    static DIGIT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let digit_re = DIGIT_RE.get_or_init(|| regex::Regex::new(r"[0-9]+").unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in digit_re.find_iter(text) {
        let len = m.end() - m.start();
        if (4..=8).contains(&len) {
            let chunk = &text[last..m.start()];
            if !chunk.trim().is_empty() {
                out.push_str(&translate_chunked(t, chunk, src, tgt)?);
            } else {
                out.push_str(chunk); // 纯空白间隔：原样保留，不送翻译器（否则可能把两个相邻验证码拼到一起）
            }
            out.push_str(m.as_str()); // 验证码原样保留，不送翻译器
            last = m.end();
        }
    }
    let tail = &text[last..];
    if !tail.trim().is_empty() {
        out.push_str(&translate_chunked(t, tail, src, tgt)?);
    } else {
        out.push_str(tail);
    }
    Ok(out)
}

/// 把纯文本正文翻译成目标语言：用 URL 正则把文本切成【散文段 | URL 段】，只翻散文，
/// URL 原样保留；每个散文段内部再由 translate_text_preserving_codes 挖出 4–8 位
/// 验证码保持原样——不再依赖"模型碰巧不改数字"的假设，而是代码层面强制隔离。
/// 返回**纯文本**，由调用方 body_to_html。
pub fn translate_prose(
    t: &dyn Translator,
    plain: &str,
    src: &str,
    tgt: &str,
) -> Result<String, String> {
    static URL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let url_re =
        URL_RE.get_or_init(|| regex::Regex::new(r#"<?(https?://[^\s<>"']+)>?"#).unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in url_re.find_iter(plain) {
        let prose = &plain[last..m.start()];
        if !prose.trim().is_empty() {
            out.push_str(&translate_text_preserving_codes(t, prose, src, tgt)?);
        } else {
            out.push_str(prose);
        }
        out.push_str(m.as_str()); // URL 原样
        last = m.end();
    }
    let tail = &plain[last..];
    if !tail.trim().is_empty() {
        out.push_str(&translate_text_preserving_codes(t, tail, src, tgt)?);
    } else {
        out.push_str(tail);
    }
    Ok(out)
}

/// 把「auto 或显式语言码」解析为 NLLB(FLORES-200) 源语言码。
/// auto 时用 whatlang 检测；不可靠或未映射则回退 eng_Latn。
pub fn resolve_source_lang(requested: &str, text: &str) -> String {
    if requested != "auto" {
        return requested.to_string();
    }
    use whatlang::Lang;
    let info = match whatlang::detect(text) {
        Some(i) if i.is_reliable() => i,
        _ => return "eng_Latn".to_string(),
    };
    let code = match info.lang() {
        Lang::Eng => "eng_Latn",
        Lang::Cmn => "zho_Hans",
        Lang::Rus => "rus_Cyrl",
        Lang::Jpn => "jpn_Jpan",
        Lang::Kor => "kor_Hang",
        Lang::Fra => "fra_Latn",
        Lang::Deu => "deu_Latn",
        Lang::Spa => "spa_Latn",
        Lang::Por => "por_Latn",
        Lang::Ita => "ita_Latn",
        _ => "eng_Latn",
    };
    code.to_string()
}

/// 译文缓存键：(account, folder, uid, src, tgt)。五元组而非仅 uid，
/// 是为了让切换源/目标语言后不会命中旧翻译（陈旧结果）。
pub type TransKey = (String, String, u32, String, String);

/// 译文内存缓存：容量上限 + FIFO 淘汰。不落盘。
pub struct TransCache {
    inner: Mutex<(HashMap<TransKey, String>, VecDeque<TransKey>)>,
    cap: usize,
}

impl TransCache {
    pub fn new(cap: usize) -> TransCache {
        TransCache {
            inner: Mutex::new((HashMap::new(), VecDeque::new())),
            cap: cap.max(1),
        }
    }
    pub fn get(&self, key: &TransKey) -> Option<String> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.0.get(key).cloned()
    }
    pub fn put(&self, key: TransKey, html: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.0.insert(key.clone(), html).is_none() {
            g.1.push_back(key);
        }
        while g.1.len() > self.cap {
            if let Some(old) = g.1.pop_front() {
                g.0.remove(&old);
            }
        }
    }
}

/// 模型的持久化存放目录：`$XDG_DATA_HOME/dms-email-client/models/nllb-200-distilled-600M`
/// （无 XDG 时退化到当前目录下同名相对路径）。
pub fn model_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dms-email-client/models/nllb-200-distilled-600M")
}

/// ensure_model 需要落地的全部文件：CT2 模型本体 + HuggingFace 分词器全套。
/// NllbLocal::load 用 ct2rs 的 HfTokenizer 读 tokenizer.json，所以分词器相关
/// 文件（tokenizer.json / tokenizer_config.json / special_tokens_map.json /
/// sentencepiece.bpe.model）缺一不可，不能只下 CT2 模型文件。
const MODEL_FILES: [&str; 7] = [
    "config.json",
    "model.bin",
    "sentencepiece.bpe.model",
    "shared_vocabulary.txt",
    "special_tokens_map.json",
    "tokenizer_config.json",
    "tokenizer.json",
];

/// 确保模型就绪：缺失则从 HuggingFace 下载社区预转换好的 CT2 (int8) 版模型 +
/// 配套分词器文件到 model_dir()，避免运行时依赖 Python 转换脚本。
/// 若 model.bin 与 tokenizer.json 均已存在，视为已就绪，直接跳过下载。
pub fn ensure_model() -> Result<PathBuf, String> {
    let dir = model_dir();
    let model_bin = dir.join("model.bin");
    let tokenizer_json = dir.join("tokenizer.json");
    if model_bin.exists() && tokenizer_json.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    // 社区预转换好的 CT2 int8 仓库（已验证可用，含分词器全套文件）。
    let repo = "JustFrederik/nllb-200-distilled-600M-ct2-int8";
    let api = hf_hub::api::sync::Api::new().map_err(|e| format!("hf-hub 初始化失败: {e}"))?;
    let r = api.model(repo.to_string());
    for f in MODEL_FILES {
        let got = r.get(f).map_err(|e| format!("下载 {f} 失败: {e}"))?;
        std::fs::copy(&got, dir.join(f)).map_err(|e| format!("落盘 {f} 失败: {e}"))?;
    }
    Ok(dir)
}

/// 管理 NLLB 模型的懒加载与空闲卸载，保持 daemon 常驻内存精简：
/// 首次使用时才 ensure_model()+load()，空闲超过 5 分钟由后台线程卸载。
pub struct ModelManager {
    // (已加载的翻译器, 上次使用时刻)
    slot: Mutex<(Option<NllbLocal>, Instant)>,
}

impl ModelManager {
    pub fn new() -> ModelManager {
        ModelManager {
            slot: Mutex::new((None, Instant::now())),
        }
    }

    /// 取用翻译器执行 f；模型未加载则先 ensure_model()+load。执行后更新 last-used。
    pub fn with_translator<R>(&self, f: impl FnOnce(&NllbLocal) -> R) -> Result<R, String> {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.0.is_none() {
            let dir = ensure_model()?;
            g.0 = Some(NllbLocal::load(&dir)?);
        }
        g.1 = Instant::now();
        let t = g.0.as_ref().unwrap();
        Ok(f(t))
    }

    /// 后台线程：每 60 秒检查一次，空闲超过 5 分钟则卸载模型并把内存归还 OS。
    pub fn start_idle_unloader(self: &Arc<ModelManager>) {
        let me = Arc::clone(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            let mut g = me.slot.lock().unwrap_or_else(|e| e.into_inner());
            if g.0.is_some() && g.1.elapsed() > Duration::from_secs(5 * 60) {
                g.0 = None; // Drop 卸载模型
                drop(g);
                crate::daemon::release_free_memory();
                println!("[translate] 空闲卸载 NLLB 模型，归还内存");
            }
        });
    }
}

impl Default for ModelManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod spike_tests {
    use super::*;

    /// 需要真实模型，默认忽略；手动运行：
    /// cargo test --release spike_translate -- --ignored --nocapture
    #[test]
    #[ignore]
    fn spike_translate() {
        let dir = dirs::data_dir()
            .unwrap()
            .join("dms-email-client/models/nllb-200-distilled-600M");
        let t = NllbLocal::load(&dir).expect("load model");
        let out = t.translate_one("Hello, world.", "eng_Latn", "zho_Hans").unwrap();
        println!("translated = {out:?}");
        assert!(!out.trim().is_empty(), "empty translation");
        assert!(out.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "no Chinese chars: {out:?}");
    }

    /// ModelManager 端到端：ensure_model 走跳过下载路径（模型已在本机就绪）+
    /// lazy load + 实际翻译一句。默认忽略，手动运行：
    /// cargo test --release model_manager_lazy_load -- --ignored --nocapture
    #[test]
    #[ignore]
    fn model_manager_lazy_load() {
        let mm = ModelManager::new();
        let out = mm
            .with_translator(|t| t.translate_one("Hello, world.", "eng_Latn", "zho_Hans"))
            .expect("with_translator")
            .expect("translate_one");
        println!("translated = {out:?}");
        assert!(out.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "no Chinese chars: {out:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假翻译器：把每个散文段包上「<译>...</译>」，便于断言哪些段被翻译。
    struct MockTranslator;
    impl Translator for MockTranslator {
        fn translate(&self, text: &str, _src: &str, _tgt: &str) -> Result<String, String> {
            Ok(format!("<译>{text}</译>"))
        }
    }

    #[test]
    fn keeps_urls_and_codes_untranslated() {
        let plain = "Click https://gog.com/deal now. Code 482913 expires.";
        let out = translate_prose(&MockTranslator, plain, "eng_Latn", "zho_Hans").unwrap();
        // URL 与验证码原样保留
        assert!(out.contains("https://gog.com/deal"), "url mangled: {out}");
        assert!(out.contains("482913"), "code mangled: {out}");
        // 散文被翻译（出现译标记）
        assert!(out.contains("<译>"), "prose not translated: {out}");
        // URL 不在译标记内部
        assert!(!out.contains("<译>https"), "url got translated: {out}");
    }

    #[test]
    fn empty_and_url_only() {
        assert_eq!(translate_prose(&MockTranslator, "", "eng_Latn", "zho_Hans").unwrap(), "");
        let only = translate_prose(&MockTranslator, "https://a.com/x", "eng_Latn", "zho_Hans").unwrap();
        assert_eq!(only, "https://a.com/x");
    }

    /// 假翻译器：把文本里每个 ASCII 数字都改写成 '9'，用来证明验证码若被送进
    /// translate() 就会被改写——从而验证 translate_prose 是否真正把验证码排除在外
    /// （而不是依赖"模型碰巧不改数字"的假设）。
    struct DigitMangler;
    impl Translator for DigitMangler {
        fn translate(&self, text: &str, _src: &str, _tgt: &str) -> Result<String, String> {
            Ok(text
                .chars()
                .map(|c| if c.is_ascii_digit() { '9' } else { c })
                .collect())
        }
    }

    #[test]
    fn digit_mangling_translator_still_preserves_codes() {
        let plain = "Your code 482913 expires soon.";
        let out = translate_prose(&DigitMangler, plain, "eng_Latn", "zho_Hans").unwrap();
        assert!(out.contains("482913"), "4-8 digit code was sent to translator and mangled: {out}");
    }

    /// Wraps DigitMangler but records whether translate() was ever called with a
    /// whitespace-only chunk. DigitMangler itself is content-preserving on non-digit
    /// text (identity on whitespace), so a plain output-content assertion can't tell
    /// apart "translated then returned unchanged" from "never sent, pushed verbatim"
    /// — this tracker makes the routing bug (fix #1) observable in a unit test, the
    /// same way a real lossy translator could merge/garble that gap in production.
    struct DigitManglerTrackWhitespaceCalls {
        whitespace_calls: std::cell::Cell<u32>,
    }
    impl Translator for DigitManglerTrackWhitespaceCalls {
        fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String> {
            if text.trim().is_empty() {
                self.whitespace_calls.set(self.whitespace_calls.get() + 1);
            }
            DigitMangler.translate(text, src, tgt)
        }
    }

    /// Fix #1: whitespace-only gap between two adjacent 4-8 digit codes must NOT be
    /// sent to the (possibly lossy) translator — otherwise a mangling/empty-returning
    /// translator can merge the two codes together (e.g. "482913123456") or inject a
    /// stray glyph in the separator. The gap must be preserved verbatim.
    #[test]
    fn two_adjacent_codes_stay_separated() {
        let plain = "Codes 482913 123456 end.";
        let tracker = DigitManglerTrackWhitespaceCalls { whitespace_calls: std::cell::Cell::new(0) };
        let out = translate_prose(&tracker, plain, "eng_Latn", "zho_Hans").unwrap();
        assert!(out.contains("482913"), "first code mangled/lost: {out}");
        assert!(out.contains("123456"), "second code mangled/lost: {out}");
        assert!(
            out.contains("482913 123456"),
            "codes got merged or separator corrupted: {out}"
        );
        assert_eq!(
            tracker.whitespace_calls.get(),
            0,
            "whitespace-only inter-code gap must not be routed through translate()"
        );
    }

    /// Fix #2: NLLB has a bounded input length; a very long prose block sent as one
    /// translate() call would get silently truncated by the model. translate_chunked
    /// must split long text on line boundaries into batches and rejoin them, so both
    /// the first and last line survive translation intact.
    #[test]
    fn long_prose_is_fully_translated_not_truncated() {
        let mut long = String::new();
        for i in 1..=60 {
            long.push_str(&format!("Line {i}: some sentence about email translation testing.\n"));
        }
        assert!(
            long.chars().count() > MAX_TRANSLATE_CHARS,
            "test fixture not long enough: {} chars",
            long.chars().count()
        );

        let out = translate_chunked(&MockTranslator, &long, "eng_Latn", "zho_Hans").unwrap();
        assert!(out.contains("Line 1:"), "first line missing from output (truncated?): {out}");
        assert!(out.contains("Line 60:"), "last line missing from output (truncated?): {out}");
        assert!(out.contains("<译>"), "content not routed through translator: {out}");

        // Short text stays a single batch: exactly one translate() call, whole text
        // wrapped once (not split per line).
        let short = "Hello, this is a short sentence.";
        let short_out = translate_chunked(&MockTranslator, short, "eng_Latn", "zho_Hans").unwrap();
        assert_eq!(
            short_out,
            format!("<译>{short}</译>"),
            "short text should go through as a single translate call: {short_out}"
        );
    }

    #[test]
    fn explicit_source_is_passthrough() {
        assert_eq!(resolve_source_lang("rus_Cyrl", "irrelevant"), "rus_Cyrl");
    }

    #[test]
    fn auto_detects_common_langs() {
        assert_eq!(resolve_source_lang("auto", "This is clearly an English sentence about email."), "eng_Latn");
        assert_eq!(resolve_source_lang("auto", "Это предложение на русском языке для проверки."), "rus_Cyrl");
        // 无法可靠判断时回退英语
        assert_eq!(resolve_source_lang("auto", "12345 !!! ???"), "eng_Latn");
    }

    #[test]
    fn cache_hit_miss_and_eviction() {
        let c = TransCache::new(2);
        let k1: TransKey = ("a".into(), "INBOX".into(), 1, "eng_Latn".into(), "zho_Hans".into());
        let k2: TransKey = ("a".into(), "INBOX".into(), 2, "eng_Latn".into(), "zho_Hans".into());
        let k3: TransKey = ("a".into(), "INBOX".into(), 3, "eng_Latn".into(), "zho_Hans".into());
        assert_eq!(c.get(&k1), None);
        c.put(k1.clone(), "one".into());
        assert_eq!(c.get(&k1), Some("one".into()));
        c.put(k2.clone(), "two".into());
        c.put(k3.clone(), "three".into()); // 超容量，淘汰最早的 k1
        assert_eq!(c.get(&k1), None, "k1 should be evicted");
        assert_eq!(c.get(&k3), Some("three".into()));
    }
}

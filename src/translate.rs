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

use std::path::Path;

use ct2rs::tokenizers::hf::Tokenizer as HfTokenizer;
use ct2rs::{Config, TranslationOptions, Translator};

/// 本地 NLLB 翻译器。持有已加载的 CTranslate2 模型 + 分词器。
pub struct NllbLocal {
    translator: Translator<HfTokenizer>,
}

impl NllbLocal {
    /// 从模型目录加载（目录内含 CT2 模型 + tokenizer.json / sentencepiece.bpe.model）。
    pub fn load(model_dir: &Path) -> Result<NllbLocal, String> {
        let mut tokenizer = HfTokenizer::new(model_dir)
            .map_err(|e| format!("加载分词器失败: {e}"))?;
        // 关闭自动 special-token 后处理，改由 translate_one 手动拼接
        // "</s> <src_lang>" 后缀，见上方模块说明。
        tokenizer.disable_spacial_token();

        let translator = Translator::with_tokenizer(model_dir, tokenizer, &Config::default())
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
}

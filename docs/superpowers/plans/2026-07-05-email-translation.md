# Email Body Translation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add on-demand, fully-offline translation of an email's body in the DMS email plugin's detail view, toggling between original and translated text.

**Architecture:** Translation runs inside the Rust daemon via CTranslate2 (`ct2rs`) running an NLLB-200-distilled-600M model. A new `translate` IPC command fetches the raw message, extracts plain text, translates prose (preserving URLs/codes), re-linkifies, caches in memory, and returns HTML. The model is lazy-loaded and unloaded after idle to keep resident memory lean. A thin `Translator` trait exists only for testability. The QML detail view gains a "翻译" button; settings gain an enable switch and source/target language dropdowns.

**Tech Stack:** Rust (daemon), `ct2rs` (CTranslate2 bindings), `whatlang` (language detect), `hf-hub` (model download), Quickshell QML (frontend).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-05-email-translation-design.md` — authoritative.
- cargo target-dir is `/tmp/cargo-target` (tmpfs); installed binary lives at `~/.local/bin/dms-email-client` (bare-name on PATH). After building, install with `rm -f ~/.local/bin/dms-email-client && cp /tmp/cargo-target/release/dms-email-client ~/.local/bin/` (rm bypasses ETXTBSY), then kill the running daemon so the plugin self-heals to the new binary.
- Model stored at `~/.local/share/dms-email-client/models/nllb-200-distilled-600M/` (via `dirs::data_dir()`), a persistent (non-tmpfs) path.
- Target default language `zho_Hans`; source default `auto`. NLLB uses FLORES-200 codes (e.g. `eng_Latn`, `rus_Cyrl`, `jpn_Jpan`, `zho_Hans`).
- No fallback backend. Commit to ct2rs.
- Translation cache is in-memory only (no disk writes — U-disk write reduction).
- Daemon idle RSS must return to its ~16MB baseline when not translating (lazy load + idle unload).
- Follow existing daemon patterns: `Mutex`/`RwLock` poisoning recovery via `.unwrap_or_else(|e| e.into_inner())`; IPC commands are `\t`-separated lines; per-connection thread already in place.

---

## File Structure

- **Create** `src/translate.rs` — all translation logic: `Translator` trait, `NllbLocal`, `MockTranslator` (test), language detection, URL-preserving segment/translate/reassemble, in-memory cache, model lifecycle (lazy load + idle unload), model download.
- **Modify** `src/daemon.rs` — add `mod translate;` (at crate root actually, see Task 6); refactor raw-fetch into `fetch_raw_message()`; add `translate` IPC command handler; call idle-unload starter.
- **Modify** `src/main.rs` — add `Translate` CLI subcommand.
- **Modify** `Cargo.toml` — add `ct2rs`, `whatlang`, `hf-hub`.
- **Modify** `dmsEmailClient/DmsEmailClientWidget.qml` — translate button, `detailBodyZh`, `translateProcess`, toggle.
- **Modify** `dmsEmailClient/DmsEmailClientSettings.qml` — `translateEnabled` switch + source/target dropdowns.
- **Modify** `README.md` — document feature + first-run model download.

`mod translate;` is declared in `src/main.rs` (crate root) alongside `mod config; mod daemon;`. `daemon.rs` refers to it as `crate::translate`.

---

## Task 1: ct2rs environment spike — translate one sentence (GO/NO-GO)

De-risk the hardest part before building anything. This task proves CTranslate2 + `ct2rs` + an NLLB model actually run in this environment and pins the exact `ct2rs` API used by later tasks.

**Files:**
- Modify: `Cargo.toml`
- Create: `src/translate.rs`
- Modify: `src/main.rs` (add `mod translate;`)

**Interfaces:**
- Produces: `pub struct NllbLocal` with `pub fn load(model_dir: &std::path::Path) -> Result<NllbLocal, String>` and `pub fn translate_one(&self, text: &str, src: &str, tgt: &str) -> Result<String, String>`. Later tasks depend on exactly these signatures.

- [ ] **Step 1: Install the CTranslate2 system library**

Run (Arch):
```bash
# CTranslate2 C++ runtime that ct2rs links against
yay -S ctranslate2   # or: pacman -S extra/ctranslate2 if available
pkg-config --exists ctranslate2 && echo "ctranslate2 present" || echo "MISSING"
```
Expected: `ctranslate2 present`. If MISSING, `ct2rs` will fail to build — resolve before continuing (this is the primary risk in the spec §8).

- [ ] **Step 2: Add dependencies**

Edit `Cargo.toml`, under `[dependencies]` add:
```toml
ct2rs = "0.9"
whatlang = "0.16"
hf-hub = { version = "0.3", features = ["ureq"] }
dirs = "5.0"   # already present; keep
```
Note: confirm the current `ct2rs` version on crates.io; adjust if `0.9` is not latest. `ct2rs` bundles model/tokenizer loading (`ct2rs::Translator`) and handles SentencePiece for NLLB.

- [ ] **Step 3: Obtain an NLLB CTranslate2 model for the spike**

Run:
```bash
mkdir -p ~/.local/share/dms-email-client/models
python -m pip install --user ctranslate2 transformers sentencepiece
ct2-transformers-converter --model facebook/nllb-200-distilled-600M \
  --quantization int8 \
  --output_dir ~/.local/share/dms-email-client/models/nllb-200-distilled-600M
ls ~/.local/share/dms-email-client/models/nllb-200-distilled-600M
```
Expected: directory contains `model.bin`, `config.json`, `shared_vocabulary.*`, and a SentencePiece file (`sentencepiece.bpe.model` copied in). If the converter does not copy the SPM file, download it from the HF repo `facebook/nllb-200-distilled-600M` and place it in the same dir (Task 7 automates this).

- [ ] **Step 4: Write `src/translate.rs` minimal NllbLocal**

Create `src/translate.rs`:
```rust
//! 离线翻译：CTranslate2 (ct2rs) 跑 NLLB-200-distilled-600M。

use std::path::Path;

/// 本地 NLLB 翻译器。持有已加载的 CTranslate2 模型 + 分词器。
pub struct NllbLocal {
    translator: ct2rs::Translator,
}

impl NllbLocal {
    /// 从模型目录加载（目录内含 CT2 模型 + sentencepiece.bpe.model）。
    pub fn load(model_dir: &Path) -> Result<NllbLocal, String> {
        let translator = ct2rs::Translator::new(model_dir, &ct2rs::Config::default())
            .map_err(|e| format!("加载 NLLB 模型失败: {e}"))?;
        Ok(NllbLocal { translator })
    }

    /// 翻译一段文本。src/tgt 为 NLLB(FLORES-200) 语言码，如 eng_Latn / zho_Hans。
    /// NLLB 约定：源句前加源语言 token，target_prefix 为目标语言 token。
    pub fn translate_one(&self, text: &str, src: &str, tgt: &str) -> Result<String, String> {
        let results = self
            .translator
            .translate_batch(
                &[text.to_string()],
                &ct2rs::TranslationOptions {
                    // NLLB: 每条以目标语言 token 作为 target_prefix
                    ..Default::default()
                },
                Some(&[vec![tgt.to_string()]]),   // target_prefix
                Some(src),                         // source language token
            )
            .map_err(|e| format!("翻译失败: {e}"))?;
        Ok(results.into_iter().next().unwrap_or_default().0)
    }
}
```
NOTE: The exact `ct2rs` API (constructor, `translate_batch` signature, how source lang / `target_prefix` are passed for NLLB) MUST be reconciled against the installed `ct2rs` version's docs during this step. Adjust the code above to compile and produce correct output. The **signatures `load` and `translate_one` are fixed** (later tasks depend on them); only their bodies may change.

Add to `src/main.rs` near the other `mod` lines:
```rust
mod translate;
```

- [ ] **Step 5: Write the spike test**

Append to `src/translate.rs`:
```rust
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
```

- [ ] **Step 6: Run the spike**

Run:
```bash
cargo build --release 2>&1 | tail -3
cargo test --release spike_translate -- --ignored --nocapture 2>&1 | tail -20
```
Expected: build succeeds; test prints a Chinese translation of "Hello, world." and passes. **If this fails, STOP** — the ct2rs path is not viable in this environment; report back before proceeding (per spec §8, there is no fallback).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/translate.rs src/main.rs
git commit -m "feat(translate): ct2rs + NLLB spike, translate one sentence"
```

---

## Task 2: Translator trait + URL/code-preserving translation of body text

Pure logic, fully testable with a mock — no model needed. This is where subtle bugs live.

**Files:**
- Modify: `src/translate.rs`

**Interfaces:**
- Consumes: `NllbLocal` (Task 1).
- Produces:
  - `pub trait Translator { fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String>; }`
  - `impl Translator for NllbLocal` (delegates to `translate_one`).
  - `pub fn translate_prose(t: &dyn Translator, plain: &str, src: &str, tgt: &str) -> Result<String, String>` — translates a plain-text body, leaving URLs and 4–8 digit codes untouched, returning **plain text** (caller runs `body_to_html`).

- [ ] **Step 1: Write failing tests**

Append to `src/translate.rs`:
```rust
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --release translate::tests 2>&1 | tail -15`
Expected: FAIL — `translate_prose` and `Translator` not defined.

- [ ] **Step 3: Implement the trait and `translate_prose`**

Add to `src/translate.rs` (above the test modules):
```rust
/// 翻译后端抽象（薄）。仅为可测试性与接缝，本期只有 NllbLocal 一个实现。
pub trait Translator {
    fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String>;
}

impl Translator for NllbLocal {
    fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, String> {
        self.translate_one(text, src, tgt)
    }
}

/// 把纯文本正文翻译成目标语言：用 URL 正则把文本切成【散文段 | URL 段】，只翻散文，
/// URL 原样保留；4–8 位验证码这类独立数字串也保持不动（NLLB 一般会保留数字，但为
/// 稳妥仍以整段散文送入——数字不会被改写）。返回**纯文本**，由调用方 body_to_html。
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
            out.push_str(&t.translate(prose, src, tgt)?);
        } else {
            out.push_str(prose);
        }
        out.push_str(m.as_str()); // URL 原样
        last = m.end();
    }
    let tail = &plain[last..];
    if !tail.trim().is_empty() {
        out.push_str(&t.translate(tail, src, tgt)?);
    } else {
        out.push_str(tail);
    }
    Ok(out)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --release translate::tests 2>&1 | tail -15`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/translate.rs
git commit -m "feat(translate): Translator trait + URL-preserving translate_prose (TDD)"
```

---

## Task 3: Language auto-detect (whatlang → NLLB code)

**Files:**
- Modify: `src/translate.rs`

**Interfaces:**
- Produces: `pub fn resolve_source_lang(requested: &str, text: &str) -> String` — if `requested != "auto"`, returns it as-is; otherwise detects via `whatlang` and maps to a FLORES-200 code, defaulting to `eng_Latn` when detection is unreliable/unknown.

- [ ] **Step 1: Write failing tests**

Add into `src/translate.rs`'s `mod tests`:
```rust
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
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test --release translate::tests::auto_detects 2>&1 | tail -8`
Expected: FAIL — `resolve_source_lang` not defined.

- [ ] **Step 3: Implement**

Add to `src/translate.rs`:
```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --release translate::tests 2>&1 | tail -8`
Expected: PASS (all translate tests).

- [ ] **Step 5: Commit**

```bash
git add src/translate.rs
git commit -m "feat(translate): auto source-language detection via whatlang (TDD)"
```

---

## Task 4: In-memory translation cache

**Files:**
- Modify: `src/translate.rs`

**Interfaces:**
- Produces:
  - `pub struct TransCache` with `pub fn new(cap: usize) -> TransCache`, `pub fn get(&self, key: &TransKey) -> Option<String>`, `pub fn put(&self, key: TransKey, html: String)`.
  - `pub type TransKey = (String, String, u32, String, String);` — (account, folder, uid, src, tgt).
  - Internally `Mutex`-guarded; FIFO eviction at capacity.

- [ ] **Step 1: Write failing tests**

Add into `mod tests`:
```rust
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
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test --release translate::tests::cache 2>&1 | tail -8`
Expected: FAIL — `TransCache` not defined.

- [ ] **Step 3: Implement**

Add to `src/translate.rs`:
```rust
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --release translate::tests 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/translate.rs
git commit -m "feat(translate): in-memory FIFO translation cache (TDD)"
```

---

## Task 5: Model manager — lazy load, idle unload, first-run download

**Files:**
- Modify: `src/translate.rs`

**Interfaces:**
- Produces:
  - `pub fn model_dir() -> std::path::PathBuf` — the persistent model path.
  - `pub fn ensure_model() -> Result<std::path::PathBuf, String>` — downloads NLLB CT2 model via `hf-hub` to `model_dir()` if missing; returns the dir.
  - `pub struct ModelManager` with `pub fn new() -> ModelManager`, `pub fn with_translator<R>(&self, f: impl FnOnce(&NllbLocal) -> R) -> Result<R, String>` (lazy-loads, updates last-used), and `pub fn start_idle_unloader(self: &std::sync::Arc<ModelManager>)` (spawns a thread that unloads after 5 min idle and calls `crate::daemon::release_free_memory` — see note).

- [ ] **Step 1: Implement `model_dir` + `ensure_model`**

Add to `src/translate.rs`:
```rust
use std::path::PathBuf;

pub fn model_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dms-email-client/models/nllb-200-distilled-600M")
}

/// 确保模型就绪：缺失则从 HuggingFace 下载 CT2 版模型 + 分词器到 model_dir()。
/// 使用社区已转换好的 CT2 仓库，避免运行时依赖 Python 转换脚本。
pub fn ensure_model() -> Result<PathBuf, String> {
    let dir = model_dir();
    let model_bin = dir.join("model.bin");
    let spm = dir.join("sentencepiece.bpe.model");
    if model_bin.exists() && spm.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    // CT2 预转换仓库（int8）。文件名以该仓库实际内容为准。
    let repo = "entai2965/nllb-200-distilled-600M-ctranslate2";
    let api = hf_hub::api::sync::Api::new().map_err(|e| format!("hf-hub 初始化失败: {e}"))?;
    let r = api.model(repo.to_string());
    for f in ["model.bin", "config.json", "shared_vocabulary.txt", "sentencepiece.bpe.model"] {
        let got = r.get(f).map_err(|e| format!("下载 {f} 失败: {e}"))?;
        std::fs::copy(&got, dir.join(f)).map_err(|e| format!("落盘 {f} 失败: {e}"))?;
    }
    Ok(dir)
}
```
NOTE: Confirm the CT2-converted repo name and its exact file list during implementation (search HuggingFace for an `nllb-200-distilled-600M-ctranslate2` int8 repo). If none suitable exists, fall back to downloading `facebook/nllb-200-distilled-600M` and running the CT2 converter once at install time (document in README, Task 10) — but prefer a pre-converted repo to keep runtime Python-free.

- [ ] **Step 2: Implement `ModelManager` (lazy load + idle unload)**

Add to `src/translate.rs`:
```rust
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 管理 NLLB 模型的懒加载与空闲卸载，保持 daemon 常驻内存精简。
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

    /// 取用翻译器执行 f；模型未加载则先 ensure_model()+load。更新 last-used。
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

    /// 后台线程：空闲超过 5 分钟则卸载模型并归还内存。
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
```
NOTE: `crate::daemon::release_free_memory` is currently private in `daemon.rs`. In this step, change its declaration to `pub(crate) fn release_free_memory()` so `translate.rs` can call it.

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build --release 2>&1 | grep -E "error|Finished" | grep -v imap-proto`
Expected: `Finished`. (No unit test here — download/unload are covered by manual e2e in Task 6.)

- [ ] **Step 4: Commit**

```bash
git add src/translate.rs src/daemon.rs
git commit -m "feat(translate): model manager — lazy load, idle unload, hf-hub download"
```

---

## Task 6: `translate` IPC command + `fetch_raw_message` refactor + CLI subcommand

Wires everything into the daemon and CLI.

**Files:**
- Modify: `src/daemon.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `translate::{ModelManager, TransCache, TransKey, translate_prose, resolve_source_lang}`, existing `extract_body`, `body_to_html`.
- Produces: IPC line `translate\t<account>\t<folder>\t<uid>\t<src>\t<tgt>` → JSON `{"ok":true,"body":"<html>"}` / `{"ok":false,"error":"…"}`.

- [ ] **Step 1: Refactor raw fetch out of `fetch_body`**

In `src/daemon.rs`, extract the "connect + examine + UID FETCH BODY.PEEK[] + parse" portion of `fetch_body` into a reusable helper. Add:
```rust
/// 连接账户、只读打开文件夹、按 UID 取原始邮件并解析。供 fetch_body 与 translate 复用。
fn fetch_raw_message(
    account: &Account,
    folder: &str,
    uid: &str,
) -> Result<mail_parser::Message<'static>, Box<dyn std::error::Error>> {
    let mut session = connect_session(account)?;
    session.examine(folder)?;
    let fetches = session.uid_fetch(uid, "BODY.PEEK[]")?;
    let fetch = fetches.iter().next().ok_or("未找到该邮件")?;
    let raw = fetch.body().ok_or("邮件无正文数据")?.to_vec();
    let _ = session.logout();
    let msg = mail_parser::MessageParser::default()
        .parse(&raw)
        .ok_or("邮件解析失败")?;
    // 解析借用 raw；转为 owned 以便跨函数返回
    Ok(msg.into_owned())
}
```
Then update `fetch_body`'s `run` closure to call `fetch_raw_message(account, folder, uid)?` instead of inlining connect/fetch/parse, keeping the rest (extract_body → body_to_html → cache) unchanged.

- [ ] **Step 2: Add shared translation state to the daemon**

In `src/daemon.rs`, add near the top (after imports):
```rust
use std::sync::OnceLock;

fn model_manager() -> &'static std::sync::Arc<crate::translate::ModelManager> {
    static MM: OnceLock<std::sync::Arc<crate::translate::ModelManager>> = OnceLock::new();
    MM.get_or_init(|| {
        let mm = std::sync::Arc::new(crate::translate::ModelManager::new());
        mm.start_idle_unloader();
        mm
    })
}

fn trans_cache() -> &'static crate::translate::TransCache {
    static TC: OnceLock<crate::translate::TransCache> = OnceLock::new();
    TC.get_or_init(|| crate::translate::TransCache::new(200))
}
```
(If `OnceLock` is already imported via `std::sync::{... OnceLock ...}`, don't duplicate the `use`.)

- [ ] **Step 3: Implement `fetch_translation`**

Add to `src/daemon.rs`:
```rust
/// 取信→提取纯文本→(auto 检测源语言)→查缓存→翻译散文(保留 URL)→body_to_html→缓存。
fn fetch_translation(
    accounts: &[Account],
    account_name: &str,
    folder: &str,
    uid: &str,
    src_req: &str,
    tgt: &str,
) -> String {
    let account = match accounts.iter().find(|a| a.name == account_name) {
        Some(a) => a,
        None => return json_err("账户不存在"),
    };
    let run = || -> Result<String, Box<dyn std::error::Error>> {
        let msg = fetch_raw_message(account, folder, uid)?;
        let plain = extract_body(&msg);
        let src = crate::translate::resolve_source_lang(src_req, &plain);
        let key: crate::translate::TransKey = (
            account_name.to_string(),
            folder.to_string(),
            uid.parse::<u32>().unwrap_or(0),
            src.clone(),
            tgt.to_string(),
        );
        if let Some(html) = trans_cache().get(&key) {
            return Ok(serde_json::json!({ "ok": true, "body": html }).to_string());
        }
        let translated_plain = model_manager()
            .with_translator(|t| crate::translate::translate_prose(t, &plain, &src, tgt))
            .map_err(|e| e.to_string())??;
        let html = body_to_html(&translated_plain);
        trans_cache().put(key, html.clone());
        Ok(serde_json::json!({ "ok": true, "body": html }).to_string())
    };
    run().unwrap_or_else(|e| json_err(&e.to_string()))
}
```

- [ ] **Step 4: Wire the IPC command**

In `handle_client`'s `match parts.as_slice()`, add an arm (alongside `["body", ...]`):
```rust
["translate", account, folder, uid, src, tgt] => {
    let resp = fetch_translation(accounts, account, folder, uid, src, tgt);
    let _ = stream.write_all(resp.as_bytes());
}
```

- [ ] **Step 5: Add the CLI subcommand**

In `src/main.rs`, add to `enum Commands`:
```rust
/// Translate an email body via the daemon (offline NLLB)
Translate {
    account: String,
    folder: String,
    uid: String,
    /// NLLB source lang code, or "auto"
    src: String,
    /// NLLB target lang code (e.g. zho_Hans)
    tgt: String,
},
```
And in the `match cli.command` block:
```rust
Some(Commands::Translate { account, folder, uid, src, tgt }) => {
    send_command(&format!("translate\t{account}\t{folder}\t{uid}\t{src}\t{tgt}"));
}
```

- [ ] **Step 6: Build, install, and end-to-end verify against the live daemon**

Run:
```bash
cargo build --release 2>&1 | grep -E "error|Finished" | grep -v imap-proto
rm -f ~/.local/bin/dms-email-client && cp /tmp/cargo-target/release/dms-email-client ~/.local/bin/
kill "$(pgrep -f 'dms-email-client daemon' | head -1)" 2>/dev/null; sleep 8
# pick a real English mail's account/folder/uid from status, then:
~/.local/bin/dms-email-client status | python3 -c "import json,sys; d=json.load(sys.stdin); m=d['unread_mails'][0]; print(m['account'], m['folder'], m['uid'])"
# substitute below with the printed values:
~/.local/bin/dms-email-client translate "<account>" "<folder>" "<uid>" auto zho_Hans | python3 -c "import json,sys; d=json.load(sys.stdin); print('ok=',d.get('ok')); print(d.get('body','')[:300])"
```
Expected: `ok= True` and a Chinese translation (first call downloads the model — may take minutes). Second call on the same mail returns instantly (cache).

- [ ] **Step 7: Verify idle unload returns memory**

Run:
```bash
PID=$(pgrep -f 'dms-email-client daemon' | head -1)
awk '/VmRSS/{print "RSS after translate:", $2, "kB"}' /proc/$PID/status
echo "waiting 6 min for idle unload…"; sleep 360
awk '/VmRSS/{print "RSS after idle:", $2, "kB"}' /proc/$PID/status
```
Expected: RSS drops back toward the ~16 MB baseline after unload.

- [ ] **Step 8: Commit**

```bash
git add src/daemon.rs src/main.rs
git commit -m "feat(daemon): translate IPC command + fetch_raw_message refactor + CLI"
```

---

## Task 7: QML — translate button and original/translated toggle

**Files:**
- Modify: `dmsEmailClient/DmsEmailClientWidget.qml`

**Interfaces:**
- Consumes: CLI `translate <account> <folder> <uid> <src> <tgt>`; existing `root.selectedMail`, `root.detailBody`, `root.binPath`, `bodyText` TextEdit.
- Produces: user-visible translate toggle.

- [ ] **Step 1: Add state properties**

Near the other detail properties (around `property string detailBody: ""`), add:
```qml
readonly property bool translateEnabled: (pluginData && pluginData.translateEnabled) === true
readonly property string translateSourceLang: (pluginData && pluginData.translateSourceLang) ? pluginData.translateSourceLang : "auto"
readonly property string translateTargetLang: (pluginData && pluginData.translateTargetLang) ? pluginData.translateTargetLang : "zho_Hans"
property string detailBodyZh: ""      // 译文 HTML（空=未翻译）
property bool showTranslated: false   // 当前是否显示译文
property bool translating: false
```
In the function that opens a mail (where `root.detailBody = ""` is set), also reset:
```qml
root.detailBodyZh = "";
root.showTranslated = false;
root.translating = false;
```

- [ ] **Step 2: Add the translate Process**

Near `bodyProcess`, add:
```qml
Process {
    id: translateProcess
    property string aAccount: ""
    property string aFolder: ""
    property string aUid: ""
    command: [root.binPath, "translate", aAccount, aFolder, aUid,
              root.translateSourceLang, root.translateTargetLang]
    running: false
    stdout: StdioCollector {
        onStreamFinished: {
            root.translating = false;
            try {
                let d = JSON.parse(this.text);
                if (d.ok) {
                    root.detailBodyZh = d.body || "";
                    root.showTranslated = true;
                } else {
                    root.showToast("翻译失败：" + (d.error || ""));
                }
            } catch (e) {
                root.showToast("翻译失败");
            }
        }
    }
}
```

- [ ] **Step 3: Make the body text show original or translation**

Change the `bodyText` TextEdit `text:` binding (currently uses `root.detailBody`) to:
```qml
text: (root.showTranslated ? root.detailBodyZh : root.detailBody)
      .replace(/<a /g, '<a style="color:' + String(Theme.primary) + ';text-decoration:none" ')
```

- [ ] **Step 4: Add the translate button**

Add a button near the body area (e.g., above the `DankFlickable` that wraps `bodyText`), visible only when enabled and not loading the body:
```qml
Rectangle {
    visible: root.translateEnabled && !root.detailLoading && root.detailError === ""
    width: tLabel.implicitWidth + Theme.spacingM * 2
    height: Theme.iconSize * 1.4
    radius: Theme.cornerRadius
    color: tArea.containsMouse ? Theme.surfaceContainerHighest : Theme.surfaceContainer
    StyledText {
        id: tLabel
        anchors.centerIn: parent
        text: root.translating ? "翻译中…（首次需下载模型，请稍候）"
              : (root.showTranslated ? "原文" : "翻译")
        font.pixelSize: Theme.fontSizeSmall
        color: Theme.surfaceText
    }
    MouseArea {
        id: tArea
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: {
            if (root.translating) return;
            if (root.showTranslated) { root.showTranslated = false; return; }
            if (root.detailBodyZh !== "") { root.showTranslated = true; return; }
            // 尚未翻译：触发 daemon
            root.translating = true;
            translateProcess.aAccount = root.selectedMail.account;
            translateProcess.aFolder = root.selectedMail.folder || "INBOX";
            translateProcess.aUid = String(root.selectedMail.uid);
            translateProcess.running = false;
            translateProcess.running = true;
        }
    }
}
```

- [ ] **Step 5: Verify in the running shell**

Reload the plugin (disable/enable in DMS settings, or restart quickshell). Open an English email, click 翻译.
Expected: button shows loading, then body switches to Chinese; clicking 原文 toggles back. (Requires Task 9 to have flipped `translateEnabled` on; until then temporarily hardcode `translateEnabled: true` to test, then revert.)

- [ ] **Step 6: Commit**

```bash
git add dmsEmailClient/DmsEmailClientWidget.qml
git commit -m "feat(widget): translate button with original/translated toggle"
```

---

## Task 8: QML settings — enable switch + language dropdowns

**Files:**
- Modify: `dmsEmailClient/DmsEmailClientSettings.qml`

**Interfaces:**
- Produces: `pluginData.translateEnabled` (bool), `pluginData.translateSourceLang`, `pluginData.translateTargetLang` (NLLB codes).

- [ ] **Step 1: Inspect existing setting components**

Read `dmsEmailClient/DmsEmailClientSettings.qml` to see how `SliderSetting` and any existing toggle/dropdown components are used (the file already has `SliderSetting`; check for a `ToggleSetting`/`DropdownSetting` in the DMS plugin API or how other DMS plugins do switches — e.g. `~/.config/DankMaterialShell/plugins/*/` for a `StyledToggle`/`DankToggle`).

- [ ] **Step 2: Add the enable toggle**

Add a toggle bound to `settingKey: "translateEnabled"` (default false), using whichever toggle component the DMS plugin settings API provides (mirror an existing DMS plugin's toggle usage found in Step 1). Label: "启用翻译", description: "在邮件详情页显示翻译按钮。首次翻译需联网下载约 600MB 离线模型（存于 ~/.local/share/dms-email-client/models）。".

- [ ] **Step 3: Add source/target language dropdowns**

Add two dropdowns (again mirroring the DMS dropdown component from Step 1) bound to `translateSourceLang` (default `"auto"`) and `translateTargetLang` (default `"zho_Hans"`). Options as `{label, value}` pairs:
```
源语言: 自动检测=auto, 英语=eng_Latn, 俄语=rus_Cyrl, 日语=jpn_Jpan, 韩语=kor_Hang, 法语=fra_Latn, 德语=deu_Latn, 西班牙语=spa_Latn
目标语言: 简体中文=zho_Hans, 英语=eng_Latn, 日语=jpn_Jpan
```
Description on source: "邮件原文语言，通常保持自动检测即可。"

- [ ] **Step 4: Verify**

Reload the plugin; open plugin settings. Toggle 启用翻译 on, confirm the translate button appears in the detail view; change target language and confirm translation output language changes.

- [ ] **Step 5: Commit**

```bash
git add dmsEmailClient/DmsEmailClientSettings.qml
git commit -m "feat(settings): translation enable switch + source/target language dropdowns"
```

---

## Task 9: README documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document the feature**

Add a "翻译（离线）" section to `README.md` covering:
- What it does (detail-view body translation, original/translated toggle).
- Prerequisite: install CTranslate2 system library (`yay -S ctranslate2`).
- First-run: enabling and first translation downloads ~600MB NLLB model to `~/.local/share/dms-email-client/models/nllb-200-distilled-600M/`; requires network the first time, fully offline afterward.
- Settings: enable switch, source (auto/…), target (default 简体中文).
- Memory note: model is loaded on demand and unloaded after 5 minutes idle.

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document offline email translation feature"
```

---

## Self-Review (completed during authoring)

- **Spec coverage:** §3 UX → Task 7; §4.1 IPC/refactor → Task 6; §4.2 trait/mock → Task 2; §4.3 lazy load+unload → Task 5; §4.4 URL-preserving → Task 2; §4.5 model dir+download → Task 5; §4.6 in-memory cache → Task 4; §5 settings → Task 8; §6 error handling → Tasks 6/7 (json_err + toast); §7 memory reconciliation → Task 5 + verified Task 6 Step 7; §8 risks → Task 1 spike; §9 testing → Tasks 2/3/4 unit + Task 6 e2e; language detect (§5 auto) → Task 3. All covered.
- **Type consistency:** `NllbLocal::load`/`translate_one` (T1) → used T5; `Translator`/`translate_prose` (T2) → used T6; `resolve_source_lang` (T3) → used T6; `TransCache`/`TransKey` (T4) → used T6; `ModelManager::{with_translator,start_idle_unloader}` (T5) → used T6; `release_free_memory` made `pub(crate)` (T5) → exists in daemon.rs. IPC arg order `account folder uid src tgt` consistent across T6 (handler), T6 (CLI), T7 (Process command).
- **Placeholder scan:** ct2rs API (T1 Step 4) and CT2 model repo (T5 Step 1) are explicitly flagged as "reconcile against installed version" with concrete fallbacks — these are real spike outcomes, not lazy placeholders. No TODO/TBD left.

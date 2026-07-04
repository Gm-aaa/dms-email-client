# 邮件正文翻译功能 — 设计文档

日期：2026-07-05
状态：已确认，待实现计划

## 1. 概述

给 DMS 邮件客户端插件加"正文翻译"：在邮件详情页手动把外文正文译成目标语言（默认简体中文），一键在原文/译文之间切换。翻译**完全离线、在本机**完成——用 CTranslate2（`ct2rs` Rust 绑定）在守护进程内跑 NLLB-200-distilled-600M 模型。

## 2. 目标 / 非目标

**目标**
- 详情页正文按需翻译，原文↔译文切换。
- 离线、隐私（正文不出本机）、零成本。
- 不破坏 daemon 的常驻精简内存（不翻译时仍 ~16MB）。
- 源/目标语言可在插件设置里配置。

**非目标（本期不做）**
- 不翻译主题、不翻译列表、不批量翻译。
- 不做第二个翻译后端（deeplx/google/ollama）——但保留可扩展的 `Translator` trait 接口。
- 不做译文落盘缓存（仅内存缓存）。

## 3. 交互（前端 QML）

- 详情页在正文区上方加「翻译」按钮，仅当设置开关 `translateEnabled` 为真时显示。
- 点击：调用 daemon `translate` 命令；返回后把正文区从原文 `detailBody` 切换为译文 `detailBodyZh`，按钮变「原文」。再点切回。
- 状态：翻译进行中显示「翻译中…」；模型首次需下载时显示「下载模型中…（约 600MB，仅首次）」。
- 译文与原文一样是 linkify 过的 HTML（🔗/⧉/验证码可点），用同一个 `TextEdit(RichText)` 渲染。

## 4. 架构

### 4.1 总览
翻译逻辑放在**守护进程内**，复用现有的取信逻辑：把 `fetch_body` 里"连接 + `examine` + `UID FETCH BODY.PEEK[]` + `MessageParser::parse`"抽成一个 `fetch_raw_message()` 辅助函数，`fetch_body` 和 `translate` 都用它拿到 `mail_parser::Message`，再各自 `extract_body`。避免在别处重复 IMAP 逻辑。翻译走的是 `extract_body` 的**纯文本**（非 `body_to_html` 的 HTML）。

新增 IPC 命令：
```
translate\t<account>\t<folder>\t<uid>\t<src_lang>\t<tgt_lang>
```
- `src_lang`：NLLB 语言码（如 `eng_Latn`）或字面量 `auto`（daemon 用 `whatlang` 检测）。
- `tgt_lang`：NLLB 语言码（默认 `zho_Hans`）。
- 返回：`{"ok":true,"body":"<译文 HTML>"}` 或 `{"ok":false,"error":"…"}`。

CLI 侧新增 `translate` 子命令（`send_command` 一次性响应，与 `body` 同款）。

### 4.2 Translator 抽象（薄 trait，主要为可测试性）
```rust
trait Translator {
    /// 把 text 从 src 语言翻成 tgt 语言。src/tgt 为 NLLB 语言码。
    fn translate(&self, text: &str, src: &str, tgt: &str) -> Result<String, TranslateError>;
}
```
- 唯一实现 `NllbLocal`（ct2rs + NLLB-600M）。
- 单元测试用 `MockTranslator`（回显/加标记）注入，从而在**不加载模型**下测"URL 保留切段 / 拼回 / 缓存 / 语言检测"等核心逻辑。
- 不实现第二个后端；trait 仅作接缝。

### 4.3 模型生命周期（懒加载 + 空闲卸载）
- 全局 `Mutex<Option<NllbLocal>>`，初始 `None`。
- 首次 `translate` 时加载模型（若本地缺模型 → 先下载，见 4.5）；加载成功后驻留内存（~600MB）。
- 后台计时：距上次翻译 5 分钟无调用 → 卸载（置 `None` + `release_free_memory()`），RSS 回落到基线 ~16MB。
- `Mutex` 保证并发 translate 串行（NLLB 单次推理已够重，串行可接受）。

### 4.4 内容 / 链接处理
1. `extract_body(msg)` 得到纯文本（已含占位符→HTML 回退修复）。
2. 用现有 URL 正则把文本切成【散文段 | URL/验证码原样段】序列。
3. 只把散文段逐段送 `Translator::translate`；URL/验证码原样保留。
4. 拼回译文纯文本 → `body_to_html()` 重新 linkify → 得到译文 HTML。
- 好处：译文里链接/复制/验证码仍可点；URL 不会被模型翻坏。
- 逐段翻译会损失跨段上下文，对邮件可接受。

### 4.5 模型获取
- 存放：`~/.local/share/dms-email-client/models/nllb-200-distilled-600M/`（`dirs::data_dir()`，**持久盘、非 tmpfs**）。含 CTranslate2 转换后的模型 + SentencePiece 分词器。
- 首次翻译若目录缺失/不完整 → 从 HuggingFace 下载（约 600MB int8）。**v1 采用阻塞式**：该次 `translate` 调用一直等到下载 + 加载完成再返回译文；QML 侧在按钮 loading 文案上给出"首次翻译需下载模型（~600MB），请稍候…"的提示（前端无法区分"下载中/翻译中"，用同一 loading 态即可，仅首次会久）。
- README + 设置项文案写明：首次需联网下载 ~600MB、存放位置、可离线使用。

### 4.6 译文缓存（内存）
- daemon 内 `Mutex<HashMap<(String account, String folder, u32 uid, String src, String tgt), String>>` 缓存译文 HTML（键含 src/tgt，换语言不会串味）。
- 命中即返回，不重复推理。简单容量上限（如 200 条，超出按插入顺序 FIFO 淘汰）防膨胀。
- **不落盘**（契合 U 盘减写）；daemon 重启即清空。
- 正文本身变化时（同 uid 重新取信）对应译文缓存项作废。

## 5. 配置 / 设置

- **前端设置**（`DmsEmailClientSettings.qml`，存 pluginData）：
  - `translateEnabled`（开关，默认关）。
  - `translateSourceLang`（下拉：自动检测 + 常见语言 → NLLB 码，默认"自动"）。
  - `translateTargetLang`（下拉：常见语言 → NLLB 码，默认 `zho_Hans`）。
- 语言下拉展示友好名（"英语/俄语/日语/…"），值为 NLLB 码；"自动"传字面量 `auto`。
- QML 把选中的 src/tgt 随 `translate` 命令传给 daemon。

## 6. 错误处理

- 模型缺失 → 触发下载并回中间态；下载失败 → `{ok:false,error}` → UI toast，保留原文。
- CT2 运行库缺失/加载失败 → 明确错误信息（提示需装 CTranslate2）。
- 语言检测失败 → 回退默认源语言（英语）或原样返回并提示。
- 翻译异常 → toast 报错，正文保持原文，绝不卡死详情页。

## 7. 内存与常驻精简的调和

翻译是可选、重量级操作。通过 4.3 的"懒加载 + 空闲 5 分钟卸载"，daemon 只在实际翻译的时间窗内占用 ~600MB，其余时间维持 ③ 优化后的 ~16MB。译文缓存在内存但体量小（HTML 文本）。

## 8. 风险与前置

- **`ct2rs` 依赖 CTranslate2 C++ 库**：Arch 需装 `ctranslate2`（AUR 或自编），并让 `ct2rs` 链接到它。构建复杂度上升——这是本功能最硬的一块，实现计划第一步应先打通"ct2rs + NLLB 跑通一句翻译"的最小验证。
- **SentencePiece 分词器**：NLLB 用 SPM，需随模型下载并让 ct2rs/CT2 正确加载。
- **模型体积**：~600MB 下载 + 常驻内存期间 ~600MB RSS。
- 认准 ct2rs，**不做兜底后端**（用户决定）。

## 9. 测试

- **单元测试（不需模型）**：用 `MockTranslator` 覆盖
  - URL/验证码保留切段 + 拼回（译文里 URL 原样、可 linkify）；
  - `auto` 源语言经 whatlang 检测映射到 NLLB 码；
  - 内存缓存命中/淘汰；
  - 空文本/纯 URL 文本等边界。
- **手动端到端**：真模型翻译一封英文邮件，人工核对；验证空闲卸载后 RSS 回落。

## 10. 已解决的决策记录

- 后端：ct2rs 进程内 NLLB-600M，真离线；不做兜底。
- 抽象：保留薄 `Translator` trait（仅为可测试 + 接缝），不实现第二后端。
- 交互：手动按钮切换，仅详情正文。
- 缓存：仅内存，不落盘。
- 源/目标语言：设置可配，默认 自动→简体中文。
- 模型：NLLB-600M-distilled int8（~600MB），首次联网下载到 data_dir，空闲 5 分钟卸载。

# Changelog

本项目所有值得注意的改动都记录在此文件。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循
[语义化版本](https://semver.org/lang/zh-CN/)。发布 tag 形如 `vX.Y.Z`，与 `Cargo.toml` /
`plugin.json` 的版本保持一致；推送该 tag 会由 GitHub Actions 自动构建并发布 Release。

## [Unreleased]

## [0.2.0] - 2026-07-07

### 新增
- 安装 / 卸载脚本 `install.sh` / `uninstall.sh`：默认从 GitHub Release 下载预编译二进制
  （加 `--build` 则本地源码编译），二进制安装到 `~/.local/bin`、插件复制到 DMS 插件目录
  （复制而非软链接）；卸载支持 `--purge` 一并清除配置 / 缓存 / 离线模型。安装位置可用
  环境变量覆盖。
- GitHub Actions 发布流程：推送 `v*` tag 自动编译并创建 Release，附预编译二进制
  `dms-email-client-x86_64-linux`，Release 说明取自本 CHANGELOG 对应版本段落。
- 配置项 `imap_timeout_secs`（默认 60，最小 5）：IMAP 稳态读写超时，设置面板「IMAP 超时」可调。
- 配置项 `body_cache_limit`（默认 500，0=不限）：正文磁盘缓存文件数上限，设置面板「正文缓存上限」可调。

### 变更
- 后端从单文件 `daemon.rs`（1277 行）按职责重构为多模块：`ipc` / `segment` / `mailhtml` /
  `sysmem`，以及 `daemon/{mod,state,imap_sync,commands}`。纯结构调整，行为不变。
- URL / 验证码切分逻辑统一到 `segment` 模块，消除正文渲染与翻译两处重复的正则规则。
- 设置面板文案厘清易混项：「本地缓存上限」→「邮件列表容量」、「缓存目录」→「正文缓存目录」。

### 修复（健壮性 / 兜底）
- IMAP 稳态操作（取头部 / 正文 / 标记已读）现有读写超时：半死的 TCP、NAT/代理静默丢连下
  会报错重连，而非永久阻塞导致账户不再刷新、取信一直转圈。后台等待新邮件的 IDLE 仍保持
  独立的 5 分钟长超时，不受影响。
- 账户线程内部若发生 panic，现由 `catch_unwind` 兜底自动重启，不再静默死亡后永不刷新。
- 正文磁盘缓存按文件数上限淘汰最旧文件，防止缓存目录无上限增长。
- 离线翻译模型下载改为「全部文件存在且非空」的完整性校验 + 每文件最多 3 次重试 + 断点续下，
  避免半截下载被误判为就绪、随后反复加载失败。

## [0.1.0]

- 初始版本：Rust 守护进程（IMAP IDLE 实时监听、多账户并发、垃圾箱智能识别、桌面通知、
  Unix socket IPC、正文磁盘缓存、配置热重载）+ DankMaterialShell QML 插件（状态栏小部件、
  下拉阅读器、单封/整箱已读管理、正文翻译：Google / DeepLX / 本地离线 NLLB）。

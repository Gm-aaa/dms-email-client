# DMS Email Client

DMS Email Client 是一个为 **DankMaterialShell (DMS)** 桌面环境量身定制的高性能邮件检查与通知插件。它采用 **Rust** 编写高性能后台守护进程，并结合 **QML (Quickshell)** 编写现代化的桌面小部件（Widget）和配置面板。

## 预览

<table>
  <tr>
    <td align="center" width="50%">
      <img src="docs/screenshot-popout.png" alt="未读邮件下拉面板" width="320"><br>
      <sub>下拉面板：账户分类筛选、垃圾邮件标签、已读/未读区分</sub>
    </td>
    <td align="center" width="50%">
      <img src="docs/screenshot-detail.png" alt="邮件详情" width="320"><br>
      <sub>邮件详情：链接渲染为「🔗 域名 ⧉」可点击打开/复制，可选中复制，一键标记已读</sub>
    </td>
  </tr>
  <tr>
    <td align="center" colspan="2">
      <img src="docs/screenshot-settings.png" alt="设置面板" width="460"><br>
      <sub>设置面板：常见服务商预设（QQ / Gmail / Outlook / 网易 163）、账户管理</sub>
    </td>
  </tr>
</table>

## 核心特性

- **高性能 Rust 后端**：
  - 采用基于事件驱动的 IMAP IDLE（推送通知）长连接，实时监听新邮件，资源占用极低。
  - 支持多账户并发监听（不同账户运行在独立的后台线程中）。
  - 内置垃圾邮件文件夹（Spam/Junk）智能识别，可配置监控文件夹。
  - 支持通过系统通知服务（D-Bus）发送新邮件桌面提醒。
  - 提供本地正文磁盘缓存，阅读已下载邮件时零网络延迟。
  - 支持配置文件变动监控与热重载。
- **IPC 通信**：
  - 后端守护进程与前端 QML 之间通过 Unix Socket 进行轻量级、低延迟的 JSON 数据交互。socket 位于 `$XDG_RUNTIME_DIR/dms-email-client.sock`（每用户私有；该环境变量缺失时回退到系统临时目录并以用户名区分）。
- **DMS QML 插件**：
  - **状态栏小部件**：支持水平和垂直状态栏模式下的 Pill 小部件展示，包含邮件图标、未读计数徽章以及错误状态气泡。
  - **下拉面板**：点击图标展示邮件列表下拉面板，支持快速浏览未读邮件、查看邮件发送人、主题及日期。
  - **内置阅读器**：点击邮件在下拉面板中直接查看正文（HTML 自动转纯文本）。正文中的链接渲染为「🔗 域名」可点击打开、附「⧉」一键复制完整链接；验证码（独立 4–8 位数字）可点击复制；正文支持鼠标选中复制。
  - **已读管理**：单封「标记已读」与整箱「一键已读」；已读邮件保留在列表中（仅去掉未读红点并暗化显示），未读邮件以红点标记。
  - **配置面板**：内置账户管理界面，提供 QQ 邮箱、Gmail、Outlook、网易 163 等常见 IMAP 服务商配置预设，支持调整刷新间隔、缓存上限、下拉面板高度等。

---

## 项目结构

```text
├── Cargo.toml                  # Rust 项目依赖配置
├── Cargo.lock
├── src/                        # 后端守护进程与 CLI 源码
│   ├── main.rs                 # 命令行解析与守护进程/客户端入口
│   ├── daemon.rs               # IMAP 核心监听循环、Unix Socket IPC、桌面通知
│   └── config.rs               # 配置文件 (config.toml) 加载与持久化逻辑
├── dmsEmailClient/             # DMS 前端插件目录（QML）
│   ├── plugin.json             # DMS 插件清单描述文件
│   ├── DmsEmailClientWidget.qml   # 状态栏小部件、下拉面板及 IPC 交互实现
│   └── DmsEmailClientSettings.qml # 插件设置面板 UI 界面
├── docs/                       # 文档资源（README 截图等）
└── target/                     # Rust 编译输出目录
```

---

## 配置文件说明

插件配置采用 TOML 格式，默认存储在 `~/.config/dms-email-client/config.toml`。首次运行或配置文件丢失时，后端会自动创建一份包含 QQ 邮箱示例的默认配置文件：

```toml
# 缓存的未读邮件总数上限（0 表示不限制），会在所有启用的账户间平均分配
cache_limit = 50

# 邮件正文磁盘缓存目录（留空则自动使用 $XDG_CACHE_HOME/dms-email-client，即 ~/.cache/dms-email-client）
cache_dir = ""

[[accounts]]
name = "QQ Mail"
host = "imap.qq.com"
port = 993
username = "your_email@qq.com"
password = "your_imap_authorization_code" # 多数邮箱需要独立的授权码而非账户登录密码
ssl = true
enabled = true
```

---

## 编译与安装

### 1. 编译后端程序
请确保您的系统中已安装 Rust 编译工具链（`cargo`）。在项目根目录下执行：

```bash
cargo build --release
```

编译生成的可执行二进制文件位于 `target/release/dms-email-client`。

将二进制安装到 `PATH` 中（QML 插件默认按名字 `dms-email-client` 查找，无需硬编码路径）：

```bash
# 方式一：装到用户目录（确保 ~/.local/bin 在 PATH 中）
install -Dm755 target/release/dms-email-client ~/.local/bin/dms-email-client

# 方式二：用 cargo 安装到 ~/.cargo/bin
cargo install --path .

# 方式三：系统级安装（需 root）
sudo install -m755 target/release/dms-email-client /usr/local/bin/dms-email-client
```

> 如果你不想把二进制放进 `PATH`，也可以把 `DmsEmailClientWidget.qml` 与 `DmsEmailClientSettings.qml` 顶部的 `binPath` 改为二进制的绝对路径。

### 2. 安装 DMS 插件
将 `dmsEmailClient` 目录复制或符号链接到 DankMaterialShell 的插件目录中。例如：

```bash
ln -s /absolute/path/to/dms-email-client/dmsEmailClient ~/.local/share/dms/plugins/dmsEmailClient
```

---

## 命令行与 IPC 协议

`dms-email-client` 既是后台守护进程，也是前端与守护进程交互的 CLI 工具。它支持以下命令行参数：

- **启动守护进程**：
  ```bash
  dms-email-client daemon
  ```
- **查询当前未读邮件列表（JSON 格式）**：
  ```bash
  dms-email-client status
  ```
- **获取单封邮件正文**：
  ```bash
  dms-email-client body <account> <folder> <uid>
  ```
- **标记单封邮件为已读**：
  ```bash
  dms-email-client read <account> <folder> <uid>
  ```
- **一键标记全部为已读**（省略 account 则所有账户）：
  ```bash
  dms-email-client read-all [account]
  ```
- **展示当前配置（JSON 格式）**：
  ```bash
  dms-email-client config show
  ```
- **导入并保存配置（通过标准输入接收 JSON）**：
  ```bash
  echo '{"cache_limit":50,"cache_dir":"...","accounts":[]}' | dms-email-client config save
  ```

### IPC 协议指令
向 socket（`$XDG_RUNTIME_DIR/dms-email-client.sock`）发送以 `\t` 分隔的纯文本指令，以 `\n` 结尾：
- `status` -> 返回全局状态 JSON（含已读/未读邮件，每封带 `seen` 字段）。
- `body\t<account>\t<folder>\t<uid>` -> 返回指定邮件的正文 JSON（正文为可渲染 HTML），并写入本地磁盘缓存。
- `read\t<account>\t<folder>\t<uid>` -> 标记为已读，将该邮件的 `seen` 置为 true（仍保留在列表中），返回 `{"ok":true}`。
- `read_all\t<account>` -> 标记该账户（account 为空则所有账户）当前全部未读为已读，返回 `{"ok":true,"marked":N}`。
- `reload` / `shutdown` -> 关闭守护进程。

---

## 许可证

[MIT License](LICENSE)

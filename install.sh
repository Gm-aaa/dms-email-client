#!/usr/bin/env bash
#
# DMS Email Client 安装脚本
#
# 默认从 GitHub 最新 Release 下载预编译二进制（用户无需 Rust/cmake）；
# 加 --build 则在本地用 cargo 从源码编译。插件 QML 文件从当前克隆的仓库复制安装。
#
# 安装位置（可用环境变量覆盖）：
#   二进制 → $DMS_EMAIL_BIN_DIR      (默认 ~/.local/bin)
#   插件   → $DMS_EMAIL_PLUGIN_DIR   (默认 ~/.local/share/dms/plugins)/dmsEmailClient
#   仓库   → $DMS_EMAIL_REPO         (默认 Gm-aaa/dms-email-client，用于下载 Release)
#
# 用法：
#   ./install.sh            # 下载 Release 预编译二进制并安装
#   ./install.sh --build    # 本地源码编译再安装（需 Rust + cmake + g++）
#
set -euo pipefail

REPO="${DMS_EMAIL_REPO:-Gm-aaa/dms-email-client}"
BIN_DIR="${DMS_EMAIL_BIN_DIR:-$HOME/.local/bin}"
PLUGIN_ROOT="${DMS_EMAIL_PLUGIN_DIR:-$HOME/.local/share/dms/plugins}"
PLUGIN_DIR="$PLUGIN_ROOT/dmsEmailClient"
ASSET="dms-email-client-x86_64-linux"
BIN_NAME="dms-email-client"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

msg()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "缺少命令：$1"; }

usage() { sed -n '2,/^set -euo/p' "$0" | sed 's/^#\{0,1\} \{0,1\}//; /^set -euo/d'; }

BUILD_FROM_SOURCE=0
for arg in "$@"; do
    case "$arg" in
        --build) BUILD_FROM_SOURCE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数：$arg（--help 查看用法）" ;;
    esac
done

# 插件文件必须来自本地克隆的仓库
[ -d "$SCRIPT_DIR/dmsEmailClient" ] \
    || die "请在克隆的仓库根目录运行本脚本（缺少 dmsEmailClient/ 插件目录）"

SRC_BIN=""
TMP_BIN=""
cleanup() { [ -n "$TMP_BIN" ] && rm -f "$TMP_BIN"; }
trap cleanup EXIT

build_binary() {
    need cargo
    msg "本地编译（cargo build --release，首次会编译内置 CTranslate2，较慢）..."
    ( cd "$SCRIPT_DIR" && cargo build --release )
    # 尊重 .cargo/config.toml 里可能自定义的 target-dir
    local td
    td="$(cd "$SCRIPT_DIR" && cargo metadata --no-deps --format-version 1 2>/dev/null \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    [ -n "$td" ] || td="$SCRIPT_DIR/target"
    SRC_BIN="$td/release/$BIN_NAME"
    [ -f "$SRC_BIN" ] || die "未找到编译产物：$SRC_BIN"
}

download_binary() {
    need curl
    msg "查询 $REPO 最新 Release..."
    local url
    url="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
        | grep -o "https://[^\"]*/$ASSET")" || true
    url="$(printf '%s\n' "$url" | head -1)"
    [ -n "$url" ] || { warn "最新 Release 中未找到资产 $ASSET"; return 1; }
    msg "下载 $url ..."
    TMP_BIN="$(mktemp)"
    curl -fSL --progress-bar "$url" -o "$TMP_BIN" || return 1
    SRC_BIN="$TMP_BIN"
}

if [ "$BUILD_FROM_SOURCE" -eq 1 ]; then
    build_binary
else
    download_binary || die "从 Release 下载失败。可改用源码编译：./install.sh --build（需 Rust + cmake + g++）"
fi

# 安装二进制（移动到 PATH 目录，非软链接）
msg "安装二进制 → $BIN_DIR/$BIN_NAME"
install -Dm755 "$SRC_BIN" "$BIN_DIR/$BIN_NAME"

# 安装插件：仅 plugin.json + *.qml，绝不含二进制
msg "安装插件 → $PLUGIN_DIR"
mkdir -p "$PLUGIN_DIR"
install -m644 "$SCRIPT_DIR/dmsEmailClient/plugin.json" "$PLUGIN_DIR/"
install -m644 "$SCRIPT_DIR/dmsEmailClient/"*.qml "$PLUGIN_DIR/"

# PATH 检查：插件按名字 dms-email-client 查找二进制
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR 不在 PATH 中——插件将找不到二进制。请把它加入 PATH（如在 shell 配置里 export PATH=\"$BIN_DIR:\$PATH\"）。" ;;
esac

msg "安装完成。"
echo "  二进制：$BIN_DIR/$BIN_NAME"
echo "  插件：  $PLUGIN_DIR"
echo "在 DankMaterialShell 中启用 “DMS Email Client” 插件即可（守护进程随插件启用自动启动）。"

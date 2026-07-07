#!/usr/bin/env bash
#
# DMS Email Client 卸载脚本
#
# 移除已安装的二进制与插件，并尽力停掉仍在运行的守护进程。
# 默认保留用户数据（配置/缓存/已下载的翻译模型）；加 --purge 一并删除。
#
# 位置（与 install.sh 相同，可用环境变量覆盖）：
#   二进制 → $DMS_EMAIL_BIN_DIR      (默认 ~/.local/bin)
#   插件   → $DMS_EMAIL_PLUGIN_DIR   (默认 ~/.local/share/dms/plugins)/dmsEmailClient
#
# 用法：
#   ./uninstall.sh            # 移除二进制 + 插件
#   ./uninstall.sh --purge    # 另外删除配置、正文缓存、离线翻译模型
#
set -euo pipefail

BIN_DIR="${DMS_EMAIL_BIN_DIR:-$HOME/.local/bin}"
PLUGIN_ROOT="${DMS_EMAIL_PLUGIN_DIR:-$HOME/.local/share/dms/plugins}"
PLUGIN_DIR="$PLUGIN_ROOT/dmsEmailClient"
BIN_NAME="dms-email-client"

msg()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }

usage() { sed -n '2,/^set -euo/p' "$0" | sed 's/^#\{0,1\} \{0,1\}//; /^set -euo/d'; }

PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数：$arg（--help 查看用法）" ;;
    esac
done

# 尽力停掉仍在运行的守护进程（插件被禁用时 DMS 也会终止它；这里作双保险）
if pkill -f "$BIN_NAME daemon" 2>/dev/null; then
    msg "已停止运行中的守护进程"
fi

if [ -e "$BIN_DIR/$BIN_NAME" ]; then
    msg "移除二进制 $BIN_DIR/$BIN_NAME"
    rm -f "$BIN_DIR/$BIN_NAME"
else
    warn "未发现二进制 $BIN_DIR/$BIN_NAME（可能已删除或安装到别处）"
fi

if [ -d "$PLUGIN_DIR" ]; then
    msg "移除插件 $PLUGIN_DIR"
    rm -rf "$PLUGIN_DIR"
else
    warn "未发现插件目录 $PLUGIN_DIR"
fi

if [ "$PURGE" -eq 1 ]; then
    CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/dms-email-client"
    CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/dms-email-client"
    DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/dms-email-client"
    msg "清除用户数据（--purge）"
    for d in "$CONFIG_DIR" "$CACHE_DIR" "$DATA_DIR"; do
        [ -e "$d" ] && { rm -rf "$d"; echo "  删除 $d"; }
    done
fi

msg "卸载完成。"
echo "记得在 DankMaterialShell 中停用/移除 “DMS Email Client” 插件。"
[ "$PURGE" -eq 0 ] && echo "用户配置与缓存已保留；如需一并删除请加 --purge。"

#!/usr/bin/env bash
# 本机安装：release 构建 + 拷入 PATH 目录。
# 用法：./install.sh [目标目录]   # 目标目录默认 ${XDG_BIN_HOME:-~/.local/bin}
set -euo pipefail

cd "$(dirname "$0")"

BIN=jj-android-device
DEST="${1:-${XDG_BIN_HOME:-$HOME/.local/bin}}"

echo "==> cargo build --release"
cargo build --release

mkdir -p "$DEST"
# 原子替换：cp 到临时文件再 mv 换新 inode。
# 直接 cp 覆盖会原地改写旧二进制，macOS 内核对该 inode 缓存的 code signature
# 与新内容不符 -> 运行即 SIGKILL(137)。mv rename 分配新 inode 可根治。
tmp="$DEST/.$BIN.tmp.$$"
cp "target/release/$BIN" "$tmp"
mv -f "$tmp" "$DEST/$BIN"
echo "==> 已安装：$DEST/$BIN"
"$DEST/$BIN" --version

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "提示：$DEST 不在 PATH 中，请加入后使用，例如：" ;
     echo "      echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.zshrc && source ~/.zshrc" ;;
esac

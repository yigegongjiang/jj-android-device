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
cp "target/release/$BIN" "$DEST/$BIN"
echo "==> 已安装：$DEST/$BIN"
"$DEST/$BIN" --version

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "提示：$DEST 不在 PATH 中，请加入后使用，例如：" ;
     echo "      echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.zshrc && source ~/.zshrc" ;;
esac

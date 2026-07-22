```When Editing
本文档作用: 工程工作流程 (可用工具 / 调试 / 发布); MUST NOT 写工程说明 (→ README.md) / LLM 约束 (→ AGENTS.md)
遵循 AGENTS.md 文档编写规范
- 所有段落均为条件段, 根据工程实际决定保留或删除; 存在即为明确流程, MUST NOT 附加强度标记
- 发布内按顺序编号步骤; 顶部 TL;DR ≤ 5 行; 删除子段后重编号保持连续
- 风险点 / 不可逆操作用 `>` 引用块; 高危操作 MUST 标禁用条件
```

# 可用工具

- `gh` — 已登录（git push）
- `adb` — 本机已装（Homebrew `android-platform-tools` 或 Android SDK Platform Tools）；采集运行时唯一外部依赖，详见 [README.md#使用](./README.md)

# 调试

CLI 单组件，仓库根即 cargo 工程。

- 类型检查：`cargo check`（提交前另跑 `cargo clippy`，零警告）
- 测试：`cargo test`  # 无真机时覆盖去重状态机 / epoch 解析 / 设备选择 / pid 守卫 / PNG 头解析 / 格式化等纯逻辑
- 运行：`cargo run -- <args>`  # 例 `cargo run -- logs --help`；接真机时 `cargo run -- logs -s <serial>` / `cargo run -- screenshot -s <serial>`
- 真机验证：
  - 断线重连：杀掉 `adb logcat` 子进程（`pkill -f 'logcat -b all -v epoch'`）或拔线 → `events.log` 先后出现 `event=disconnect` / `event=reconnect`，业务日志续增；跨边界 `sort session-*.log | uniq -d` 应为空（不倒灌）
  - 优雅退出：Ctrl-C（前台）或 `kill -INT/-TERM <pid>` → `events.log` 记 `event=exit`，`pgrep -f 'logcat -b all -v epoch'` 为空，`session.pid` 被清理
  - 长跑：数小时后 `.heartbeat` mtime 与当前时间差在数秒内
  - 截屏：`cargo run -- screenshot -s <serial>` → `file <落盘 png>` 报 `PNG image data, WxH`，分辨率与摘要一致

# 发布

代码变更完成后立即执行（= 需求交付的最后环节）。本工程无 CI / 无远程部署：发布 = 版本落定 + 本机 Release 构建 + 二进制装入 PATH + push tag 记录版本。交付闸 = `cargo test` 通过 + `cargo build --release` 成功。

## TL;DR

依序执行：

1. 验证：`cargo check && cargo test`
2. 写版本：`Cargo.toml` `[package] version`（单一源）+ `CHANGELOG.md` + `CHANGELOG.dev.md`（与 tag 一致）
3. 本机安装 + 提交：`cargo build --release` → 拷二进制入 PATH → commit + annotated tag + push branch + tag
4. 修上版 bug：amend + 删远程 tag + 重打 + force push

## 1. 验证

`cargo check && cargo test`（任一失败即中止发布）。真机场景验证见 [调试](#调试)。

## 2. 写版本

- 版本号：默认递增 PATCH（第三位）；新功能 → MINOR；不兼容改动 → MAJOR。
- 单一版本源：`Cargo.toml` `[package] version`（单 crate 单字段，无脚本）。
- 同步 `CHANGELOG.md` + `CHANGELOG.dev.md`（与 tag 一致）。

## 3. 本机安装 + 提交

```bash
./install.sh                 # release 构建 + 拷二进制入 PATH（默认 ${XDG_BIN_HOME:-~/.local/bin}；失败即中止）
git add -A
git commit -m "release: vX.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin master
git push origin vX.Y.Z
```

> 首次发布前需配置远程仓库（`git remote add origin <url>`）与个人 PATH 目录（默认 `~/.local/bin/`）。
> 他人拷入的二进制首次运行被 Gatekeeper 拦时，需 `xattr -dr com.apple.quarantine <path>`；本机自 build 无此问题。

## 4. 修上版 bug

上版存在明显 bug 时，amend 修复后重新发布。

> `--force-with-lease` + 删远程 tag 会改写已推送历史；仅在「刚发布、远程未被他人拉取」时使用。

```bash
./install.sh
git commit --amend --no-edit
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin master --force-with-lease
git push origin vX.Y.Z
```

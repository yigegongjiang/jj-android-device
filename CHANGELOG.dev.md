```When Editing
本文档作用: 面向开发者的发版记录; CHANGELOG.md 的超集, 1:1 镜像 + 技术变更子项
遵循 AGENTS.md 文档编写规范
- 每条主项 = CHANGELOG.md 对应条目 (原文), 下方缩进子项承载技术变更
- 子项 MAY 写路径 / 函数 / 机制; ≤ 1 行
```

# Changelog (developer, follow [CHANGELOG.md](./CHANGELOG.md))

## [0.4.0] - 2026-07-22

### Added

- `netwatch` 子命令：实时监控指定应用的网络累计收发字节增量，判断端侧是否收到平台下发（无需 root）
  - `netwatch.rs`：`parse_uid_bytes` 仅累计 `dumpsys netstats detail` 的「UID stats」段 rb/tb（顶格段标题界定，排除 Dev/Xt/UID tag stats 避免重复计数）；`adb.rs` 加 `dumpsys_netstats`/`net_tcp_raw`/`pm_packages_with_uid` 薄封装
  - 采样循环 `SAMPLE_INTERVAL_SECS=2`；rx 单次增量 ≥`BURST_HIGHLIGHT_BYTES=1KiB` 高亮；`tokio::signal::ctrl_c` 触发汇总退出（观测时长 + 期间累计）
- 省略包名时列出「当前有网络连接的应用」交互选择；收到数据（rx 明显跳变）时高亮提示，Ctrl-C 结束并汇总本次观测
  - `established_uids` 解析 `/proc/net/tcp{,6}` state=01 的 uid，`parse_pm_uids` 解析 `pm list packages -U` 建包名↔uid 映射，复用 `device::select_target` 选设备

## [0.3.0] - 2026-07-22

### Added

- `screenshot` 子命令：抓取设备当前屏幕存为 PNG，落 `~/.config/jj-android-device/screenshots/<serial>/`
  - `adb.rs` 加 `screencap_png`（`exec-out screencap -p`，绕 PTY 取裸二进制，不复用做 utf8 trim 的 `run`）；`screenshot.rs` 编排，`png_dimensions` 从 IHDR 解析分辨率
  - 设备选择逻辑 `select_target`/`prompt_choice` 从 `logs.rs` 提到 `device.rs` 跨子命令共享；`session.rs` 抽 `config_root()` + `screenshot_dir()`
- 单设备直抓、多设备交互选择；界面禁止截屏（安全窗口）时明确报错而非落坏文件
  - PNG magic-byte（`\x89PNG\r\n\x1a\n`）校验为捕获路径唯一运行时护栏（`cargo test` 覆盖不到），非 PNG 即 `bail`

## [0.2.0] - 2026-07-22

### Changed

- 直接运行 `jj-android-device` 即开始采集，`logs` 子命令可省略
  - `cli.rs`：`Cli` 加 `#[command(flatten)] logs` + `Option<Command>`，`resolve()` 无子命令时回落 `Command::Logs`

### Removed

- 移除 `--buffer-mib` / `--status-interval` / `--output-dir` 参数，行为改为内部固定（更简洁）
  - `logs.rs` 提常量 `BUFFER_MIB=8` / `STATUS_INTERVAL_SECS=30`，输出根目录固定 `session::default_root()`；清除随之而来的死分支（buffer/心跳/output-dir 的可选逻辑）
- 移除自动生成的 `help` 子命令（仍可用 `-h` / `--help`）
  - `cli.rs`：`#[command(disable_help_subcommand = true)]`

### Fixed

- 修复安装脚本覆盖旧版后二进制运行被系统终止（exit 137）的问题
  - `install.sh`：`cp` 原地覆写复用旧 inode，macOS code-signature 缓存与新内容不符触发 SIGKILL；改 `cp` 临时文件 + `mv -f` 换新 inode

## [0.1.0] - 2026-07-22

### Added

- `logs` 子命令：实时全量采集指定 Android 设备 logcat，逐行落盘全部可读 buffer
  - `adb logcat -b all -v epoch`（epoch 规避时区）；`collector` 读 stdout → 有界 `mpsc` → `sink` 落盘，背压不丢行
- 断线自愈：连接中断自动等待设备回连续采，防倒灌不重复历史行、抖动缓冲不丢重连窗口日志
  - 以已写最后一行 epoch 为 `-T` 起点；`sink::LineRouter` 水位去重（`ts<水位`/`==水位`查同毫秒边界集/`>水位`关闭），`Reconnecting` 标记界定重连去重窗口；会话初 `-G <mib>M` 扩容
- 优雅退出：Ctrl-C / SIGTERM 结束不留残留 `adb` 子进程；同设备同会话单例互斥
  - `kill_on_drop(true)` + `watch` 广播关停，`run()` 开头同步装 SIGINT/SIGTERM handler 关启动窗口；`PidGuard` 写/清 `session.pid` + `libc::kill` 存活探测
- 设备档案 `readme.md`：首次采集前自动记录身份 / 系统 / 硬件 / 网络 / 存储 / logcat 能力等
  - `profile` 经 `getprop`/`dumpsys`/`df` 收集，任一项失败标注「不可用」不阻塞；档案已存在则跳过
- 结构化终端输出：启动摘要 + 周期心跳 + 事件即时行，日志正文全量落盘不刷屏
  - `report` 原子指标 + `interval` 心跳节流刷 `.heartbeat` mtime；事件同写 `events.log` 与 stdout 单行 `key=value`

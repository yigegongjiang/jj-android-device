```When Editing
本文档作用: 工程总览 (价值主张 / 使用 / 架构 / 结构); MUST NOT 写发布流程 (→ workflow.md) / LLM 约束 (→ AGENTS.md)
遵循 AGENTS.md 文档编写规范
- 章节按需增删, 只留项目真有的; 首行一行价值主张, MUST NOT 带 LLM 提示
- 短并列项用表格; 可执行步骤 fenced + `#` 注释同行
- NEVER 写「开发」段 (VibeCoding 不向人类解释 dev 命令)
```

# jj-android-logs

Android 设备实时 logcat 全量采集工具：经 `adb` 逐行落盘指定设备的全部可读 buffer，断线自愈 / 防倒灌 / 优雅退出，供事后离线分析。

## 使用

- 前置：本机装 `adb`；目标设备开 USB/WiFi 调试并授权，`adb devices` 显示 `device`。
- 安装：`cargo build --release` 产出单个静态二进制 → 拷入个人 PATH（例 `~/bin/`）；升级即替换该文件。他人机器传入的二进制首次运行可能被 Gatekeeper 拦，需清 quarantine 或签名/公证。
- 运行：启动时枚举 `adb devices` 交互选择目标（单台可默认直采）；也可参数/环境变量指定序列号跳过交互。每进程只采一台，并采多台则多次启动。
- 形态：CLI 前台阻塞运行，Ctrl-C 结束；终端只输出 3 类结构化行——启动摘要（版本 / pid / 设备身份 / buffer 扩容结果 / 产物路径）、周期心跳（默认 30s：运行时长·累计行数·字节·速率·最近时间戳·状态·断线/重连次数，`--status-interval` 可调，`0` 关闭）、事件即时行（断线原因 / 重连 / 扩容失败 / 优雅退出），MUST NOT 刷屏 logcat 正文。需常驻由用户自行 `nohup ... &` / `screen` / `tmux`。
- 产物：写入 `~/.config/jj-android-logs/<serial>/`，按设备与会话隔离——会话日志 / 事件日志 / 心跳文件 / 设备档案 `readme.md`；实时逐行追加，用户自行用 `tail`/`rg` 查看。本工具不内建查看界面。

## 架构

- 语言 Rust（stable, 2021 edition+）+ 异步运行时 tokio，统一处理子进程 IO / 定时器 / 信号 / 内部通道。
- `tokio::process` 拉起 `adb logcat`（`kill_on_drop` 杜绝退出残留）；stdout 走业务落盘，stderr 分类断连原因驱动退避。
- 外层守护 loop 断线重连，每次重启 logcat 前重算 `-T` 时间戳绕过 buffer 历史行防倒灌；会话初扩大 logcat buffer 作抖动缓冲。
- `enum` 状态机建模 `WaitingDevice` / `Streaming` / `Backoff` / `Draining` / `Stopped`；读写线程间有界 `mpsc` channel 背压保证不丢日志；`interval` 每秒节流刷心跳文件 mtime。
- 唯一外部依赖 = 本机 `adb`；第三方 crate 限主流成熟库（tokio / clap / serde / time / anyhow 等），无运行时环境要求。

## 项目结构

- `Cargo.toml` — crate 定义 + 依赖 + 版本单一源（`[package] version`，待建）
- `src/` — Rust 源码（待建）：守护循环 / 状态机 / adb 子进程管理 / 落盘 + 心跳 / 设备档案生成 / CLI 参数
- 产物输出目录见 [使用](#使用)，运行时生成，不在源码树内

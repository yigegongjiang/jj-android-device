```When Editing
本文档作用: 工程总览 (价值主张 / 使用 / 架构 / 结构); MUST NOT 写发布流程 (→ workflow.md) / LLM 约束 (→ AGENTS.md)
遵循 AGENTS.md 文档编写规范
- 章节按需增删, 只留项目真有的; 首行一行价值主张, MUST NOT 带 LLM 提示
- 短并列项用表格; 可执行步骤 fenced + `#` 注释同行
- NEVER 写「开发」段 (VibeCoding 不向人类解释 dev 命令)
```

# jj-android-device

Android 设备工具集（`adb` 驱动，单个静态二进制，子命令形态）。首个子命令 `logs`：实时全量采集指定设备 logcat，逐行落盘全部可读 buffer，断线自愈 / 防倒灌 / 优雅退出，供事后离线分析。后续扩展 `screen`（截图）等子命令。

## 使用

- 前置：本机装 `adb`；目标设备开 USB/WiFi 调试并授权，`adb devices` 显示 `device`。
- 安装：`cargo build --release` 产出单个静态二进制 `jj-android-device` → 拷入个人 PATH（例 `~/bin/`）；升级即替换该文件。他人机器传入的二进制首次运行可能被 Gatekeeper 拦，需清 quarantine 或签名/公证。
- 命令：`jj-android-device <子命令>`，当前仅 `logs`。`jj-android-device logs --help` 看参数。
- `logs` 运行：启动时枚举 `adb devices`，单台直采、多台交互选择；`-s/--serial` 指定序列号跳过交互。每进程只采一台，并采多台则多次启动。同一设备同一会话仅允许一个进程（pid 单例守卫）。
- `logs` 形态：CLI 前台阻塞运行，Ctrl-C / SIGTERM 优雅结束；终端只输出 3 类结构化行——启动摘要（版本 / pid / 设备身份 / buffer 扩容结果 / 产物路径）、周期心跳（默认 30s：运行时长·累计行数·字节·速率·最近时间戳·状态·断线/重连次数，`--status-interval` 可调，`0` 关闭）、事件即时行（断线原因 / 重连 / 扩容失败 / unauthorized / 优雅退出），MUST NOT 刷屏 logcat 正文。需常驻由用户自行 `nohup ... &` / `screen` / `tmux`。
- `logs` 参数：`--buffer-mib`（会话初各 buffer 扩容目标，默认 8，`0` 不扩容）、`--output-dir`（覆盖默认根目录）。
- 产物：默认写入 `~/.config/jj-android-device/logs/<serial>/`，按设备与会话隔离——会话日志 / 事件日志 / 心跳文件 / 设备档案 `readme.md`；实时逐行追加，用户自行用 `tail`/`rg` 查看。本工具不内建查看界面。

## 架构

- 语言 Rust（stable, 2021 edition+）+ 异步运行时 tokio，统一处理子进程 IO / 定时器 / 信号 / 内部通道。
- CLI 顶层子命令分发（clap derive）；`adb` 封装、设备枚举/选择等为各子命令共享，`logs` 专有逻辑独立成模块。
- `tokio::process` 拉起 `adb logcat -b all -v epoch`（`kill_on_drop` 杜绝退出残留）；stdout 逐行落盘，stderr 分类断连原因驱动退避。
- 外层守护 loop 断线重连：以「已写最后一行的设备 epoch」为 `-T` 起点续采，sink 侧按水位去重（`ts<水位` 丢 / `ts==水位` 查同毫秒边界集去重 / `ts>水位` 关闭去重），既防倒灌又不丢重连窗口日志；会话初扩大 logcat buffer 作抖动缓冲。
- `enum` 状态机建模 `WaitingDevice` / `Streaming` / `Backoff` / `Draining` / `Stopped`；collector→sink 有界 `mpsc` channel 背压保证不丢日志；`interval` 每秒节流刷心跳文件 mtime。
- 唯一外部依赖 = 本机 `adb`；第三方 crate 限主流成熟库（tokio / clap / anyhow / chrono / libc），无运行时环境要求。

## 项目结构

- `Cargo.toml` — crate 定义 + 依赖 + 版本单一源（`[package] version`）
- `src/main.rs` — 入口 + 子命令分发；`src/cli.rs` — 顶层 CLI 与 `logs` 参数
- `src/adb.rs` `src/device.rs` `src/util.rs` — 跨子命令共享：adb 调用封装 / 设备模型与选择 / 格式化
- `src/logs.rs` — `logs` 子命令编排（设备选择→会话准备→档案→采集守护→优雅退出）
- `src/collector.rs` `src/sink.rs` — 采集守护循环 + 断连分类 / 落盘写入 + 去重状态机
- `src/session.rs` `src/profile.rs` `src/report.rs` — 产物路径与 pid 守卫 / 设备档案生成 / 指标·事件·心跳输出
- 产物输出目录见 [使用](#使用)，运行时生成，不在源码树内

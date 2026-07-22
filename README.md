```When Editing
工程总览 (价值主张 / 使用 / 架构 / 结构); MUST NOT 写发布流程 (→ workflow.md) / LLM 约束 (→ AGENTS.md)
遵循 AGENTS.md 文档编写规范; 使用段先给可跑命令再给参数表; NEVER 写「开发」段
```

# jj-android-device

Android 设备工具集（`adb` 驱动，单个静态二进制，子命令形态）。子命令：

- `logs`：实时全量采集设备 logcat 逐行落盘，断线自愈 / 防倒灌 / 优雅退出。
- `screenshot`：抓取设备当前屏幕，PNG 落盘。
- `netwatch`：无 root 实时监控某应用的网络收发字节增量，判断端侧是否收到平台下发。

## 安装

```bash
./install.sh                   # 构建并拷入 ${XDG_BIN_HOME:-~/.local/bin}；升级=重跑
```

前置：本机装 `adb`；设备开调试并授权，`adb devices` 显示 `device`。

## 使用

```bash
jj-android-device                  # 直接跑=默认 logs 子命令；单设备直采，多设备弹交互选择
jj-android-device -s <serial>      # 指定设备
jj-android-device screenshot       # 抓当前屏幕；单设备直抓，多设备弹交互选择
jj-android-device screenshot -s <serial>
jj-android-device netwatch                 # 交互选「当前有网络连接的应用」后开始监控
jj-android-device netwatch <package>       # 直接监控指定应用（包名）；多设备加 -s <serial>
```

- `-s/--serial`（可选）各子命令通用；`netwatch` 另收一个可选位置参数 `<package>`（应用包名）。`logs` 是默认子命令，可省略。
- `logs`：前台运行，`Ctrl-C` 优雅退出；每进程采一台，常驻自行 `nohup` / `tmux`。其余行为内部固定，不暴露参数：buffer 扩容 8MiB、心跳 30s、输出目录 `~/.config/jj-android-device/logs`。
- `screenshot`：一次性抓屏即退出（`exec-out screencap -p` 取原始 PNG，校验 PNG 头再落盘）。
- `netwatch`：前台运行，`Ctrl-C` 结束并打印本次观测汇总。只读系统计数器（`dumpsys netstats` 的 UID 段），不改设备 / 不需 root。省略包名时列出 `/proc/net/tcp` 中有活动连接的应用供选择。行为内部固定，不暴露参数：采样 2s、rx 单次增量 ≥1KiB 高亮为「收到数据」。

产物落 `~/.config/jj-android-device/`：`logs/<serial>/`（会话日志 / 事件日志 / 心跳 / 设备档案，实时追加自行 `tail` / `rg`）、`screenshots/<serial>/screenshot-<时间戳>.png`。`logs` 终端只打启动摘要 / 周期心跳 / 事件行，不刷 logcat 正文；`screenshot` 打一次落盘摘要（路径 / 大小 / 分辨率）。

## 架构

- Rust + tokio；`tokio::process` 拉起 `adb logcat -b all -v epoch`（`kill_on_drop`），stdout 逐行落盘。
- 守护 loop 断线重连：以最后落盘行的设备 epoch 为 `-T` 续采，sink 按水位去重（既防倒灌又不丢重连窗口）。
- 状态机 `WaitingDevice/Streaming/Backoff/Draining/Stopped`；collector→sink 有界 `mpsc` 背压不丢日志。
- 唯一外部依赖 = 本机 `adb`；crate 限 tokio / clap / anyhow / chrono / libc。

## 项目结构

- `src/main.rs` `src/cli.rs` — 入口 + 子命令分发 / CLI 与各子命令参数
- `src/adb.rs` `src/device.rs` `src/util.rs` — 跨子命令共享：adb 封装（含 `screencap`）/ 设备枚举·选择 / 格式化
- `src/logs.rs` — `logs` 编排（选设备→备会话→档案→采集守护→退出）
- `src/screenshot.rs` — `screenshot` 编排（选设备→抓屏→校验 PNG→落盘→摘要）
- `src/netwatch.rs` — `netwatch` 编排（选设备→解析目标 uid→采样 netstats→打印收发增量），含 netstats / `/proc/net/tcp` / `pm` 解析纯函数
- `src/collector.rs` `src/sink.rs` — 采集守护 + 断连分类 / 落盘 + 去重状态机
- `src/session.rs` `src/profile.rs` `src/report.rs` — 产物路径（logs / screenshots）与 pid 守卫 / 设备档案 / 指标·事件·心跳输出

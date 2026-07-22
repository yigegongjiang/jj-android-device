```When Editing
工程总览 (价值主张 / 使用 / 架构 / 结构); MUST NOT 写发布流程 (→ workflow.md) / LLM 约束 (→ AGENTS.md)
遵循 AGENTS.md 文档编写规范; 使用段先给可跑命令再给参数表; NEVER 写「开发」段
```

# jj-android-device

Android 设备工具集（`adb` 驱动，单个静态二进制，子命令形态）。当前子命令 `logs`：实时全量采集设备 logcat 逐行落盘，断线自愈 / 防倒灌 / 优雅退出。

## 安装

```bash
./install.sh                   # 构建并拷入 ${XDG_BIN_HOME:-~/.local/bin}；升级=重跑
```

前置：本机装 `adb`；设备开调试并授权，`adb devices` 显示 `device`。

## 使用

```bash
jj-android-device logs                 # 单设备直采；多设备弹交互选择
jj-android-device logs -s <serial>     # 指定设备
```

前台运行，`Ctrl-C` 优雅退出；每进程采一台，常驻自行 `nohup` / `tmux`。

<!-- prettier-ignore -->
| 参数 | 默认 | 说明 |
|---|---|---|
| `-s, --serial <SERIAL>` | 交互选择 | 目标设备序列号 |
| `--buffer-mib <MiB>` | `8` | 会话初扩容各 logcat buffer；`0` 不扩容 |
| `--status-interval <SEC>` | `30` | 心跳输出间隔秒；`0` 关闭 |
| `--output-dir <DIR>` | `~/.config/jj-android-device/logs` | 输出根目录 |

产物落 `<output-dir>/<serial>/`（会话日志 / 事件日志 / 心跳 / 设备档案），实时追加自行 `tail` / `rg`。终端只打启动摘要 / 周期心跳 / 事件行，不刷 logcat 正文。

## 架构

- Rust + tokio；`tokio::process` 拉起 `adb logcat -b all -v epoch`（`kill_on_drop`），stdout 逐行落盘。
- 守护 loop 断线重连：以最后落盘行的设备 epoch 为 `-T` 续采，sink 按水位去重（既防倒灌又不丢重连窗口）。
- 状态机 `WaitingDevice/Streaming/Backoff/Draining/Stopped`；collector→sink 有界 `mpsc` 背压不丢日志。
- 唯一外部依赖 = 本机 `adb`；crate 限 tokio / clap / anyhow / chrono / libc。

## 项目结构

- `src/main.rs` `src/cli.rs` — 入口 + 子命令分发 / CLI 与 `logs` 参数
- `src/adb.rs` `src/device.rs` `src/util.rs` — 跨子命令共享：adb 封装 / 设备选择 / 格式化
- `src/logs.rs` — `logs` 编排（选设备→备会话→档案→采集守护→退出）
- `src/collector.rs` `src/sink.rs` — 采集守护 + 断连分类 / 落盘 + 去重状态机
- `src/session.rs` `src/profile.rs` `src/report.rs` — 产物路径与 pid 守卫 / 设备档案 / 指标·事件·心跳输出

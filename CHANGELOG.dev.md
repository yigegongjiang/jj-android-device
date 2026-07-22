```When Editing
本文档作用: 面向开发者的发版记录; CHANGELOG.md 的超集, 1:1 镜像 + 技术变更子项
遵循 AGENTS.md 文档编写规范
- 每条主项 = CHANGELOG.md 对应条目 (原文), 下方缩进子项承载技术变更
- 子项 MAY 写路径 / 函数 / 机制; ≤ 1 行
```

# Changelog (developer, follow [CHANGELOG.md](./CHANGELOG.md))

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

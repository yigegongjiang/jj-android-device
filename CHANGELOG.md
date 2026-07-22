```When Editing
本文档作用: 面向使用者的发版记录; 只写用户感受得到的变化, MUST NOT 写技术细节 (→ CHANGELOG.dev.md)
遵循 AGENTS.md 文档编写规范
- 写: 新功能 / 行为修复 / 体验 / 安全 / 命令迁移
- MUST NOT 写: 文件路径 / 函数名 / 组件名 / 依赖包名 / 重构细节
- 单条 ≤ 2 行, 单版本 ≤ 5 条; 段落: Added / Changed / Fixed / Removed / Security
- 无用户可感知变化 → 占位: `跟随版本同步发布`
```

# Changelog

[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) + [SemVer](https://semver.org/).

## [0.3.0] - 2026-07-22

### Added

- `screenshot` 子命令：抓取设备当前屏幕存为 PNG，落 `~/.config/jj-android-device/screenshots/<serial>/`
- 单设备直抓、多设备交互选择；界面禁止截屏（安全窗口）时明确报错而非落坏文件

## [0.2.0] - 2026-07-22

### Changed

- 直接运行 `jj-android-device` 即开始采集，`logs` 子命令可省略

### Removed

- 移除 `--buffer-mib` / `--status-interval` / `--output-dir` 参数，行为改为内部固定（更简洁）
- 移除自动生成的 `help` 子命令（仍可用 `-h` / `--help`）

### Fixed

- 修复安装脚本覆盖旧版后二进制运行被系统终止（exit 137）的问题

## [0.1.0] - 2026-07-22

### Added

- `logs` 子命令：实时全量采集指定 Android 设备 logcat，逐行落盘全部可读 buffer
- 断线自愈：连接中断自动等待设备回连续采，防倒灌不重复历史行、抖动缓冲不丢重连窗口日志
- 优雅退出：Ctrl-C / SIGTERM 结束不留残留 `adb` 子进程；同设备同会话单例互斥
- 设备档案 `readme.md`：首次采集前自动记录身份 / 系统 / 硬件 / 网络 / 存储 / logcat 能力等
- 结构化终端输出：启动摘要 + 周期心跳 + 事件即时行，日志正文全量落盘不刷屏

//! 命令行接口：顶层 `jj-android-device` + 子命令。
//!
//! `logs` 为默认子命令：省略子命令直接跑 `jj-android-device` 等价于 `jj-android-device logs`。
//! `screenshot` 为一次性截屏子命令。后续可平滑新增更多子命令。

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "jj-android-device", version, about, long_about = None, disable_help_subcommand = true)]
pub struct Cli {
    /// 省略子命令时，此 serial 即传给默认的 `logs`
    #[arg(short = 's', long = "serial")]
    pub serial: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// 归一化为待执行的子命令参数：无子命令时回落到默认的 `logs`。
    pub fn resolve(self) -> Command {
        self.command
            .unwrap_or(Command::Logs(LogsArgs { serial: self.serial, action: None }))
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 实时全量采集指定 Android 设备的 logcat（断线自愈 / 防倒灌 / 优雅退出）
    Logs(LogsArgs),
    /// 抓取指定 Android 设备当前屏幕，PNG 落盘到 `~/.config`
    Screenshot(ScreenshotArgs),
    /// 实时监控某应用网络累计收发字节增量（无 root），判断端侧是否收到下发
    Netwatch(NetwatchArgs),
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// 目标设备序列号；省略时单设备直采、多设备交互选择
    #[arg(short = 's', long = "serial")]
    pub serial: Option<String>,
    /// 省略时执行采集；`open` 用 macOS 默认应用打开最新日志文件
    #[command(subcommand)]
    pub action: Option<LogsAction>,
}

#[derive(Subcommand, Debug)]
pub enum LogsAction {
    /// 用 macOS 默认应用打开最新的会话日志文件
    Open,
}

#[derive(Args, Debug)]
pub struct ScreenshotArgs {
    /// 目标设备序列号；省略时单设备直抓、多设备交互选择
    #[arg(short = 's', long = "serial")]
    pub serial: Option<String>,
}

#[derive(Args, Debug)]
pub struct NetwatchArgs {
    /// 目标应用包名；省略时从「当前有网络连接的应用」中交互选择
    pub package: Option<String>,
    /// 目标设备序列号；省略时单设备直用、多设备交互选择
    #[arg(short = 's', long = "serial")]
    pub serial: Option<String>,
}

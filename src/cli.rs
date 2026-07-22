//! 命令行接口：顶层 `jj-android-device` + 子命令。
//!
//! `logs` 为默认子命令：省略子命令直接跑 `jj-android-device` 等价于 `jj-android-device logs`。
//! 后续可平滑新增 `screen` 等子命令。

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "jj-android-device", version, about, long_about = None, disable_help_subcommand = true)]
pub struct Cli {
    /// 省略子命令时，此处参数即传给默认的 `logs`
    #[command(flatten)]
    pub logs: LogsArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// 归一化为待执行的子命令参数：无子命令时回落到默认的 `logs`。
    pub fn resolve(self) -> Command {
        self.command.unwrap_or(Command::Logs(self.logs))
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 实时全量采集指定 Android 设备的 logcat（断线自愈 / 防倒灌 / 优雅退出）
    Logs(LogsArgs),
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// 目标设备序列号；省略时单设备直采、多设备交互选择
    #[arg(short = 's', long = "serial")]
    pub serial: Option<String>,
}

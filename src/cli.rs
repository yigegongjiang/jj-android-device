//! 命令行接口：顶层 `jj-android-device` + 子命令。
//!
//! 当前仅 `logs`（实时采集）；后续可平滑新增 `screen` 等子命令。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "jj-android-device", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
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

    /// 周期心跳输出间隔（秒），0 关闭周期心跳
    #[arg(long, default_value_t = 30)]
    pub status_interval: u64,

    /// 会话开始时将各 logcat buffer 扩容到该大小（MiB），0 表示不扩容
    #[arg(long, default_value_t = 8)]
    pub buffer_mib: u32,

    /// 输出根目录（默认 ~/.config/jj-android-device/logs）
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
}

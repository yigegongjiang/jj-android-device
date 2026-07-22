//! jj-android-device：Android 设备工具集入口。
//!
//! 顶层为子命令分发；当前提供 `logs`（实时全量采集 logcat），后续可扩展 `screen` 等。

mod adb;
mod cli;
mod collector;
mod device;
mod logs;
mod profile;
mod report;
mod session;
mod sink;
mod util;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.resolve() {
        Command::Logs(args) => logs::run(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

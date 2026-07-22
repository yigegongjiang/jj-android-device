//! jj-android-device：Android 设备工具集入口。
//!
//! 顶层为子命令分发；提供 `logs`（实时全量采集 logcat）、`screenshot`（一次性截屏）、
//! `netwatch`（应用网络收发字节实时监控）与 `procs`（打印活跃进程名），后续可继续扩展。

mod adb;
mod cli;
mod collector;
mod device;
mod logs;
mod netwatch;
mod procmap;
mod procs;
mod profile;
mod report;
mod screenshot;
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
        Command::Logs(args) => match args.action {
            Some(cli::LogsAction::Open) => logs::open().await,
            None => logs::run(args).await,
        },
        Command::Screenshot(args) => screenshot::run(args).await,
        Command::Netwatch(args) => netwatch::run(args).await,
        Command::Procs(args) => procs::run(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

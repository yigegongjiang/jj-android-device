//! `logs` 子命令编排：设备选择 -> 会话准备 -> 档案 -> 启动摘要 -> 采集守护 -> 优雅退出。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, watch};

use crate::cli::LogsArgs;
use crate::device;
use crate::report::{self, EventLog, Metrics, StartupSummary, State};
use crate::session::{self, PidGuard};
use crate::sink::{self, Msg};
use crate::{adb, collector, procmap, profile};

/// collector -> sink 的有界通道容量：足够吸收突发，又对写盘阻塞形成背压。
const CHANNEL_CAP: usize = 16_384;

/// 会话初将各 logcat buffer 扩容到该大小（MiB）。内部固定值，不暴露为参数。
const BUFFER_MIB: u32 = 8;

/// 周期心跳输出间隔（秒）。内部固定值，不暴露为参数。
const STATUS_INTERVAL_SECS: u64 = 30;

pub async fn run(args: LogsArgs) -> Result<()> {
    // 0. 尽早安装关停信号处理器（SIGINT/SIGTERM），关闭「启动窗口内错过信号」的缝隙
    let (sd_tx, sd_rx) = watch::channel(false);
    let sd_tx = Arc::new(sd_tx);
    spawn_signal_watcher(sd_tx.clone())?;

    // 1. 选择目标设备
    let devices = device::list().await.context("枚举 adb 设备失败")?;
    let target = device::select_target(devices, args.serial.as_deref())?;
    let serial = target.serial.clone();

    // 2. 会话产物布局 + pid 单例守卫（输出根目录固定）
    let root = session::default_root()?;
    let sess = session::setup(&root, &serial)?;
    let _pid = PidGuard::acquire(&sess.pid_path)?;

    // 3. 身份信息 + 本机 adb 版本
    let host_adb = adb::host_version().await.unwrap_or_else(|_| "不可用".to_string());
    let id = profile::identity(&serial, &target).await;

    // 4. 扩容 logcat buffer（best-effort）——须在取起点前，扩容可能清空 buffer
    let (expand_ok, expand_note, buffers) = expand_buffer(&serial).await;

    // 5. 取设备当前 epoch 作防倒灌起点（仅采此刻之后）
    let start_ms = adb::device_epoch_ms(&serial)
        .await
        .context("读取设备时钟失败（用于防倒灌起点）")?;

    // 6. 打开产物文件
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sess.log_path)
        .await
        .with_context(|| format!("打开会话日志失败: {}", sess.log_path.display()))?;
    let events_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sess.events_path)
        .await
        .with_context(|| format!("打开事件日志失败: {}", sess.events_path.display()))?;
    let heartbeat = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&sess.heartbeat_path)
        .with_context(|| format!("打开心跳文件失败: {}", sess.heartbeat_path.display()))?;
    let events = Arc::new(EventLog::new(events_file));

    // 7. 设备档案：不存在才生成一次（不覆盖历史手改）
    let readme_new = !sess.readme_path.exists();
    if readme_new {
        if let Err(e) =
            profile::generate(&serial, &target, &id, &buffers, &expand_note, &host_adb, &sess.readme_path).await
        {
            eprintln!("warning: 设备档案生成失败（不影响采集）: {e:#}");
        }
    }

    // 8. 共享状态与通道
    let metrics = Metrics::new(start_ms);
    let (tx, rx) = mpsc::channel::<Msg>(CHANNEL_CAP);

    // 9. 启动摘要（一次性）
    let buffer_target = format!("{BUFFER_MIB}MiB");
    report::print_startup_summary(&StartupSummary {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        start_local: sess.started_local.clone(),
        serial: serial.clone(),
        manufacturer: id.manufacturer.clone(),
        model: id.model.clone(),
        android: id.android_label(),
        api: id.api_level.clone(),
        connection: target.connection_label(),
        buffer_target,
        buffer_result: expand_note.clone(),
        log_path: sess.log_path.display().to_string(),
        events_path: sess.events_path.display().to_string(),
        heartbeat_path: sess.heartbeat_path.display().to_string(),
        readme_path: sess.readme_path.display().to_string(),
        readme_note: if readme_new { "本轮新生成" } else { "已存在，复用" }.to_string(),
    });

    // 10. 起始事件（+ 扩容失败事件，若有）
    events
        .emit("start", &format!("pid={} serial={serial} start_epoch={}", std::process::id(), crate::util::ms_to_epoch_str(start_ms)))
        .await?;
    if !expand_ok {
        events.emit("buffer_expand_failed", &format!("detail={expand_note:?}")).await?;
    }

    // 11. 拉起任务：进程名轮询 / sink / 周期心跳 / 信号
    // procmap 先起,让 sink 落盘时尽早能把 pid 反查为进程名
    let (procmap_rx, procmap_handle) = procmap::spawn(serial.clone(), sd_rx.clone());
    let sink_handle = tokio::spawn(sink::run(rx, log_file, heartbeat, metrics.clone(), procmap_rx));
    let status_handle = tokio::spawn(report::status_task(
        metrics.clone(),
        Duration::from_secs(STATUS_INTERVAL_SECS),
        sd_rx.clone(),
    ));

    // 12. 采集守护主循环（阻塞至收到关停信号）
    let collect_res = collector::run(&serial, tx, events.clone(), metrics.clone(), sd_rx.clone()).await;

    // 13. 广播关停（覆盖 sink 异常关闭导致 collector 返回的情形），排空并汇合
    let _ = sd_tx.send(true);
    let sink_res = sink_handle.await.context("sink 任务 join 失败")?;
    let _ = status_handle.await;
    let _ = procmap_handle.await;
    metrics.set_state(State::Stopped);

    // 14. 退出事件（含最终统计）
    events
        .emit(
            "exit",
            &format!(
                "lines={} bytes={} disconnects={} reconnects={} uptime={}",
                metrics.lines(),
                crate::util::human_bytes(metrics.bytes()),
                metrics.disconnects(),
                metrics.reconnects(),
                crate::util::human_duration(metrics.uptime()),
            ),
        )
        .await?;

    // 汇报底层任务错误（若有），pid 守卫随 `_pid` drop 自动清理
    collect_res.context("采集守护循环出错")?;
    sink_res.context("落盘任务出错")?;
    Ok(())
}

/// 尝试扩容 buffer 并校验实际结果，返回 (是否成功, 说明, 扩容后各 buffer 大小)。
///
/// 关键：`logcat -G` 退出码不代表尺寸真的生效——部分厂商会静默限制。故回读 `-g`
/// 与请求值比对，报告「确认」而非「已请求」，并据此决定是否触发扩容失败事件。
async fn expand_buffer(serial: &str) -> (bool, String, String) {
    let mib = BUFFER_MIB;
    if let Err(e) = adb::set_buffer_size(serial, mib).await {
        let sizes = adb::buffer_sizes(serial).await.unwrap_or_default();
        return (false, format!("失败，降级为不扩容: {e}"), sizes);
    }
    // 回读真实大小并校验
    let sizes = adb::buffer_sizes(serial).await.unwrap_or_default();
    let want = mib as u64 * 1024 * 1024;
    match adb::parse_min_ring_buffer_bytes(&sizes) {
        // 允许 5% 显示取整余量
        Some(min) if min * 100 >= want * 95 => {
            (true, format!("已确认扩容至 ≥{mib}MiB/buffer（实测最小 {}）", crate::util::human_bytes(min)), sizes)
        }
        Some(min) => (
            false,
            format!("部分 buffer 被设备限制：请求 {mib}MiB，实测最小仅 {}", crate::util::human_bytes(min)),
            sizes,
        ),
        None => (true, format!("已请求 {mib}MiB/buffer（无法回读校验实际大小）"), sizes),
    }
}

/// 同步安装关停信号处理器（SIGINT=Ctrl-C / SIGTERM=`nohup` 场景 kill），
/// 收到任一信号即广播 shutdown。信号流在函数返回前创建，OS handler 立即生效，
/// 避免启动阶段（设备查询 / 档案生成）内错过信号。
#[cfg(unix)]
fn spawn_signal_watcher(sd_tx: Arc<watch::Sender<bool>>) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).context("安装 SIGINT 处理器失败")?;
    let mut sigterm = signal(SignalKind::terminate()).context("安装 SIGTERM 处理器失败")?;
    tokio::spawn(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        let _ = sd_tx.send(true);
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_signal_watcher(sd_tx: Arc<watch::Sender<bool>>) -> Result<()> {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sd_tx.send(true);
    });
    Ok(())
}

/// `logs open`：用 macOS 默认应用打开最新的会话日志文件（跨全部设备取最新）。
pub async fn open() -> Result<()> {
    let root = session::default_root()?;
    let path = latest_session_log(&root).ok_or_else(|| {
        anyhow::anyhow!("在 {} 下未找到任何会话日志（先跑一次 logs 采集）", root.display())
    })?;
    let status = tokio::process::Command::new("open")
        .arg(&path)
        .status()
        .await
        .with_context(|| format!("调用 open 打开日志失败: {}", path.display()))?;
    if !status.success() {
        bail!("open 退出码非零（{status}）: {}", path.display());
    }
    println!("opened {}", path.display());
    Ok(())
}

/// 扫描 `<root>/<serial>/` 下所有 `session-<stamp>.log`（排除 `.events.log`），取时间戳
/// 文件名字典序最大者（定宽 `%Y%m%d-%H%M%S`，字典序即时序 = 最新会话）。无则 None。
fn latest_session_log(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None; // (文件名, 全路径)
    for dev in std::fs::read_dir(root).ok()?.flatten() {
        if !dev.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dev.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if !is_session_log(&name) {
                continue;
            }
            if best.as_ref().is_none_or(|(bn, _)| name > *bn) {
                best = Some((name, f.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 主会话日志文件名判定：`session-<stamp>.log`，排除 `.events.log` 旁文件与 pid/readme。
fn is_session_log(name: &str) -> bool {
    name.starts_with("session-") && name.ends_with(".log") && !name.ends_with(".events.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_log_filter() {
        assert!(is_session_log("session-20260722-141100.log"));
        assert!(!is_session_log("session-20260722-141100.events.log"));
        assert!(!is_session_log("session.pid"));
        assert!(!is_session_log("readme.md"));
    }

    #[test]
    fn latest_picks_newest_across_devices() {
        let root = std::env::temp_dir().join(format!("jjad-open-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b) = (root.join("devA"), root.join("devB"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("session-20260722-100000.log"), "").unwrap();
        std::fs::write(a.join("session-20260722-100000.events.log"), "").unwrap(); // 应被排除
        std::fs::write(b.join("session-20260722-235959.log"), "").unwrap(); // 最新
        let got = latest_session_log(&root).unwrap();
        assert_eq!(got.file_name().unwrap(), "session-20260722-235959.log");
        let _ = std::fs::remove_dir_all(&root);
    }
}

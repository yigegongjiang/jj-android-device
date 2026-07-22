//! `logs` 子命令编排：设备选择 -> 会话准备 -> 档案 -> 启动摘要 -> 采集守护 -> 优雅退出。

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, watch};

use crate::cli::LogsArgs;
use crate::device::{self, Device, Selection};
use crate::report::{self, EventLog, Metrics, StartupSummary, State};
use crate::session::{self, PidGuard};
use crate::sink::{self, Msg};
use crate::{adb, collector, profile};

/// collector -> sink 的有界通道容量：足够吸收突发，又对写盘阻塞形成背压。
const CHANNEL_CAP: usize = 16_384;

pub async fn run(args: LogsArgs) -> Result<()> {
    // 0. 尽早安装关停信号处理器（SIGINT/SIGTERM），关闭「启动窗口内错过信号」的缝隙
    let (sd_tx, sd_rx) = watch::channel(false);
    let sd_tx = Arc::new(sd_tx);
    spawn_signal_watcher(sd_tx.clone())?;

    // 1. 选择目标设备
    let devices = device::list().await.context("枚举 adb 设备失败")?;
    let target = select_device(devices, args.serial.as_deref())?;
    let serial = target.serial.clone();

    // 2. 会话产物布局 + pid 单例守卫
    let root = match &args.output_dir {
        Some(p) => p.clone(),
        None => session::default_root()?,
    };
    let sess = session::setup(&root, &serial)?;
    let _pid = PidGuard::acquire(&sess.pid_path)?;

    // 3. 身份信息 + 本机 adb 版本
    let host_adb = adb::host_version().await.unwrap_or_else(|_| "不可用".to_string());
    let id = profile::identity(&serial, &target).await;

    // 4. 扩容 logcat buffer（best-effort）——须在取起点前，扩容可能清空 buffer
    let (expand_ok, expand_note, buffers) = expand_buffer(&serial, args.buffer_mib).await;

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
    let buffer_target = if args.buffer_mib == 0 {
        "不扩容".to_string()
    } else {
        format!("{}MiB", args.buffer_mib)
    };
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

    // 11. 拉起任务：sink / 周期心跳 / 信号
    let sink_handle = tokio::spawn(sink::run(rx, log_file, heartbeat, metrics.clone()));
    let status_handle = if args.status_interval > 0 {
        Some(tokio::spawn(report::status_task(
            metrics.clone(),
            Duration::from_secs(args.status_interval),
            sd_rx.clone(),
        )))
    } else {
        None
    };

    // 12. 采集守护主循环（阻塞至收到关停信号）
    let collect_res = collector::run(&serial, tx, events.clone(), metrics.clone(), sd_rx.clone()).await;

    // 13. 广播关停（覆盖 sink 异常关闭导致 collector 返回的情形），排空并汇合
    let _ = sd_tx.send(true);
    let sink_res = sink_handle.await.context("sink 任务 join 失败")?;
    if let Some(h) = status_handle {
        let _ = h.await;
    }
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
async fn expand_buffer(serial: &str, mib: u32) -> (bool, String, String) {
    if mib == 0 {
        let sizes = adb::buffer_sizes(serial).await.unwrap_or_default();
        return (true, "跳过（--buffer-mib 0）".to_string(), sizes);
    }
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

/// 设备选择：单台直采、多台交互挑选、异常给出清晰原因。
fn select_device(devices: Vec<Device>, requested: Option<&str>) -> Result<Device> {
    match device::resolve(devices, requested) {
        Selection::One(d) => Ok(d),
        Selection::Choose(list) => prompt_choice(list),
        Selection::NoneOnline => {
            bail!("未发现处于 device 状态的设备；检查连线与授权后重试（adb devices）")
        }
        Selection::NotFound(s) => bail!("未找到序列号为 {s} 的设备"),
        Selection::Unusable(d) => {
            bail!("设备 {} 当前状态为 {}，无法采集（需在设备端授权，使其显示为 device）", d.serial, d.state)
        }
    }
}

/// 多设备交互挑选（提示走 stderr，保持 stdout 结构化输出洁净）。
fn prompt_choice(list: Vec<Device>) -> Result<Device> {
    eprintln!("检测到多台在线设备，请选择目标：");
    for (i, d) in list.iter().enumerate() {
        eprintln!("  [{}] {} ({}) {}", i + 1, d.serial, d.model_label(), d.connection_label());
    }
    eprint!("输入序号 (1-{}): ", list.len());
    io::stderr().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line).context("读取选择输入失败")?;
    let n: usize = line.trim().parse().context("无效序号")?;
    let idx = n.checked_sub(1).context("序号需从 1 起")?;
    list.get(idx).cloned().context("序号超出范围")
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

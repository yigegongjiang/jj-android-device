//! 可观测性：共享指标、状态机枚举、事件日志、启动摘要、周期心跳。
//!
//! 终端只输出三类结构化行（启动摘要 / 周期心跳 / 事件即时行），MUST NOT 刷屏
//! logcat 正文。事件即时行与 `events.log` 内容一致，均为单行 `key=value`。

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;
use tokio::io::AsyncWriteExt;
use tokio::sync::{watch, Mutex};

use crate::util;

/// 采集会话状态机。转移在 [`crate::collector`] 中覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    WaitingDevice,
    Streaming,
    Backoff,
    Draining,
    Stopped,
}

impl State {
    fn as_u8(self) -> u8 {
        match self {
            State::WaitingDevice => 0,
            State::Streaming => 1,
            State::Backoff => 2,
            State::Draining => 3,
            State::Stopped => 4,
        }
    }
    fn from_u8(v: u8) -> State {
        match v {
            0 => State::WaitingDevice,
            1 => State::Streaming,
            2 => State::Backoff,
            3 => State::Draining,
            _ => State::Stopped,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            State::WaitingDevice => "waiting_device",
            State::Streaming => "streaming",
            State::Backoff => "backoff",
            State::Draining => "draining",
            State::Stopped => "stopped",
        }
    }
}

/// 跨任务共享的采集指标。计数用无锁原子，读写各行其道：
/// sink 更新 lines/bytes/last_log_ms，collector 更新 state/断线·重连计数。
#[derive(Debug)]
pub struct Metrics {
    lines: AtomicU64,
    bytes: AtomicU64,
    disconnects: AtomicU64,
    reconnects: AtomicU64,
    /// 最近一行日志的设备 epoch（毫秒），-1 表示尚无
    last_log_ms: AtomicI64,
    state: AtomicU8,
    start: Instant,
}

impl Metrics {
    pub fn new(start_ms: i64) -> Arc<Self> {
        Arc::new(Metrics {
            lines: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            disconnects: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            last_log_ms: AtomicI64::new(start_ms),
            state: AtomicU8::new(State::Streaming.as_u8()),
            start: Instant::now(),
        })
    }

    /// 记录一行成功落盘的日志（行数 +1，字节累加）。
    pub fn record_line(&self, bytes: usize) {
        self.lines.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn set_last_log_ms(&self, ms: i64) {
        self.last_log_ms.store(ms, Ordering::Relaxed);
    }
    pub fn last_log_ms(&self) -> i64 {
        self.last_log_ms.load(Ordering::Relaxed)
    }
    pub fn inc_disconnect(&self) {
        self.disconnects.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }
    pub fn set_state(&self, s: State) {
        self.state.store(s.as_u8(), Ordering::Relaxed);
    }
    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Relaxed))
    }
    pub fn lines(&self) -> u64 {
        self.lines.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn disconnects(&self) -> u64 {
        self.disconnects.load(Ordering::Relaxed)
    }
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }
    pub fn uptime(&self) -> Duration {
        self.start.elapsed()
    }
}

/// 本地时区 ISO8601 时间戳（事件/心跳行时间前缀）。
fn now_iso() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 事件日志：同一条事件同时写入 `events.log` 与 stdout，单行 `key=value`。
pub struct EventLog {
    file: Mutex<tokio::fs::File>,
}

impl EventLog {
    pub fn new(file: tokio::fs::File) -> Self {
        EventLog { file: Mutex::new(file) }
    }

    /// 追加一条事件。`detail` 已是结构化的 `key=value ...` 片段（可为空）。
    pub async fn emit(&self, event: &str, detail: &str) -> Result<()> {
        let line = if detail.is_empty() {
            format!("ts={} event={}\n", now_iso(), event)
        } else {
            format!("ts={} event={} {}\n", now_iso(), event, detail)
        };
        // 先落盘再回显，保证 events.log 不落后于终端
        {
            let mut f = self.file.lock().await;
            f.write_all(line.as_bytes()).await.context("写入 events.log 失败")?;
            f.flush().await.context("flush events.log 失败")?;
        }
        print!("{line}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        Ok(())
    }
}

/// 一次性启动摘要（多行块）。字段来源见 [`crate::logs`]。
#[allow(clippy::too_many_arguments)]
pub fn print_startup_summary(summary: &StartupSummary) {
    println!("── jj-android-device logs 启动 ──────────────────────────────");
    println!("version={}  pid={}  start={}", summary.version, summary.pid, summary.start_local);
    println!(
        "device serial={} manufacturer={} model={} android={} api={} connection={}",
        summary.serial,
        summary.manufacturer,
        summary.model,
        summary.android,
        summary.api,
        summary.connection,
    );
    println!(
        "logcat buffers=all target_buffer={} expand={}",
        summary.buffer_target, summary.buffer_result,
    );
    println!("session.log     = {}", summary.log_path);
    println!("events.log      = {}", summary.events_path);
    println!("heartbeat       = {}", summary.heartbeat_path);
    println!("readme.md       = {} ({})", summary.readme_path, summary.readme_note);
    println!("──────────────────────────────────────────────────────────");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// 启动摘要字段载体。
pub struct StartupSummary {
    pub version: String,
    pub pid: u32,
    pub start_local: String,
    pub serial: String,
    pub manufacturer: String,
    pub model: String,
    pub android: String,
    pub api: String,
    pub connection: String,
    pub buffer_target: String,
    pub buffer_result: String,
    pub log_path: String,
    pub events_path: String,
    pub heartbeat_path: String,
    pub readme_path: String,
    pub readme_note: String,
}

/// 周期心跳任务：每 `interval` 输出一行运行指标；`interval` 为 0 时不启动（调用方保证）。
pub async fn status_task(
    metrics: Arc<Metrics>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // 立即返回的首 tick，跳过

    let mut prev_lines = metrics.lines();
    let mut prev_bytes = metrics.bytes();
    let mut prev_at = Instant::now();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                let dt = now.duration_since(prev_at).as_secs_f64().max(1e-6);
                let lines = metrics.lines();
                let bytes = metrics.bytes();
                let rate_lines = (lines - prev_lines) as f64 / dt;
                let rate_bytes = (bytes - prev_bytes) as f64 / dt;
                let last_ms = metrics.last_log_ms();
                let last_repr = if last_ms < 0 {
                    "none".to_string()
                } else {
                    util::ms_to_epoch_str(last_ms)
                };
                println!(
                    "ts={} event=heartbeat uptime={} lines={} bytes={} rate_lines={:.1}/s rate_bytes={}/s last_log={} state={} disconnects={} reconnects={}",
                    now_iso(),
                    util::human_duration(metrics.uptime()),
                    lines,
                    util::human_bytes(bytes),
                    rate_lines,
                    util::human_bytes(rate_bytes as u64),
                    last_repr,
                    metrics.state().as_str(),
                    metrics.disconnects(),
                    metrics.reconnects(),
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
                prev_lines = lines;
                prev_bytes = bytes;
                prev_at = now;
            }
            _ = shutdown.changed() => break,
        }
    }
}

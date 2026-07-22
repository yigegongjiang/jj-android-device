//! 采集守护循环：外层驱动断线重连，内层流式读 logcat 并投递给 sink。
//!
//! 状态机转移（[`State`]）：Streaming ⇄ WaitingDevice（断线）→ Backoff（退避轮询）
//! → Streaming（回连重拉），收到关停信号 → Draining → 退出。

use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;

use crate::report::{EventLog, Metrics, State};
use crate::sink::Msg;
use crate::{adb, util};

/// 一次流式采集的结束方式。
enum Outcome {
    /// 收到关停信号，优雅退出
    Shutdown,
    /// logcat 流自然结束（断线 / 设备离线 / 进程退出）
    Ended { token: String, detail: String },
}

/// 采集守护主循环。`shutdown` 转为 true 时优雅退出。
pub async fn run(
    serial: &str,
    tx: Sender<Msg>,
    events: Arc<EventLog>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let since = util::ms_to_logcat_arg(metrics.last_log_ms());

        if !first {
            // 一次成功回连即将重拉：记录重连事件与新的 -T 起点
            metrics.inc_reconnect();
            events
                .emit("reconnect", &format!("since={since}"))
                .await?;
        }
        first = false;

        metrics.set_state(State::Streaming);
        // 通知 sink 进入重连去重模式（首启同样走一次：阈值=起点、boundary 空，不误伤）
        if tx.send(Msg::Reconnecting).await.is_err() {
            break; // sink 已退出
        }

        let child = match adb::spawn_logcat(serial, &since) {
            Ok(c) => c,
            Err(e) => {
                metrics.inc_disconnect();
                events
                    .emit("disconnect", &format!("reason=spawn_failed detail={:?}", e.to_string()))
                    .await?;
                metrics.set_state(State::WaitingDevice);
                if !wait_for_device(serial, &mut shutdown, &events).await? {
                    break;
                }
                continue;
            }
        };

        match stream(child, &tx, &mut shutdown, &metrics).await? {
            Outcome::Shutdown => break,
            Outcome::Ended { token, detail } => {
                metrics.inc_disconnect();
                let d = if detail.is_empty() {
                    format!("reason={token}")
                } else {
                    format!("reason={token} stderr={detail:?}")
                };
                events.emit("disconnect", &d).await?;
                metrics.set_state(State::WaitingDevice);
                if !wait_for_device(serial, &mut shutdown, &events).await? {
                    break;
                }
            }
        }
    }

    metrics.set_state(State::Draining);
    Ok(())
}

/// 阻塞等待目标设备回到 `device` 状态。返回 `Ok(false)` 表示等待期间收到关停信号。
async fn wait_for_device(
    serial: &str,
    shutdown: &mut watch::Receiver<bool>,
    events: &EventLog,
) -> Result<bool> {
    let mut backoff = Duration::from_secs(1);
    let cap = Duration::from_secs(5);
    let mut warned_unauthorized = false;

    loop {
        if *shutdown.borrow() {
            return Ok(false);
        }
        match adb::get_state(serial).await {
            Ok(s) if s == "device" => return Ok(true),
            Ok(s) if s == "unauthorized" && !warned_unauthorized => {
                events.emit("unauthorized", &format!("serial={serial}")).await?;
                warned_unauthorized = true;
            }
            _ => {} // offline / unauthorized(已提示) / not found / 查询失败：继续退避等待
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return Ok(false),
        }
        backoff = (backoff * 2).min(cap);
    }
}

/// 单次流式读取：stdout 逐行投递 sink，stderr 收集用于断连分类。
async fn stream(
    mut child: Child,
    tx: &Sender<Msg>,
    shutdown: &mut watch::Receiver<bool>,
    _metrics: &Arc<Metrics>,
) -> Result<Outcome> {
    let stdout = child.stdout.take().context("logcat 缺少 stdout 管道")?;
    let stderr = child.stderr.take().context("logcat 缺少 stderr 管道")?;
    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut errbuf = String::new();
    let mut errline = String::new();
    let mut err_done = false;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(Outcome::Shutdown), // child drop 自动 kill
            res = out.read_until(b'\n', &mut buf) => {
                match res {
                    Ok(0) => break, // stdout EOF：流结束
                    Ok(_) => {
                        let line = std::mem::take(&mut buf);
                        // 背压：sink 落盘慢时在此 await，同时保持对关停的响应
                        tokio::select! {
                            biased;
                            _ = shutdown.changed() => return Ok(Outcome::Shutdown),
                            r = tx.send(Msg::Line(line)) => {
                                if r.is_err() {
                                    return Ok(Outcome::Shutdown); // sink 已退出
                                }
                            }
                        }
                    }
                    Err(e) => {
                        errbuf.push_str(&format!("[stdout read error: {e}]"));
                        break;
                    }
                }
            }
            res = err.read_line(&mut errline), if !err_done => {
                match res {
                    Ok(0) => err_done = true, // stderr EOF：停止轮询该分支，避免空转
                    Ok(_) => { errbuf.push_str(&errline); errline.clear(); }
                    Err(_) => err_done = true,
                }
            }
        }
    }

    // stdout 收束后，把 stderr 读到 EOF 再分类：断连原因可能滞后于 stdout 关闭
    let _ = err.read_to_string(&mut errbuf).await;
    let status = child.wait().await.ok();
    let (token, detail) = classify(&errbuf, status);
    Ok(Outcome::Ended { token, detail })
}

/// 依据 stderr 与退出状态归类断连原因，驱动上层事件记录。
fn classify(stderr: &str, status: Option<ExitStatus>) -> (String, String) {
    let low = stderr.to_lowercase();
    let token = if low.contains("device offline") {
        "device_offline"
    } else if low.contains("unauthorized") {
        "unauthorized"
    } else if low.contains("device not found")
        || low.contains("no devices")
        || low.contains("not found")
    {
        "device_not_found"
    } else if low.contains("closed") {
        "connection_closed"
    } else {
        "stream_ended"
    };
    let mut detail = stderr.trim().replace('\n', " ⏎ ");
    if let Some(st) = status {
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str(&format!("[exit: {st}]"));
    }
    (token.to_string(), detail)
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classify_offline() {
        let (t, _) = classify("error: device offline", None);
        assert_eq!(t, "device_offline");
    }

    #[test]
    fn classify_not_found() {
        let (t, _) = classify("error: device 'X' not found", None);
        assert_eq!(t, "device_not_found");
    }

    #[test]
    fn classify_unauthorized() {
        let (t, _) = classify("error: device unauthorized.", None);
        assert_eq!(t, "unauthorized");
    }

    #[test]
    fn classify_fallback() {
        let (t, _) = classify("", None);
        assert_eq!(t, "stream_ended");
    }
}

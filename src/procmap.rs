//! 进程名解析器:周期性 `ps` 快照,维护 `pid -> 进程名` 映射供落盘富化查用。
//!
//! logcat 行只带 pid/tid 数字,无法直读是哪个进程/app。这里用 `adb shell ps` 定期
//! 建映射(watch 快照,读端零竞争),写盘时按 pid 反查进程名。best-effort:任一次
//! 查询失败保留上一份快照,查不到的 pid 由调用方回退显示,绝不阻塞采集。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::adb;

/// 一份 `pid -> 进程名` 快照(不可变,读端克隆 `Arc` 即可)。
pub type Snapshot = Arc<HashMap<u32, String>>;

/// 轮询间隔(秒)。pid 复用窗口 = 该值;取 10s 折中新鲜度与 adb 调用频率。
const POLL_INTERVAL_SECS: u64 = 10;

/// 拉起进程名轮询任务,返回快照接收端与 JoinHandle。
///
/// 立即做一次首轮查询(尽量让最早的日志也能解析),之后每 [`POLL_INTERVAL_SECS`]
/// 刷新一次;收到关停信号即退出。查询失败时不推送,读端沿用上一份快照。
pub fn spawn(
    serial: String,
    mut shutdown: watch::Receiver<bool>,
) -> (watch::Receiver<Snapshot>, JoinHandle<()>) {
    let (tx, rx) = watch::channel::<Snapshot>(Arc::new(HashMap::new()));
    let handle = tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }
            if let Some(out) = adb::shell_opt(&serial, "ps -A -o PID,NAME").await {
                let _ = tx.send(Arc::new(parse_ps(&out)));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)) => {}
                _ = shutdown.changed() => break,
            }
        }
    });
    (rx, handle)
}

/// 解析 `ps -A -o PID,NAME` 输出为 `pid -> 进程名`。
///
/// 跳过表头(含 `PID`);每行首 token 为 pid,其余(trim 后)为进程名。现代 toybox
/// 的 `NAME` 列给完整进程名(含 `:suffix` 多进程后缀),非截断的 15 字符 comm。
fn parse_ps(out: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for line in out.lines() {
        let line = line.trim_start();
        let Some((pid_tok, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_tok.parse::<u32>() else {
            continue; // 表头行 "PID NAME" 或空行
        };
        let name = rest.trim();
        if !name.is_empty() {
            map.insert(pid, name.to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::parse_ps;

    #[test]
    fn parses_pid_name() {
        let out = "\
  PID NAME
    1 init
 1040 android.hardware.audio.service_64
 2815 com.qualcomm.qti.devicestatisticsservice
 3604 com.qualcomm.qti.services.systemhelper:systemhelper_service";
        let m = parse_ps(out);
        assert_eq!(m.get(&1).map(String::as_str), Some("init"));
        assert_eq!(m.get(&1040).map(String::as_str), Some("android.hardware.audio.service_64"));
        assert_eq!(
            m.get(&3604).map(String::as_str),
            Some("com.qualcomm.qti.services.systemhelper:systemhelper_service"),
        );
        assert!(!m.contains_key(&0)); // 表头未被误收
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn skips_garbage_lines() {
        let m = parse_ps("garbage\n\n  \n42 foo");
        assert_eq!(m.get(&42).map(String::as_str), Some("foo"));
        assert_eq!(m.len(), 1);
    }
}

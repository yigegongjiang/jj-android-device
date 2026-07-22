//! 通用格式化辅助：人类可读字节 / 时长 / 设备 epoch 时间戳。

use std::time::Duration;

/// 字节数格式化为人类可读（B / KiB / MiB / GiB）。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// 以 1024 进制的 GiB 呈现（用于设备存储/内存档案），保留两位小数。
pub fn gib(bytes: u64) -> String {
    format!("{:.2}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// 时长格式化为 `HhMMmSSs`（省略前导零单位）。
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m{sec:02}s")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// 设备 epoch 毫秒 -> logcat `-T` 参数字符串 `sec.mmm`。
///
/// 带小数点，确保 logcat 将其解析为「时间」而非「行数」。
pub fn ms_to_logcat_arg(ms: i64) -> String {
    let sec = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    format!("{sec}.{millis:03}")
}

/// 设备 epoch 毫秒 -> 人类可读的 `sec.mmm`（用于状态行/事件展示）。
pub fn ms_to_epoch_str(ms: i64) -> String {
    ms_to_logcat_arg(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
    }

    #[test]
    fn duration_fmt() {
        assert_eq!(human_duration(Duration::from_secs(5)), "5s");
        assert_eq!(human_duration(Duration::from_secs(65)), "1m05s");
        assert_eq!(human_duration(Duration::from_secs(3661)), "1h01m01s");
    }

    #[test]
    fn logcat_arg_roundtrip() {
        assert_eq!(ms_to_logcat_arg(1784689028000), "1784689028.000");
        assert_eq!(ms_to_logcat_arg(1784689028022), "1784689028.022");
        assert_eq!(ms_to_logcat_arg(1784689028205), "1784689028.205");
    }
}

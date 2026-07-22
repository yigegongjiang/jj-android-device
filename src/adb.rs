//! `adb` 命令封装：设备枚举之外的一次性查询、设备时钟、buffer 扩容、logcat 拉起。
//!
//! 采集运行时唯一外部依赖即本机 `adb`。所有函数按需容错：查询类失败向上返回
//! `Result`，设备档案等「尽力而为」字段由调用方用 [`shell_opt`] 静默降级。

use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::{Child, Command};

/// 执行一条一次性 adb 命令，返回 stdout（去除首尾空白）。失败（非零退出）报错。
async fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("adb")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("无法执行 adb {}（本机是否已安装 adb？）", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("adb {} 失败: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 本机 adb 版本首行（如 `Android Debug Bridge version 1.0.41`）。
pub async fn host_version() -> Result<String> {
    let v = run(&["version"]).await?;
    Ok(v.lines().next().unwrap_or("").trim().to_string())
}

/// `adb devices -l` 原始输出（供 [`crate::device`] 解析）。
pub async fn devices_raw() -> Result<String> {
    run(&["devices", "-l"]).await
}

/// 目标设备连接状态：`device` / `unauthorized` / `offline` 等；设备不存在时返回 Err。
pub async fn get_state(serial: &str) -> Result<String> {
    run(&["-s", serial, "get-state"]).await
}

/// 在设备上执行 shell 命令，返回 stdout（trim）。失败报错。
pub async fn shell(serial: &str, script: &str) -> Result<String> {
    run(&["-s", serial, "shell", script]).await
}

/// 尽力而为的 shell 查询：任何失败或空输出都返回 `None`，供档案字段静默降级。
pub async fn shell_opt(serial: &str, script: &str) -> Option<String> {
    match shell(serial, script).await {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// 读取单个系统属性（`getprop <key>`），空值/失败返回 None。
pub async fn getprop(serial: &str, key: &str) -> Option<String> {
    shell_opt(serial, &format!("getprop {key}")).await
}

/// 读取设备当前 epoch（毫秒）。用作会话防倒灌起点：只采「此刻之后」的日志。
///
/// 优先 `date +%s.%3N`（毫秒），降级 `date +%s`（秒）。设备时钟与 logcat
/// `-v epoch` 时间戳同源（CLOCK_REALTIME），故比较无时区问题。
pub async fn device_epoch_ms(serial: &str) -> Result<i64> {
    let raw = shell(serial, "date +%s.%3N")
        .await
        .context("读取设备时钟失败")?;
    parse_epoch_ms(&raw).ok_or_else(|| anyhow!("无法解析设备时钟输出: {raw:?}"))
}

/// 解析 `date` 输出的 epoch 字符串为毫秒。支持 `sec` 与 `sec.mmm`（多余位截断/补零）。
fn parse_epoch_ms(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (sec, frac) = match raw.split_once('.') {
        Some((s, f)) => (s, f),
        None => (raw, ""),
    };
    let sec: i64 = sec.parse().ok()?;
    // 取前 3 位作为毫秒，不足补零；`%3N` 未被支持时 frac 可能是字面 `%3N` -> 解析失败降级为 0。
    let mut millis = 0i64;
    for (i, ch) in frac.chars().take(3).enumerate() {
        let d = ch.to_digit(10)?;
        millis += d as i64 * 10i64.pow(2 - i as u32);
    }
    Some(sec * 1000 + millis)
}

/// 尝试把各 logcat buffer 扩容到 `mib` MiB（`logcat -b all -G <mib>M`）。best-effort。
pub async fn set_buffer_size(serial: &str, mib: u32) -> Result<()> {
    let size = format!("{mib}M");
    run(&["-s", serial, "logcat", "-b", "all", "-G", &size]).await?;
    Ok(())
}

/// 读取各 buffer 当前大小（`logcat -b all -g`）。
pub async fn buffer_sizes(serial: &str) -> Result<String> {
    run(&["-s", serial, "logcat", "-b", "all", "-g"]).await
}

/// 解析 `logcat -b all -g` 输出，返回所有 ring buffer 中的最小字节数（用于校验扩容是否真的生效）。
pub fn parse_min_ring_buffer_bytes(g: &str) -> Option<u64> {
    let mut min: Option<u64> = None;
    for line in g.lines() {
        if let Some(size) = line.split("ring buffer is ").nth(1).and_then(parse_size) {
            min = Some(min.map_or(size, |m| m.min(size)));
        }
    }
    min
}

/// 解析形如 `8 MiB (...`、`256 KiB ...` 的开头尺寸为字节。
fn parse_size(s: &str) -> Option<u64> {
    let mut it = s.split_whitespace();
    let num: f64 = it.next()?.parse().ok()?;
    let mult = match it.next()? {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult) as u64)
}

/// 拉起 `adb logcat` 流式子进程。
///
/// - `-b all`：覆盖 main/system/crash/events/radio/security 等全部可读 buffer
/// - `-v epoch`：时间戳输出为绝对 epoch，规避时区、便于 `-T` 精确续采
/// - `-T <since>`：仅取该时刻之后的行，绕过 buffer 历史，防倒灌
/// - `kill_on_drop(true)`：子进程随 [`Child`] drop 自动 kill，杜绝退出残留
pub fn spawn_logcat(serial: &str, since: &str) -> Result<Child> {
    Command::new("adb")
        .args(["-s", serial, "logcat", "-b", "all", "-v", "epoch", "-T", since])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("拉起 adb logcat 失败")
}

#[cfg(test)]
mod tests {
    use super::parse_epoch_ms;

    #[test]
    fn epoch_with_millis() {
        assert_eq!(parse_epoch_ms("1784689029.258"), Some(1784689029258));
    }

    #[test]
    fn epoch_seconds_only() {
        assert_eq!(parse_epoch_ms("1784689029"), Some(1784689029000));
    }

    #[test]
    fn epoch_short_frac_padded() {
        assert_eq!(parse_epoch_ms("100.2"), Some(100200));
        assert_eq!(parse_epoch_ms("100.20"), Some(100200));
    }

    #[test]
    fn epoch_unsupported_format_n() {
        // `%3N` 未被 toybox 支持时可能原样返回，应判定为无法解析
        assert_eq!(parse_epoch_ms("1784689029.%3N"), None);
        assert_eq!(parse_epoch_ms("garbage"), None);
    }

    #[test]
    fn min_ring_buffer_size() {
        let g = "\
main: ring buffer is 8 MiB (2 MiB consumed, 958 KiB readable), max entry is 5120 B
system: ring buffer is 256 KiB (76 KiB consumed), max entry is 5120 B
crash: ring buffer is 8 MiB (0 B consumed), max entry is 5120 B";
        // 最小为 system 的 256 KiB
        assert_eq!(super::parse_min_ring_buffer_bytes(g), Some(256 * 1024));
    }

    #[test]
    fn all_expanded_min() {
        let g = "\
main: ring buffer is 8 MiB (2 MiB consumed)
system: ring buffer is 8 MiB (76 KiB consumed)";
        assert_eq!(super::parse_min_ring_buffer_bytes(g), Some(8 * 1024 * 1024));
    }
}

//! 设备档案 `readme.md` 生成。
//!
//! 选中设备后、正式采集前，若档案不存在则生成一次（已存在跳过，避免覆盖手改）。
//! 目的：事后翻看日志无需再连设备即可对齐上下文。任一字段获取失败标注「不可用」，
//! 绝不阻塞采集。

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::adb;
use crate::device::Device;
use crate::util;

const NA: &str = "不可用";

/// 启动摘要与档案共用的核心身份信息（避免重复 getprop）。
pub struct Identity {
    pub serial: String,
    pub manufacturer: String,
    pub model: String,
    pub brand: String,
    pub product: String,
    pub codename: String,
    pub android_release: String,
    pub api_level: String,
}

impl Identity {
    /// `Android 13 (API 33)` 形式的版本描述。
    pub fn android_label(&self) -> String {
        format!("Android {} (API {})", self.android_release, self.api_level)
    }
}

fn or_na(v: Option<String>) -> String {
    v.filter(|s| !s.is_empty()).unwrap_or_else(|| NA.to_string())
}

/// 采集核心身份信息（少量 getprop）。
pub async fn identity(serial: &str, device: &Device) -> Identity {
    Identity {
        serial: or_na(adb::getprop(serial, "ro.serialno").await).into_or(device.serial.clone()),
        manufacturer: or_na(adb::getprop(serial, "ro.product.manufacturer").await),
        model: or_na(adb::getprop(serial, "ro.product.model").await),
        brand: or_na(adb::getprop(serial, "ro.product.brand").await),
        product: or_na(adb::getprop(serial, "ro.product.name").await),
        codename: or_na(adb::getprop(serial, "ro.product.device").await),
        android_release: or_na(adb::getprop(serial, "ro.build.version.release").await),
        api_level: or_na(adb::getprop(serial, "ro.build.version.sdk").await),
    }
}

/// 小工具：`NA` 时回退到给定默认值。
trait OrInto {
    fn into_or(self, default: String) -> String;
}
impl OrInto for String {
    fn into_or(self, default: String) -> String {
        if self == NA {
            default
        } else {
            self
        }
    }
}

/// 生成设备档案并写入 `path`。`buffers`/`expand_note` 由采集侧提供（扩容后的真实大小）。
pub async fn generate(
    serial: &str,
    device: &Device,
    id: &Identity,
    buffers: &str,
    expand_note: &str,
    host_adb_version: &str,
    path: &Path,
) -> Result<()> {
    let mut md = String::new();
    let _ = writeln!(md, "# 设备档案 · {}", id.model);
    let _ = writeln!(md);
    let _ = writeln!(md, "> 由 jj-android-device 于选中设备、开始采集前自动生成。任一字段不可用标注「{NA}」。");
    let _ = writeln!(md);

    // —— 关键信息（速览） ——
    let cores = cpu_cores(serial).await;
    let primary_abi = or_na(adb::getprop(serial, "ro.product.cpu.abi").await);
    let board = or_na(adb::getprop(serial, "ro.product.board").await);
    let (data_used, data_total, _data_avail) = storage(serial, "/data").await;
    let mem = memory_total(serial).await;
    let (res, density) = screen(serial).await;
    let font_scale = font_scale(serial).await;
    let wifi = wifi_info(serial).await;
    let ip = ip_addr(serial).await.into_or(wifi.ip.clone());

    let _ = writeln!(md, "## 关键信息");
    let _ = writeln!(md);
    let items = [
        ("名称", id.product.clone()),
        ("品牌", id.brand.clone()),
        ("型号", id.model.clone()),
        ("序列号", id.serial.clone()),
        ("Android 版本", id.android_label()),
        ("内核版本", kernel_release(serial).await),
        ("处理器", format!("{board} · {cores} 核 · {primary_abi}")),
        ("存储", format!("{data_used} / {data_total}")),
        ("内存", mem.clone()),
        ("物理分辨率", format!("{res} ({density}dpi)")),
        ("字体缩放", font_scale.clone()),
        ("Wi-Fi", wifi.ssid.clone()),
        ("IP 地址", ip.clone()),
        ("MAC 地址", wifi.mac.clone()),
    ];
    for (k, v) in items {
        let _ = writeln!(md, "- **{k}**：{v}");
    }
    let _ = writeln!(md);

    // —— 身份 ——
    section(&mut md, "身份", &[
        ("序列号", id.serial.clone()),
        ("厂商", id.manufacturer.clone()),
        ("品牌", id.brand.clone()),
        ("型号", id.model.clone()),
        ("产品名", id.product.clone()),
        ("设备代号", id.codename.clone()),
    ]);

    // —— 系统 ——
    section(&mut md, "系统", &[
        ("Android 发行版本", id.android_release.clone()),
        ("API level", id.api_level.clone()),
        ("Build 指纹", or_na(adb::getprop(serial, "ro.build.fingerprint").await)),
        ("Build 类型", or_na(adb::getprop(serial, "ro.build.type").await)),
        ("安全补丁级别", or_na(adb::getprop(serial, "ro.build.version.security_patch").await)),
        ("Build 版本号", or_na(adb::getprop(serial, "ro.build.display.id").await)),
        ("内核版本", or_na(adb::shell_opt(serial, "uname -a").await)),
    ]);

    // —— 硬件 ——
    section(&mut md, "硬件", &[
        ("CPU 架构 (primary ABI)", primary_abi.clone()),
        ("支持的 ABI", or_na(adb::getprop(serial, "ro.product.cpu.abilist").await)),
        ("CPU 核数", cores.clone()),
        ("CPU 最高频率", cpu_max_freq(serial).await),
        ("SoC / board", format!("{} / {}", or_na(adb::getprop(serial, "ro.board.platform").await), board)),
        ("硬件平台", or_na(adb::getprop(serial, "ro.hardware").await)),
        ("总内存", mem),
        ("屏幕分辨率", res),
        ("屏幕密度", format!("{density} dpi")),
        ("字体缩放", font_scale),
        ("电池", battery(serial).await),
    ]);

    // —— 网络 ——
    section(&mut md, "网络", &[
        ("连接方式", device.connection_label()),
        ("Wi-Fi SSID", wifi.ssid),
        ("Wi-Fi BSSID", wifi.bssid),
        ("Wi-Fi MAC", wifi.mac),
        ("当前 IP", ip),
        ("蓝牙 MAC", or_na(adb::shell_opt(serial, "settings get secure bluetooth_address").await)),
    ]);

    // —— 存储 ——
    let (sd_used, sd_total, sd_avail) = storage(serial, "/sdcard").await;
    let (_, _, data_avail) = storage(serial, "/data").await;
    section(&mut md, "存储", &[
        ("/data 已用 / 总量 / 可用", format!("{data_used} / {data_total} / {data_avail}")),
        ("/sdcard 已用 / 总量 / 可用", format!("{sd_used} / {sd_total} / {sd_avail}")),
    ]);

    // —— adb 与调试 ——
    let idline = or_na(adb::shell_opt(serial, "id").await);
    section(&mut md, "adb 与调试", &[
        ("授权状态", device.state.clone()),
        ("是否 root", if idline.contains("uid=0") { "是".into() } else { "否".into() }),
        ("adbd 运行用户", idline),
        ("本机 adb 版本", host_adb_version.to_string()),
    ]);

    // —— logcat 能力 ——
    let _ = writeln!(md, "## logcat 能力");
    let _ = writeln!(md);
    let _ = writeln!(md, "- **扩容结果**：{expand_note}");
    let _ = writeln!(md, "- **各 buffer 当前大小**：");
    let _ = writeln!(md);
    let _ = writeln!(md, "```");
    let _ = writeln!(md, "{}", buffers.trim());
    let _ = writeln!(md, "```");
    let _ = writeln!(md);

    // —— 时区与时间 ——
    let tz = or_na(adb::getprop(serial, "persist.sys.timezone").await);
    let dev_date = or_na(adb::shell_opt(serial, "date").await);
    let diff = time_diff(serial).await;
    section(&mut md, "时区与时间", &[
        ("设备时区", tz),
        ("设备当前时间", dev_date),
        ("设备与本机时间差", diff),
    ]);

    // —— 原始快照（兜底） ——
    let _ = writeln!(md, "## 原始快照");
    let _ = writeln!(md);
    raw_block(&mut md, "getprop", adb::shell_opt(serial, "getprop").await).await;
    raw_block(&mut md, "dumpsys battery", adb::shell_opt(serial, "dumpsys battery").await).await;
    raw_block(&mut md, "df -h", adb::shell_opt(serial, "df -h").await).await;
    raw_block(&mut md, "ip addr", adb::shell_opt(serial, "ip addr").await).await;

    std::fs::write(path, md).with_context(|| format!("写入设备档案失败: {}", path.display()))?;
    Ok(())
}

fn section(md: &mut String, title: &str, rows: &[(&str, String)]) {
    let _ = writeln!(md, "## {title}");
    let _ = writeln!(md);
    for (k, v) in rows {
        let _ = writeln!(md, "- **{k}**：{v}");
    }
    let _ = writeln!(md);
}

async fn raw_block(md: &mut String, title: &str, body: Option<String>) {
    let _ = writeln!(md, "### {title}");
    let _ = writeln!(md);
    let _ = writeln!(md, "```");
    let _ = writeln!(md, "{}", body.unwrap_or_else(|| NA.to_string()));
    let _ = writeln!(md, "```");
    let _ = writeln!(md);
}

// ——— 单项采集与格式化 ———

async fn kernel_release(serial: &str) -> String {
    or_na(adb::shell_opt(serial, "uname -r").await)
}

async fn cpu_cores(serial: &str) -> String {
    or_na(adb::shell_opt(serial, "grep -c ^processor /proc/cpuinfo").await)
}

async fn cpu_max_freq(serial: &str) -> String {
    match adb::shell_opt(serial, "cat /sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").await {
        Some(khz) => match khz.trim().parse::<f64>() {
            Ok(k) => format!("{:.2} GHz", k / 1_000_000.0),
            Err(_) => NA.to_string(),
        },
        None => NA.to_string(),
    }
}

async fn memory_total(serial: &str) -> String {
    match adb::shell_opt(serial, "grep MemTotal /proc/meminfo").await {
        Some(line) => line
            .split_whitespace()
            .nth(1)
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| util::gib(kb * 1024))
            .unwrap_or_else(|| NA.to_string()),
        None => NA.to_string(),
    }
}

/// 返回 (已用, 总量, 可用) 的 GiB 字符串。df 默认 1K 块。
async fn storage(serial: &str, mount: &str) -> (String, String, String) {
    let na = || (NA.to_string(), NA.to_string(), NA.to_string());
    let Some(out) = adb::shell_opt(serial, &format!("df {mount} | tail -1")).await else {
        return na();
    };
    let f: Vec<&str> = out.split_whitespace().collect();
    // Filesystem 1K-blocks Used Available Use% Mounted
    if f.len() >= 4 {
        let g = |i: usize| f[i].parse::<u64>().ok().map(|k| util::gib(k * 1024));
        if let (Some(total), Some(used), Some(avail)) = (g(1), g(2), g(3)) {
            return (used, total, avail);
        }
    }
    na()
}

async fn screen(serial: &str) -> (String, String) {
    let size = adb::shell_opt(serial, "wm size").await;
    let dens = adb::shell_opt(serial, "wm density").await;
    // wm size 输出可能含 "Physical size: 720x1600" 与 "Override size: ..."，取最后一个尺寸
    let res = size
        .and_then(|s| {
            s.lines()
                .filter_map(|l| l.rsplit_once(':').map(|(_, v)| v.trim().to_string()))
                .next_back()
        })
        .unwrap_or_else(|| NA.to_string());
    let density = dens
        .and_then(|s| {
            s.lines()
                .filter_map(|l| l.rsplit_once(':').map(|(_, v)| v.trim().to_string()))
                .next_back()
        })
        .unwrap_or_else(|| NA.to_string());
    (res, density)
}

async fn font_scale(serial: &str) -> String {
    match adb::shell_opt(serial, "settings get system font_scale").await {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(f) if f.fract() == 0.0 => format!("{}x", f as i64),
            Ok(f) => format!("{f}x"),
            Err(_) => NA.to_string(),
        },
        None => NA.to_string(),
    }
}

struct Wifi {
    ssid: String,
    bssid: String,
    mac: String,
    ip: String,
}

/// 从 `dumpsys wifi` 的 `mWifiInfo` 行提取 SSID / BSSID / MAC / IP。
async fn wifi_info(serial: &str) -> Wifi {
    let na = Wifi {
        ssid: NA.to_string(),
        bssid: NA.to_string(),
        mac: NA.to_string(),
        ip: NA.to_string(),
    };
    let Some(out) = adb::shell_opt(serial, "dumpsys wifi | grep mWifiInfo").await else {
        return na;
    };
    let Some(line) = out.lines().find(|l| l.contains("SSID:")) else {
        return na;
    };
    Wifi {
        ssid: extract_field(line, "SSID:").map(|s| s.trim_matches('"').to_string()).unwrap_or_else(|| NA.to_string()),
        bssid: extract_field(line, "BSSID:").unwrap_or_else(|| NA.to_string()),
        mac: extract_field(line, "MAC:").unwrap_or_else(|| NA.to_string()),
        ip: extract_field(line, "IP:").map(|s| s.trim_start_matches('/').to_string()).unwrap_or_else(|| NA.to_string()),
    }
}

/// 从 `key: value,` 形式的行里提取字段（到下一个逗号为止）。
fn extract_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = line[start..].trim_start();
    let end = rest.find(',').unwrap_or(rest.len());
    let v = rest[..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

async fn ip_addr(serial: &str) -> String {
    match adb::shell_opt(serial, "ip -f inet addr show wlan0").await {
        Some(out) => out
            .lines()
            .find_map(|l| {
                let l = l.trim();
                l.strip_prefix("inet ").map(|r| r.split('/').next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| NA.to_string()),
        None => NA.to_string(),
    }
}

/// 电池摘要：电量 / 状态 / 健康 / 温度 / 电压 / 材质。
async fn battery(serial: &str) -> String {
    let Some(out) = adb::shell_opt(serial, "dumpsys battery").await else {
        return NA.to_string();
    };
    let get = |key: &str| -> Option<String> {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim_start_matches(':').trim().to_string()))
    };
    let level = get("level").unwrap_or_default();
    let status = get("status").map(|s| battery_status(&s)).unwrap_or_default();
    let health = get("health").map(|s| battery_health(&s)).unwrap_or_default();
    let temp = get("temperature")
        .and_then(|t| t.parse::<f64>().ok())
        .map(|t| format!("{:.1}°C", t / 10.0))
        .unwrap_or_default();
    let volt = get("voltage").map(|v| format!("{v} mV")).unwrap_or_default();
    let tech = get("technology").unwrap_or_default();
    let parts: Vec<String> = [
        (!level.is_empty()).then(|| format!("{level}%")),
        (!status.is_empty()).then_some(status),
        (!health.is_empty()).then_some(health),
        (!temp.is_empty()).then_some(temp),
        (!volt.is_empty()).then_some(volt),
        (!tech.is_empty()).then_some(tech),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        NA.to_string()
    } else {
        parts.join(" / ")
    }
}

fn battery_status(s: &str) -> String {
    match s {
        "1" => "未知",
        "2" => "充电中",
        "3" => "放电中",
        "4" => "未充电",
        "5" => "已充满",
        other => other,
    }
    .to_string()
}

fn battery_health(s: &str) -> String {
    match s {
        "1" => "未知",
        "2" => "良好",
        "3" => "过热",
        "4" => "损坏",
        "5" => "过压",
        "6" => "未知故障",
        "7" => "过冷",
        other => other,
    }
    .to_string()
}

/// 设备时钟与本机时钟差（秒，带符号，正=本机快）。
async fn time_diff(serial: &str) -> String {
    match adb::device_epoch_ms(serial).await {
        Ok(dev_ms) => {
            let host_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            format!("{:+.2} 秒", (host_ms - dev_ms) as f64 / 1000.0)
        }
        Err(_) => NA.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_wifi_fields() {
        let line = r#"mWifiInfo SSID: "RUxFU1RZTEU", BSSID: d8:b3:70:1d:d5:bc, MAC: 68:50:8c:3c:ed:8f, IP: /10.0.3.26, Security type: 4"#;
        assert_eq!(extract_field(line, "SSID:").unwrap().trim_matches('"'), "RUxFU1RZTEU");
        assert_eq!(extract_field(line, "BSSID:").unwrap(), "d8:b3:70:1d:d5:bc");
        assert_eq!(extract_field(line, "MAC:").unwrap(), "68:50:8c:3c:ed:8f");
        assert_eq!(extract_field(line, "IP:").unwrap().trim_start_matches('/'), "10.0.3.26");
    }

    #[test]
    fn battery_decode() {
        assert_eq!(battery_status("2"), "充电中");
        assert_eq!(battery_health("2"), "良好");
    }
}

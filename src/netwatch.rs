//! `netwatch` 子命令：无 root 实时监控某应用的网络收发字节增量。
//!
//! 读取 Android 为每个 uid 维护的累计收发字节（`dumpsys netstats detail` 的
//! 「UID stats」段），按固定间隔采样并打印增量。数字跳变 = 端侧确有网络收发；
//! 用于「在 Partner 平台点下发后，判断目标应用是否真的从网络收到数据」——不解密
//! 内容、不依赖应用自身日志（生产版常剥离/门控 Log）、且应用无法伪造接收字节。
//!
//! 仅读系统计数器，不改设备、不需 root。Ctrl-C 优雅结束并打印本次观测汇总。
//! 输出同步双写：终端实时显示 + 落盘 `~/.config/jj-android-device/netwatch/<serial>/netwatch-<stamp>.log`
//! （header + 每行采样 + footer 全量持久化，实时 flush 可 `tail` 追溯）。
//! 解析类为纯函数（`parse_uid_bytes` / `established_uids` / `parse_pm_uids`），无真机可单测。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Local;

use crate::cli::NetwatchArgs;
use crate::{adb, device, session, util};

/// 采样间隔（秒）。内部固定值，不暴露为参数。
const SAMPLE_INTERVAL_SECS: u64 = 2;

/// rx 单次增量达到该字节数即高亮为「收到数据」，以区别长连心跳的小抖动。
const BURST_HIGHLIGHT_BYTES: u64 = 1024;

/// 监控目标：uid + 展示用标签（包名，或系统 uid 兜底描述）。
struct Target {
    uid: u32,
    label: String,
}

pub async fn run(args: NetwatchArgs) -> Result<()> {
    // 1. 选择目标设备（复用跨子命令的选择逻辑）
    let devices = device::list().await.context("枚举 adb 设备失败")?;
    let dev = device::select_target(devices, args.serial.as_deref())?;
    let serial = dev.serial.clone();

    // 2. 解析目标应用 -> uid：指定包名则直接解析，否则从「当前有网络连接的应用」交互选择
    let uid_map = parse_pm_uids(
        &adb::pm_packages_with_uid(&serial)
            .await
            .context("读取应用列表失败（pm list packages -U）")?,
    );
    let target = match args.package.as_deref() {
        Some(pkg) => {
            let uid = *uid_map.get(pkg).with_context(|| {
                format!("设备上未找到应用包名 {pkg}（用 `adb shell pm list packages` 查看可选包名）")
            })?;
            Target { uid, label: pkg.to_string() }
        }
        None => choose_net_active(&serial, &uid_map).await?,
    };

    // 3. 打开会话日志：终端显示之外同步落盘，供事后追溯（与 logs/screenshot 一致落 ~/.config）
    let dir = session::netwatch_dir(&serial)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建 netwatch 输出目录失败: {}", dir.display()))?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let log_path = dir.join(format!("netwatch-{stamp}.log"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("打开 netwatch 日志失败: {}", log_path.display()))?;

    // 4. 首次采样作为基线
    let (mut prev_rx, mut prev_tx) = sample(&serial, target.uid).await?;
    let (base_rx, base_tx) = (prev_rx, prev_tx);
    write_header(&mut file, &serial, &dev, &target, &log_path, prev_rx, prev_tx);

    // 5. 采样循环：每隔 SAMPLE_INTERVAL_SECS 记录一次增量（终端+磁盘），Ctrl-C 结束
    let started = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS)) => {}
            _ = tokio::signal::ctrl_c() => {
                write_footer(&mut file, started.elapsed(), prev_rx.saturating_sub(base_rx), prev_tx.saturating_sub(base_tx));
                return Ok(());
            }
        }
        let (rx, tx) = match sample(&serial, target.uid).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: 采样失败（将重试）: {e:#}");
                continue;
            }
        };
        let d_rx = rx.saturating_sub(prev_rx);
        let d_tx = tx.saturating_sub(prev_tx);
        let mark = if d_rx >= BURST_HIGHLIGHT_BYTES { "   ← 收到数据" } else { "" };
        emit(
            &mut file,
            &format!(
                "{}  rx +{:<10} tx +{:<10}{}",
                Local::now().format("%H:%M:%S"),
                util::human_bytes(d_rx),
                util::human_bytes(d_tx),
                mark
            ),
        );
        prev_rx = rx;
        prev_tx = tx;
    }
}

/// 同步双写一行：终端实时显示 + 追加落盘（best-effort，写盘失败不中断监控）。
fn emit(file: &mut File, line: &str) {
    println!("{line}");
    let _ = io::stdout().flush();
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

/// 采一次目标 uid 的累计 (rx, tx) 字节。
async fn sample(serial: &str, uid: u32) -> Result<(u64, u64)> {
    let out = adb::dumpsys_netstats(serial).await.context("读取 netstats 失败")?;
    Ok(parse_uid_bytes(&out, uid))
}

/// 交互选择「当前有网络连接的应用」。列表源 = `/proc/net/tcp{,6}` 中 established 的 uid。
async fn choose_net_active(serial: &str, uid_map: &HashMap<String, u32>) -> Result<Target> {
    let tcp = adb::net_tcp_raw(serial).await.context("读取网络连接失败（/proc/net/tcp）")?;
    let uids = established_uids(&tcp);

    // uid -> 该 uid 下的包名列表（可能多个共享 uid）
    let mut by_uid: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (pkg, u) in uid_map {
        by_uid.entry(*u).or_default().push(pkg.clone());
    }

    let mut choices: Vec<Target> = Vec::new();
    for u in &uids {
        let label = match by_uid.get(u) {
            Some(pk) if !pk.is_empty() => {
                let mut pk = pk.clone();
                pk.sort();
                if pk.len() > 3 {
                    format!("{}, …(+{})", pk[..3].join(", "), pk.len() - 3)
                } else {
                    pk.join(", ")
                }
            }
            _ => format!("uid:{u}（系统/无对应应用）"),
        };
        choices.push(Target { uid: *u, label });
    }

    if choices.is_empty() {
        bail!("当前没有检测到有网络连接的应用；请直接指定包名：jj-android-device netwatch <包名>");
    }

    // 提示走 stderr，保持 stdout 洁净
    eprintln!("当前有网络连接的应用（选一个盯它是否收到下发）：");
    for (i, t) in choices.iter().enumerate() {
        eprintln!("  [{}] {}  (uid {})", i + 1, t.label, t.uid);
    }
    eprint!("输入序号 (1-{}): ", choices.len());
    io::stderr().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line).context("读取选择输入失败")?;
    let n: usize = line.trim().parse().context("无效序号")?;
    let idx = n.checked_sub(1).context("序号需从 1 起")?;
    choices.into_iter().nth(idx).context("序号超出范围")
}

/// 从 `dumpsys netstats detail` 输出中，累计目标 uid 在「UID stats」段的 rb(收)/tb(发) 字节。
///
/// 仅计「UID stats」段：Dev/Xt stats 按 iface 汇总（无 uid 区分），UID tag stats 按 tag
/// 再分解（计入会重复计数）。段标题为顶格行（无前导空白），据此进出目标段。
fn parse_uid_bytes(dump: &str, uid: u32) -> (u64, u64) {
    let mut in_uid_stats = false;
    let mut cur_is_target = false;
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in dump.lines() {
        // 顶格行 = 段标题（如 `Dev stats:` / `UID stats:` / `UID tag stats:`）
        if !line.starts_with([' ', '\t']) {
            in_uid_stats = line.trim() == "UID stats:";
            cur_is_target = false;
            continue;
        }
        if !in_uid_stats {
            continue;
        }
        // ident 行携带 uid=，切换当前记账目标
        if let Some(u) = extract_num(line, "uid=") {
            cur_is_target = u == uid as u64;
            continue;
        }
        if cur_is_target {
            if let Some(v) = extract_num(line, "rb=").or_else(|| extract_num(line, "rxBytes=")) {
                rx += v;
            }
            if let Some(v) = extract_num(line, "tb=").or_else(|| extract_num(line, "txBytes=")) {
                tx += v;
            }
        }
    }
    (rx, tx)
}

/// 提取 `line` 中 `key`（如 `uid=` / `rb=`）之后紧邻的十进制数。无则 None。
fn extract_num(line: &str, key: &str) -> Option<u64> {
    let start = line.find(key)? + key.len();
    let digits: String = line[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 解析 `/proc/net/tcp` + `/proc/net/tcp6`，返回处于 ESTABLISHED(st=01) 连接的 uid 集合。
///
/// 列格式：`sl local rem st tx:rx tr retr uid ...`，st=列 3、uid=列 7（均 0 基）。
fn established_uids(raw: &str) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    for line in raw.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[3] != "01" {
            continue;
        }
        if let Ok(u) = f[7].parse::<u32>() {
            set.insert(u);
        }
    }
    set
}

/// 解析 `pm list packages -U` 输出（形如 `package:com.x uid:10120`）为 包名 -> uid。
fn parse_pm_uids(raw: &str) -> HashMap<String, u32> {
    let mut m = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        let pkg = line.strip_prefix("package:").and_then(|s| s.split_whitespace().next());
        let uid = line.split("uid:").nth(1).and_then(|s| s.trim().parse::<u32>().ok());
        if let (Some(p), Some(u)) = (pkg, uid) {
            m.insert(p.to_string(), u);
        }
    }
    m
}

fn write_header(file: &mut File, serial: &str, dev: &device::Device, t: &Target, log_path: &Path, rx: u64, tx: u64) {
    emit(file, "── jj-android-device netwatch ─────────────────────────────");
    emit(file, &format!("device   serial={} model={} connection={}", serial, dev.model_label(), dev.connection_label()));
    emit(file, &format!("target   {}  (uid {})", t.label, t.uid));
    emit(
        file,
        &format!(
            "说明     每 {SAMPLE_INTERVAL_SECS}s 采样「累计收发字节」；rx 单次增量 ≥{} 高亮为“收到数据”。Ctrl-C 结束。",
            util::human_bytes(BURST_HIGHLIGHT_BYTES)
        ),
    );
    emit(file, &format!("saved    = {}", log_path.display()));
    emit(file, &format!("基线     rx={}  tx={}", util::human_bytes(rx), util::human_bytes(tx)));
    emit(file, "───────────────────────────────────────────────────────────");
}

fn write_footer(file: &mut File, dur: Duration, rx: u64, tx: u64) {
    emit(file, "");
    emit(file, "── 结束 ────────────────────────────────────────────────────");
    emit(
        file,
        &format!(
            "观测时长 = {}   期间累计 rx=+{}  tx=+{}",
            util::human_duration(dur),
            util::human_bytes(rx),
            util::human_bytes(tx)
        ),
    );
    emit(file, "───────────────────────────────────────────────────────────");
}

#[cfg(test)]
mod tests {
    use super::*;

    // 含 Dev/Xt/UID stats/UID tag stats 四段，仿真真机结构（ident 行 + NetworkStatsHistory 行 + 桶行）。
    const NETSTATS: &str = "\
Dev stats:
  History since boot:
    ident=[{iface=wlan0}]
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=99999 rp=1 tb=88888 tp=1
Xt stats:
  History since boot:
    ident=[{iface=wlan0}]
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=77777 rp=1 tb=66666 tp=1
UID stats:
  Pending bytes: 0
  History since boot:
    ident=[{type=1}] uid=10120 set=FOREGROUND tag=0x0
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=100 rp=2 tb=40 tp=2
    ident=[{type=1}] uid=10120 set=BACKGROUND tag=0x0
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=25 rp=1 tb=10 tp=1
    ident=[{type=1}] uid=10999 set=FOREGROUND tag=0x0
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=7 rp=1 tb=7 tp=1
UID tag stats:
  History since boot:
    ident=[{type=1}] uid=10120 set=FOREGROUND tag=0xffffff82
      NetworkStatsHistory: bucketDuration=7200
      st=0 rb=50 rp=1 tb=20 tp=1
";

    #[test]
    fn uid_bytes_sums_only_uid_stats_section() {
        // 目标 uid=10120：仅 UID stats 段的两桶 rb=100+25=125，tb=40+10=50；
        // Dev/Xt（无 uid）、UID tag stats（tag 分解）与其他 uid 均不计入。
        assert_eq!(parse_uid_bytes(NETSTATS, 10120), (125, 50));
    }

    #[test]
    fn uid_bytes_other_uid() {
        assert_eq!(parse_uid_bytes(NETSTATS, 10999), (7, 7));
    }

    #[test]
    fn uid_bytes_absent_uid_is_zero() {
        assert_eq!(parse_uid_bytes(NETSTATS, 12345), (0, 0));
    }

    const PROC_TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 0 1
   1: ABCDEF01:C012 12345678:01BB 01 00000000:00000000 00:00000000 00000000 10120        0 0 1
   2: ABCDEF01:C013 12345678:01BB 06 00000000:00000000 00:00000000 00000000 10777        0 0 1
   3: ABCDEF01:C014 87654321:01BB 01 00000000:00000000 00:00000000 00000000 10105        0 0 1
";

    #[test]
    fn established_only() {
        // 仅 st=01 计入：uid 10120、10105；st=0A(LISTEN)/06(TIME_WAIT) 排除；表头行 f[3]=st 排除。
        let s = established_uids(PROC_TCP);
        assert_eq!(s.into_iter().collect::<Vec<_>>(), vec![10105, 10120]);
    }

    #[test]
    fn pm_uids_parsed() {
        let raw = "\
package:com.sunmi.ota uid:10127
package:sunmi.remotemanager uid:10120
package:broken.no.uid
package:com.x uid:notanumber
";
        let m = parse_pm_uids(raw);
        assert_eq!(m.get("com.sunmi.ota"), Some(&10127));
        assert_eq!(m.get("sunmi.remotemanager"), Some(&10120));
        assert_eq!(m.get("broken.no.uid"), None);
        assert_eq!(m.get("com.x"), None);
    }
}

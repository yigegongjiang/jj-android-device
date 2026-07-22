//! `netwatch` 子命令：无 root 实时监控网络收发字节增量。
//!
//! 每次先触发 Android netstats poll，再读取每个 uid 的累计收发字节（`dumpsys
//! netstats detail` 的「UID stats」段），按固定间隔采样并打印增量。数字跳变 =
//! 该 uid 确有网络收发。
//! 直接从系统计数器判断某应用/设备是否真的在网络上收发数据——不解密内容、不依赖
//! 应用自身日志（生产版常剥离/门控 Log）、且应用无法伪造这些字节计数。
//!
//! 两种模式，采集成本相同（单次 `dumpsys netstats` 已含全部 uid）：
//! - 单应用：指定包名 / 交互选「当前有网络连接的应用」，只盯一个 uid。
//! - 全部盯（`-a/--all`）：一次盯设备上全部 uid，每周期打印有增量的 uid，结束时按 rx 降序排名。
//!
//! 仅读系统计数器，不改设备、不需 root。Ctrl-C 优雅结束并打印本次观测汇总。
//! 输出同步双写：终端实时显示 + 落盘 `~/.config/jj-android-device/netwatch/<serial>/netwatch-<stamp>.log`
//! （header + 每行采样 + footer 全量持久化，实时 flush 可 `tail` 追溯）。
//! 解析类为纯函数（`parse_all_uid_bytes` / `parse_uid_bytes` / `established_uids` /
//! `parse_pm_uids`），无真机可单测。

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

/// 连续无增量（全 0）周期超过该次数后暂停打印/落盘（静默），有流量即恢复；避免长时间刷 0。
const ZERO_STREAK_KEEP: usize = 4;

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

    // 2. 包名 -> uid 映射（两种模式都需要：单应用解析目标 / 全部盯反查标签）
    let uid_map = parse_pm_uids(
        &adb::pm_packages_with_uid(&serial)
            .await
            .context("读取应用列表失败（pm list packages -U）")?,
    );

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

    // 全部盯模式：单独走 run_all，不解析单个目标。
    if args.all {
        return run_all(&serial, &dev, &uid_map, &mut file, &log_path).await;
    }

    // 单应用模式：指定包名则直接解析，否则从「当前有网络连接的应用」交互选择。
    let target = match args.package.as_deref() {
        Some(pkg) => {
            let uid = *uid_map.get(pkg).with_context(|| {
                format!("设备上未找到应用包名 {pkg}（用 `adb shell pm list packages` 查看可选包名）")
            })?;
            Target { uid, label: pkg.to_string() }
        }
        None => choose_net_active(&serial, &uid_map).await?,
    };

    // 首次采样作为基线
    let (mut prev_rx, mut prev_tx) = sample(&serial, target.uid).await?;
    let (base_rx, base_tx) = (prev_rx, prev_tx);
    write_header(&mut file, &serial, &dev, &target, &log_path, prev_rx, prev_tx);

    // 采样循环：每隔 SAMPLE_INTERVAL_SECS 记录一次增量（终端+磁盘），Ctrl-C 结束
    let started = Instant::now();
    let mut zero_streak = 0usize;
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
        prev_rx = rx;
        prev_tx = tx;
        // 连续静默（全 0）超过 ZERO_STREAK_KEEP 个周期后不再打印/落盘，有增量即恢复
        if d_rx == 0 && d_tx == 0 {
            zero_streak += 1;
            if zero_streak > ZERO_STREAK_KEEP {
                continue;
            }
        } else {
            zero_streak = 0;
        }
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
    }
}

/// 全部盯：一次监控设备上全部 uid。每周期打印本次有增量的 uid（按 rx 降序），
/// 并累加各 uid 的期间增量，Ctrl-C 结束时按累计 rx 降序排名——直接看哪些 app 在收发数据。
async fn run_all(
    serial: &str,
    dev: &device::Device,
    uid_map: &HashMap<String, u32>,
    file: &mut File,
    log_path: &Path,
) -> Result<()> {
    let by_uid = build_by_uid(uid_map);

    // 首次采样作为基线
    let mut prev = sample_all(serial).await?;
    let base = sum_bytes(prev.values());
    write_header_all(file, serial, dev, log_path, prev.len(), base);

    // 各 uid 期间累计增量，供结束时排名
    let mut totals: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    let started = Instant::now();
    let mut zero_streak = 0usize;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(SAMPLE_INTERVAL_SECS)) => {}
            _ = tokio::signal::ctrl_c() => {
                write_footer_all(file, started.elapsed(), &totals, &by_uid);
                return Ok(());
            }
        }
        let cur = match sample_all(serial).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: 采样失败（将重试）: {e:#}");
                continue;
            }
        };

        // 逐 uid 求增量；首次基线已强制 poll，中途新出现的 uid 必为观测期内首次产生流量，从 0 计。
        let mut changed: Vec<(u32, u64, u64)> = Vec::new();
        let (mut sum_rx, mut sum_tx) = (0u64, 0u64);
        for (&uid, &(rx, tx)) in &cur {
            let (prx, ptx) = prev.get(&uid).copied().unwrap_or((0, 0));
            let d_rx = rx.saturating_sub(prx);
            let d_tx = tx.saturating_sub(ptx);
            if d_rx > 0 || d_tx > 0 {
                changed.push((uid, d_rx, d_tx));
                sum_rx += d_rx;
                sum_tx += d_tx;
                let e = totals.entry(uid).or_insert((0, 0));
                e.0 += d_rx;
                e.1 += d_tx;
            }
        }

        prev = cur;
        // 连续静默（无任何 uid 增量）超过 ZERO_STREAK_KEEP 个周期后不再打印/落盘，有流量即恢复
        if changed.is_empty() {
            zero_streak += 1;
            if zero_streak > ZERO_STREAK_KEEP {
                continue;
            }
        } else {
            zero_streak = 0;
        }

        // 合计行（作为「仍在运行」的心跳），再按 rx 降序列出明细
        emit(
            file,
            &format!(
                "{}  active={:<3} rx +{:<10} tx +{:<10}",
                Local::now().format("%H:%M:%S"),
                changed.len(),
                util::human_bytes(sum_rx),
                util::human_bytes(sum_tx),
            ),
        );
        changed.sort_by_key(|c| std::cmp::Reverse(c.1));
        for (uid, d_rx, d_tx) in &changed {
            let mark = if *d_rx >= BURST_HIGHLIGHT_BYTES { "   ← 收到数据" } else { "" };
            emit(
                file,
                &format!(
                    "    {} (uid {})  rx +{:<10} tx +{:<10}{}",
                    label_for_uid(&by_uid, *uid),
                    uid,
                    util::human_bytes(*d_rx),
                    util::human_bytes(*d_tx),
                    mark
                ),
            );
        }
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

/// 采一次设备上全部 uid 的累计 (rx, tx) 字节。
async fn sample_all(serial: &str) -> Result<BTreeMap<u32, (u64, u64)>> {
    let out = adb::dumpsys_netstats(serial).await.context("读取 netstats 失败")?;
    Ok(parse_all_uid_bytes(&out))
}

/// 对一组 (rx, tx) 求和，返回 (Σrx, Σtx)。
fn sum_bytes<'a>(it: impl Iterator<Item = &'a (u64, u64)>) -> (u64, u64) {
    it.fold((0u64, 0u64), |(a, b), &(r, t)| (a + r, b + t))
}

/// 构建 uid -> 该 uid 下包名列表（可能多个共享 uid）。
fn build_by_uid(uid_map: &HashMap<String, u32>) -> BTreeMap<u32, Vec<String>> {
    let mut by_uid: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (pkg, u) in uid_map {
        by_uid.entry(*u).or_default().push(pkg.clone());
    }
    by_uid
}

/// uid 的展示标签：包名（多个取前 3 截断），无对应包名则兜底为 `uid:N（系统/无对应应用）`。
fn label_for_uid(by_uid: &BTreeMap<u32, Vec<String>>, uid: u32) -> String {
    match by_uid.get(&uid) {
        Some(pk) if !pk.is_empty() => {
            let mut pk = pk.clone();
            pk.sort();
            if pk.len() > 3 {
                format!("{}, …(+{})", pk[..3].join(", "), pk.len() - 3)
            } else {
                pk.join(", ")
            }
        }
        _ => format!("uid:{uid}（系统/无对应应用）"),
    }
}

/// 交互选择「当前有网络连接的应用」。列表源 = `/proc/net/tcp{,6}` 中 established 的 uid。
async fn choose_net_active(serial: &str, uid_map: &HashMap<String, u32>) -> Result<Target> {
    let tcp = adb::net_tcp_raw(serial).await.context("读取网络连接失败（/proc/net/tcp）")?;
    let uids = established_uids(&tcp);
    let by_uid = build_by_uid(uid_map);

    let choices: Vec<Target> = uids
        .iter()
        .map(|u| Target { uid: *u, label: label_for_uid(&by_uid, *u) })
        .collect();

    if choices.is_empty() {
        bail!("当前没有检测到有网络连接的应用；请直接指定包名：jj-android-device netwatch <包名>，或用 -a 盯全部");
    }

    // 提示走 stderr，保持 stdout 洁净
    eprintln!("当前有网络连接的应用（选一个盯它的收发增量）：");
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

/// 从 `dumpsys netstats detail` 输出中，累计**每个** uid 在「UID stats」段的 rb(收)/tb(发) 字节。
///
/// 仅计「UID stats」段：Dev/Xt stats 按 iface 汇总（无 uid 区分），UID tag stats 按 tag
/// 再分解（计入会重复计数）。段标题为顶格行（无前导空白），据此进出目标段。
fn parse_all_uid_bytes(dump: &str) -> BTreeMap<u32, (u64, u64)> {
    let mut in_uid_stats = false;
    let mut cur: Option<u32> = None;
    let mut map: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for line in dump.lines() {
        // 顶格行 = 段标题（如 `Dev stats:` / `UID stats:` / `UID tag stats:`）
        if !line.starts_with([' ', '\t']) {
            in_uid_stats = line.trim() == "UID stats:";
            cur = None;
            continue;
        }
        if !in_uid_stats {
            continue;
        }
        // ident 行携带 uid=，切换当前记账目标
        if let Some(u) = extract_num(line, "uid=") {
            cur = Some(u as u32);
            continue;
        }
        if let Some(uid) = cur {
            let e = map.entry(uid).or_insert((0, 0));
            if let Some(v) = extract_num(line, "rb=").or_else(|| extract_num(line, "rxBytes=")) {
                e.0 += v;
            }
            if let Some(v) = extract_num(line, "tb=").or_else(|| extract_num(line, "txBytes=")) {
                e.1 += v;
            }
        }
    }
    map
}

/// 单个 uid 的累计 (rx, tx) 字节。复用 [`parse_all_uid_bytes`] 作单一解析信源。
fn parse_uid_bytes(dump: &str, uid: u32) -> (u64, u64) {
    parse_all_uid_bytes(dump).get(&uid).copied().unwrap_or((0, 0))
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

fn write_header_all(file: &mut File, serial: &str, dev: &device::Device, log_path: &Path, uid_count: usize, base: (u64, u64)) {
    emit(file, "── jj-android-device netwatch (全部盯) ─────────────────────");
    emit(file, &format!("device   serial={} model={} connection={}", serial, dev.model_label(), dev.connection_label()));
    emit(file, &format!("target   ALL（设备全部 uid，基线含 {uid_count} 个有计数）"));
    emit(
        file,
        &format!(
            "说明     每 {SAMPLE_INTERVAL_SECS}s 采样全部 uid 累计收发；仅打印有增量的 uid（按 rx 降序），单次 rx 增量 ≥{} 高亮。Ctrl-C 结束。",
            util::human_bytes(BURST_HIGHLIGHT_BYTES)
        ),
    );
    emit(file, &format!("saved    = {}", log_path.display()));
    emit(file, &format!("基线     合计 rx={}  tx={}", util::human_bytes(base.0), util::human_bytes(base.1)));
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

fn write_footer_all(file: &mut File, dur: Duration, totals: &BTreeMap<u32, (u64, u64)>, by_uid: &BTreeMap<u32, Vec<String>>) {
    let (sum_rx, sum_tx) = sum_bytes(totals.values());
    emit(file, "");
    emit(file, "── 结束 ────────────────────────────────────────────────────");
    emit(
        file,
        &format!(
            "观测时长 = {}   期间合计 rx=+{}  tx=+{}   涉及 {} 个 uid",
            util::human_duration(dur),
            util::human_bytes(sum_rx),
            util::human_bytes(sum_tx),
            totals.len()
        ),
    );
    if !totals.is_empty() {
        emit(file, "排名（按累计 rx 降序）：");
        let mut v: Vec<(&u32, &(u64, u64))> = totals.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.1 .0));
        for (uid, (rx, tx)) in v {
            emit(
                file,
                &format!(
                    "   {} (uid {})  rx=+{}  tx=+{}",
                    label_for_uid(by_uid, *uid),
                    uid,
                    util::human_bytes(*rx),
                    util::human_bytes(*tx)
                ),
            );
        }
    }
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
    fn all_uid_bytes_maps_every_uid() {
        // 全部盯核心：一次解析出每个 uid 的累计 rx/tx。
        // 仅 UID stats 段：10120 两桶 rb=100+25=125/tb=40+10=50，10999 rb=7/tb=7；
        // Dev/Xt（无 uid）、UID tag stats（tag 分解）均不计入，故 len=2。
        let m = parse_all_uid_bytes(NETSTATS);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&10120), Some(&(125, 50)));
        assert_eq!(m.get(&10999), Some(&(7, 7)));
        assert_eq!(m.get(&12345), None);
    }

    #[test]
    fn uid_bytes_sums_only_uid_stats_section() {
        // 单 uid 复用 parse_all_uid_bytes：目标 uid=10120 得 (125, 50)。
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

    #[test]
    fn label_for_uid_falls_back_when_unknown() {
        // 全部盯会遇到无对应包名的系统 uid，需兜底展示而非丢弃。
        let uid_map: HashMap<String, u32> =
            [("com.a".to_string(), 10120), ("com.b".to_string(), 10120)].into_iter().collect();
        let by_uid = build_by_uid(&uid_map);
        assert_eq!(label_for_uid(&by_uid, 10120), "com.a, com.b");
        assert_eq!(label_for_uid(&by_uid, 1000), "uid:1000（系统/无对应应用）");
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

//! 落盘 sink：接收 logcat 行，去重后逐行写入会话日志，节流刷新心跳，累计指标。
//!
//! 去重是本工具「防倒灌 + 抖动缓冲」的正确性核心，抽为纯状态机 [`LineRouter`]
//! 便于单测。IO（写盘 / 心跳 mtime / flush 时机）在 [`run`] 任务里处理。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc::{self, error::TryRecvError};
use tokio::sync::watch;

use crate::procmap::Snapshot;
use crate::report::Metrics;
use crate::util;

/// collector -> sink 的通道消息。
pub enum Msg {
    /// 一整行 logcat 输出（含行尾换行；EOF 处末行可能无换行）
    Line(Vec<u8>),
    /// 即将拉起一条新 logcat 流：进入重连去重模式，丢弃与已写内容重叠的历史行
    Reconnecting,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Write,
    Drop,
}

/// 去重状态机。
///
/// 以「已写入的最后一行的设备 epoch 毫秒」`last_ms` 为水位：
/// - 正常流内：logcat `-b all` 输出按时间有序，直接写并维护水位与 `boundary`（同毫秒行集合）
/// - 重连后（`dedup=true`，收到 [`Msg::Reconnecting`]）：logcat 带 `-T last_ms` 会重发边界行，
///   逐行判定——`ts<水位` 丢、`ts==水位` 查 `boundary` 去重、`ts>水位` 说明越过重叠区即关闭去重
/// - 无时间戳的续行（多行日志的后续物理行）跟随其所属行的取舍（`last_kept`）
/// - `--------- beginning of <buffer>` 分隔行为 logcat UI 产物、每次拉起都会重发，一律丢弃
struct LineRouter {
    last_ms: i64,
    boundary: HashSet<Box<[u8]>>,
    dedup: bool,
    last_kept: bool,
}

impl LineRouter {
    fn new(start_ms: i64) -> Self {
        LineRouter {
            last_ms: start_ms,
            boundary: HashSet::new(),
            dedup: false,
            last_kept: true,
        }
    }

    fn last_ms(&self) -> i64 {
        self.last_ms
    }

    /// 进入重连去重模式：以当前水位为阈值，比对后续重发行。
    fn mark_reconnecting(&mut self) {
        self.dedup = true;
        self.last_kept = true;
    }

    fn decide(&mut self, line: &[u8]) -> Decision {
        if is_divider(line) {
            return Decision::Drop; // 不改变 last_kept：续行归属于前一条带时间戳的日志
        }
        match parse_ts_ms(line) {
            Some(t) => self.decide_timed(t, line),
            None => {
                // 续行/无法解析行：跟随所属日志的取舍
                if self.last_kept {
                    Decision::Write
                } else {
                    Decision::Drop
                }
            }
        }
    }

    fn decide_timed(&mut self, t: i64, line: &[u8]) -> Decision {
        if self.dedup {
            if t < self.last_ms {
                self.last_kept = false;
                Decision::Drop
            } else if t == self.last_ms {
                if self.boundary.contains(line) {
                    self.last_kept = false;
                    Decision::Drop
                } else {
                    self.boundary.insert(Box::from(line));
                    self.last_kept = true;
                    Decision::Write
                }
            } else {
                // 越过重叠区：关闭去重，恢复正常流写入
                self.dedup = false;
                self.advance(t, line);
                self.last_kept = true;
                Decision::Write
            }
        } else {
            self.advance(t, line);
            self.last_kept = true;
            Decision::Write
        }
    }

    /// 正常流写入时维护水位与同毫秒边界集合。
    fn advance(&mut self, t: i64, line: &[u8]) {
        if t > self.last_ms {
            self.last_ms = t;
            self.boundary.clear();
            self.boundary.insert(Box::from(line));
        } else if t == self.last_ms {
            self.boundary.insert(Box::from(line));
        }
        // t < last_ms：极少见的流内乱序，照写但不下调水位
    }
}

/// 是否为 logcat 的 `--------- beginning of <buffer>` 分隔行。
fn is_divider(line: &[u8]) -> bool {
    line.starts_with(b"--------- beginning of")
}

/// 从 `-v epoch` 行首解析设备 epoch（毫秒）。无前导时间戳（续行/分隔行）返回 None。
fn parse_ts_ms(line: &[u8]) -> Option<i64> {
    let start = line.iter().position(|&b| b != b' ' && b != b'\t')?;
    let rest = &line[start..];
    let end = rest
        .iter()
        .position(|&b| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n')
        .unwrap_or(rest.len());
    parse_epoch_token(&rest[..end])
}

/// 解析单个 epoch token（`sec` 或 `sec.mmm`）为毫秒。非法/空返回 None。
///
/// 去重水位（[`parse_ts_ms`]）与落盘富化（[`parse_line`]）共用此数值核。
fn parse_epoch_token(tok: &[u8]) -> Option<i64> {
    let (sec_b, frac_b) = match tok.iter().position(|&b| b == b'.') {
        Some(i) => (&tok[..i], &tok[i + 1..]),
        None => (tok, &b""[..]),
    };
    if sec_b.is_empty() || !sec_b.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut sec: i64 = 0;
    for &b in sec_b {
        sec = sec.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    let mut millis = 0i64;
    for (i, &b) in frac_b.iter().take(3).enumerate() {
        if !b.is_ascii_digit() {
            return None;
        }
        millis += (b - b'0') as i64 * 10i64.pow(2 - i as u32);
    }
    Some(sec * 1000 + millis)
}

/// `-v epoch` 行的结构拆解：epoch 毫秒 + pid/tid 原始字节 + tid 之后的余下字节。
struct Parsed<'a> {
    ms: i64,
    pid: &'a [u8],
    tid: &'a [u8],
    /// 自 tid 之后到行尾（以「空格 + 级别 + tag: msg + 换行」开头），原样保留。
    rest: &'a [u8],
}

/// 解析 `<epoch>  <pid>  <tid> <level> <tag>: msg` 为 [`Parsed`]。
///
/// 只接受行首为 epoch 且其后紧跟 pid、tid 两个数字段的规范行；续行/分隔行/任何
/// 不符合的行返回 None，交由调用方原样落盘。
fn parse_line(raw: &[u8]) -> Option<Parsed<'_>> {
    let n = raw.len();
    let mut i = 0;
    while i < n && (raw[i] == b' ' || raw[i] == b'\t') {
        i += 1;
    }
    // token1: epoch
    let e0 = i;
    while i < n && !matches!(raw[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    let ms = parse_epoch_token(&raw[e0..i])?;
    // token2: pid（数字）
    i = skip_spaces(raw, i)?;
    let p0 = i;
    while i < n && raw[i].is_ascii_digit() {
        i += 1;
    }
    if i == p0 {
        return None;
    }
    let pid = &raw[p0..i];
    // token3: tid（数字）
    i = skip_spaces(raw, i)?;
    let t0 = i;
    while i < n && raw[i].is_ascii_digit() {
        i += 1;
    }
    if i == t0 {
        return None;
    }
    let tid = &raw[t0..i];
    Some(Parsed { ms, pid, tid, rest: &raw[i..] })
}

/// 跳过一段（至少一个）空格/制表符，返回新位置；无空白可跳则 None（行格式异常）。
fn skip_spaces(raw: &[u8], mut i: usize) -> Option<usize> {
    let start = i;
    while i < raw.len() && (raw[i] == b' ' || raw[i] == b'\t') {
        i += 1;
    }
    if i == start {
        None
    } else {
        Some(i)
    }
}

/// 落盘富化：把 `-v epoch` 原始行改写为
/// `YYYY-MM-DD HH:MM:SS.mmm  pid-tid  进程名 <级别> <tag>: msg`。
///
/// - 仅当 [`parse_line`] 成功时改写；续行/分隔行/解析失败一律原样返回，绝不丢信息。
/// - 消息体保持原始字节（可能非 UTF-8），只在行首拼接 ASCII 前缀；进程名查不到以
///   `?` 占位（时间戳仍可读，进程名随下一轮 `ps` 快照补齐）。
fn enrich(raw: &[u8], names: &HashMap<u32, String>) -> Vec<u8> {
    let Some(p) = parse_line(raw) else {
        return raw.to_vec();
    };
    let name = std::str::from_utf8(p.pid)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .and_then(|pid| names.get(&pid))
        .map(String::as_str)
        .unwrap_or("?");
    let mut out = Vec::with_capacity(raw.len() + 32);
    out.extend_from_slice(util::ms_to_local(p.ms).as_bytes());
    out.extend_from_slice(b"  ");
    out.extend_from_slice(p.pid);
    out.push(b'-');
    out.extend_from_slice(p.tid);
    out.extend_from_slice(b"  ");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(p.rest); // " <级别> <tag>: msg\n"，含原始换行
    out
}

fn touch(heartbeat: &std::fs::File) -> Result<()> {
    heartbeat
        .set_modified(SystemTime::now())
        .context("更新心跳文件 mtime 失败")
}

/// sink 任务主循环。返回时保证已 flush 全部缓冲。
pub async fn run(
    mut rx: mpsc::Receiver<Msg>,
    log_file: tokio::fs::File,
    heartbeat: std::fs::File,
    metrics: Arc<Metrics>,
    procmap: watch::Receiver<Snapshot>,
) -> Result<()> {
    let mut writer = BufWriter::new(log_file);
    let mut router = LineRouter::new(metrics.last_log_ms());
    let mut dirty = false;

    // 心跳节流：每秒最多一次 mtime 更新，同时兜底 flush
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // 消费立即返回的首 tick

    'outer: loop {
        tokio::select! {
            biased;
            _ = ticker.tick() => {
                writer.flush().await.context("flush 会话日志失败")?;
                if dirty {
                    touch(&heartbeat)?;
                    dirty = false;
                }
            }
            msg = rx.recv() => {
                let Some(m) = msg else { break 'outer };
                handle(&mut writer, &mut router, &metrics, &mut dirty, &procmap, m).await?;
                // 排空立即可取的消息，突发时批量写盘减少 flush 次数
                loop {
                    match rx.try_recv() {
                        Ok(m) => handle(&mut writer, &mut router, &metrics, &mut dirty, &procmap, m).await?,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break 'outer,
                    }
                }
                // 追平后立即 flush，保证外部工具可实时增量读取
                writer.flush().await.context("flush 会话日志失败")?;
            }
        }
    }

    writer.flush().await.context("退出前 flush 会话日志失败")?;
    if dirty {
        touch(&heartbeat)?;
    }
    Ok(())
}

async fn handle(
    writer: &mut BufWriter<tokio::fs::File>,
    router: &mut LineRouter,
    metrics: &Arc<Metrics>,
    dirty: &mut bool,
    procmap: &watch::Receiver<Snapshot>,
    msg: Msg,
) -> Result<()> {
    match msg {
        Msg::Reconnecting => router.mark_reconnecting(),
        Msg::Line(buf) => {
            // 去重/水位判定在原始行上进行（不受富化影响）
            if router.decide(&buf) == Decision::Write {
                // 克隆当前快照的 Arc（一次原子递增），避免跨 await 持有 watch 读锁
                let snap = procmap.borrow().clone();
                let out = enrich(&buf, &snap);
                writer.write_all(&out).await.context("写入会话日志失败")?;
                metrics.record_line(out.len());
                metrics.set_last_log_ms(router.last_ms());
                *dirty = true;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(epoch: &str, msg: &str) -> Vec<u8> {
        format!("         {epoch}  1234  5678 I Tag     : {msg}\n").into_bytes()
    }

    #[test]
    fn parses_leading_epoch() {
        assert_eq!(parse_ts_ms(&line("1784689023.022", "x")), Some(1784689023022));
        assert_eq!(parse_ts_ms(b"--------- beginning of main\n"), None);
        assert_eq!(parse_ts_ms(b"    continuation without ts\n"), None);
    }

    #[test]
    fn normal_stream_writes_all_in_order() {
        let mut r = LineRouter::new(1000);
        for (e, m) in [("1.500", "a"), ("1.600", "b"), ("2.000", "c")] {
            assert_eq!(r.decide(&line(e, m)), Decision::Write, "line {e}");
        }
        assert_eq!(r.last_ms(), 2000);
    }

    #[test]
    fn divider_always_dropped() {
        let mut r = LineRouter::new(0);
        assert_eq!(r.decide(b"--------- beginning of system\n"), Decision::Drop);
    }

    #[test]
    fn reconnect_dedups_overlap_no_loss_no_dup() {
        let mut r = LineRouter::new(0);
        // 首个流：写入到 1.200，其中 1.200 上有两行 a、b
        r.mark_reconnecting(); // 会话起始也走一次
        let l_a = line("1.200", "a");
        let l_b = line("1.200", "b");
        let l_c = line("1.100", "old");
        assert_eq!(r.decide(&l_c), Decision::Write); // 1.100 > 0，越过阈值即写
        assert_eq!(r.decide(&l_a), Decision::Write); // 1.200 前进
        assert_eq!(r.decide(&l_b), Decision::Write); // 1.200 同毫秒新行
        assert_eq!(r.last_ms(), 1200);

        // 断线重连：logcat -T 1.200 重发 [1.100(<水位), 1.200 a, 1.200 b] + 新行
        r.mark_reconnecting();
        assert_eq!(r.decide(&l_c), Decision::Drop); // ts < 水位，历史行丢弃
        assert_eq!(r.decide(&l_a), Decision::Drop); // ts==水位且已写，丢弃
        assert_eq!(r.decide(&l_b), Decision::Drop); // 同上
        let l_b2 = line("1.200", "b_new_same_ms"); // 同毫秒但重连窗口内产生的新行
        assert_eq!(r.decide(&l_b2), Decision::Write); // 不在 boundary，保留
        let l_d = line("1.300", "d"); // 重连窗口内的新行
        assert_eq!(r.decide(&l_d), Decision::Write); // ts>水位，越过重叠、关闭去重
        assert_eq!(r.last_ms(), 1300);
    }

    #[test]
    fn continuation_follows_owner_decision() {
        let mut r = LineRouter::new(0);
        r.mark_reconnecting();
        r.decide(&line("1.000", "seed")); // 越过阈值，写，last_kept=true
        assert_eq!(r.decide(b"    multiline tail\n"), Decision::Write); // 续行跟随 -> Write

        // 制造一次被丢弃的行，其续行也应丢弃
        r.mark_reconnecting();
        assert_eq!(r.decide(&line("0.500", "old")), Decision::Drop); // <水位
        assert_eq!(r.decide(b"    tail of dropped\n"), Decision::Drop);
    }

    fn names() -> HashMap<u32, String> {
        let mut m = HashMap::new();
        m.insert(1040u32, "android.hardware.audio.service_64".to_string());
        m
    }

    #[test]
    fn enrich_known_pid() {
        // 真机采样行：epoch + 双空格 + pid + tid + 级别 + tag: msg
        let raw = b"         1784695527.474  1040  5062 D AGM: metadata_print: 92\n";
        let out = enrich(raw, &names());
        let s = String::from_utf8(out).unwrap();
        // 本地时区不定，只校验结构：pid-tid、进程名、原始 tag/msg 尾部俱在
        assert!(s.contains("  1040-5062  android.hardware.audio.service_64 D AGM: metadata_print: 92\n"), "got {s:?}");
        assert!(!s.contains("1784695527.474"), "epoch 应被替换为本地时间: {s:?}");
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn enrich_unknown_pid_placeholder() {
        let raw = b"         1784695527.474  9999  9999 I Tag: hi\n";
        let s = String::from_utf8(enrich(raw, &names())).unwrap();
        assert!(s.contains("  9999-9999  ? I Tag: hi\n"), "got {s:?}");
    }

    #[test]
    fn enrich_continuation_passthrough() {
        // 无前导 epoch 的续行/异常行原样返回
        let raw = b"    at com.foo.Bar.baz(Bar.java:1)\n";
        assert_eq!(enrich(raw, &names()), raw.to_vec());
        let divider = b"--------- beginning of main\n";
        assert_eq!(enrich(divider, &names()), divider.to_vec());
    }

    #[test]
    fn enrich_preserves_non_utf8_message() {
        // 消息体含非 UTF-8 字节，必须逐字节保留
        let mut raw = b"         1.500  1040  1040 I Tag: ".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0x00]);
        raw.push(b'\n');
        let out = enrich(&raw, &names());
        assert!(out.ends_with(&[0xff, 0xfe, 0x00, b'\n']), "非 UTF-8 尾部应原样保留");
        assert!(out.windows(2).any(|w| w == b"1-")); // 1040-1040 存在
    }

    #[test]
    fn parse_line_extracts_fields() {
        let p = parse_line(b"         1784695527.474  1040  5062 D AGM: x\n").unwrap();
        assert_eq!(p.ms, 1784695527474);
        assert_eq!(p.pid, b"1040");
        assert_eq!(p.tid, b"5062");
        assert_eq!(p.rest, b" D AGM: x\n");
        assert!(parse_line(b"    continuation\n").is_none());
    }
}

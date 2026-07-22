//! `procs` 子命令：一次性打印设备当前活跃的 **app 包名**（默认），或全部原始进程名（`-a`）。
//!
//! 默认（无输入即可）：取 `ps -A -o NAME` 的活跃进程名，剥离 `:suffix`，与
//! `pm list packages` 的已装包集合求交，得到「当前在跑的 app 包名」去重排序输出——
//! 快速确认某 app（如 `remotemanager` 之类）是否活跃。`-a/--all` 则列全部原始进程名
//! （含 `system_server` / 内核线程等）。可选位置参数 `<filter>` 做大小写不敏感子串过滤。
//!
//! stdout 仅打名字（每行一个，方便 `| rg`），摘要走 stderr，保持管道洁净。
//! 解析为纯函数（`parse_names` / `parse_pm_names` / `active_packages` / `filter_names`），无真机可单测。

use std::collections::HashSet;
use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::cli::ProcsArgs;
use crate::{adb, device};

pub async fn run(args: ProcsArgs) -> Result<()> {
    // 1. 选择目标设备（复用跨子命令的选择逻辑）
    let devices = device::list().await.context("枚举 adb 设备失败")?;
    let dev = device::select_target(devices, args.serial.as_deref())?;
    let serial = dev.serial.clone();

    // 2. 取进程名快照；默认模式再取已装包集合（并发）
    let proc_raw = adb::ps_names(&serial)
        .await
        .context("读取进程列表失败（ps -A -o NAME）")?;
    let procs = parse_names(&proc_raw);
    let proc_total = procs.len();

    // 3. 组装输出列表 + 摘要中段
    let (mut list, summary) = if args.all {
        // 全部原始进程名（含系统 / 内核线程）
        let mut names = procs;
        names.sort();
        names.dedup();
        let unique = names.len();
        (names, format!("processes  {proc_total} total / {unique} unique"))
    } else {
        // 默认：活跃 app 包名 = 进程名（剥 :suffix）∩ 已装包
        let pkg_raw = adb::pm_packages_with_uid(&serial)
            .await
            .context("读取已装包列表失败（pm list packages）")?;
        let installed = parse_pm_names(&pkg_raw);
        let pkgs = active_packages(&procs, &installed);
        let n = pkgs.len();
        (pkgs, format!("packages   {n} active（进程 {proc_total}，已装 {}）", installed.len()))
    };

    // 4. 可选子串过滤
    let matched = args.filter.as_deref().map(|f| {
        list = filter_names(&list, f);
        list.len()
    });

    // 5. 摘要（stderr，保持 stdout 洁净供管道）
    let stderr = io::stderr();
    let mut e = stderr.lock();
    let _ = writeln!(e, "── jj-android-device procs ────────────────────────────────");
    let _ = writeln!(
        e,
        "device     serial={} model={} connection={}",
        serial,
        dev.model_label(),
        dev.connection_label()
    );
    match (args.filter.as_deref(), matched) {
        (Some(f), Some(m)) => {
            let _ = writeln!(e, "{summary}；filter={f:?} 命中 {m}");
        }
        _ => {
            let _ = writeln!(e, "{summary}");
        }
    }
    let _ = writeln!(e, "───────────────────────────────────────────────────────────");
    let _ = e.flush();

    // 6. 名字逐行落 stdout
    let stdout = io::stdout();
    let mut o = stdout.lock();
    for name in &list {
        let _ = writeln!(o, "{name}");
    }
    let _ = o.flush();
    Ok(())
}

/// 解析 `ps -A -o NAME` 输出为进程名列表（保序，不去重）。
///
/// 跳过表头 `NAME` 与空行；每行 trim 后即进程名（含 `:suffix` 多进程后缀、
/// `[kworker/...]` 内核线程等，全量保留，交给下游按模式取舍）。
fn parse_names(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "NAME")
        .map(String::from)
        .collect()
}

/// 解析 `pm list packages [-U]` 输出为已装包名集合。
///
/// 每行形如 `package:com.x`（或带尾随 `uid:10120`）；取 `package:` 后到首个空白前的 token。
fn parse_pm_names(out: &str) -> HashSet<String> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// 活跃 app 包名：进程名剥 `:suffix` 后，保留命中已装包集合者，去重排序。
///
/// 多进程 app（`com.x` / `com.x:push` / `com.x:remote`）归并为一个 `com.x`；
/// `system_server`、原生守护、`[kworker/...]` 等非包进程因不在已装集合中被滤除。
fn active_packages(proc_names: &[String], installed: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = proc_names
        .iter()
        .map(|n| n.split(':').next().unwrap_or(n))
        .filter(|base| installed.contains(*base))
        .map(String::from)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 大小写不敏感子串过滤（保序）。
fn filter_names(names: &[String], needle: &str) -> Vec<String> {
    let needle = needle.to_lowercase();
    names
        .iter()
        .filter(|n| n.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{active_packages, filter_names, parse_names, parse_pm_names};
    use std::collections::HashSet;

    #[test]
    fn parses_and_skips_header() {
        let out = "\
NAME
init
com.android.systemui
com.example.remotemanager
[kworker/0:1]";
        let n = parse_names(out);
        assert_eq!(
            n,
            vec![
                "init",
                "com.android.systemui",
                "com.example.remotemanager",
                "[kworker/0:1]",
            ]
        );
    }

    #[test]
    fn skips_blank_lines() {
        let n = parse_names("NAME\n\n  \ninit\n");
        assert_eq!(n, vec!["init"]);
    }

    #[test]
    fn pm_names_handles_plain_and_uid() {
        let out = "\
package:com.android.systemui
package:com.example.remotemanager uid:10120
garbage line
package:";
        let s = parse_pm_names(out);
        assert!(s.contains("com.android.systemui"));
        assert!(s.contains("com.example.remotemanager"));
        assert_eq!(s.len(), 2); // 空包名与非法行被滤除
    }

    #[test]
    fn active_packages_strips_suffix_and_intersects() {
        let procs = vec![
            "com.example.app".to_string(),
            "com.example.app:push".to_string(),
            "com.example.app:remote".to_string(),
            "system_server".to_string(),
            "[kworker/0:1]".to_string(),
            "com.not.installed".to_string(),
        ];
        let installed: HashSet<String> =
            ["com.example.app".to_string(), "com.android.systemui".to_string()]
                .into_iter()
                .collect();
        // 多进程归并为一个包；未安装 / 非包进程被滤除
        assert_eq!(active_packages(&procs, &installed), vec!["com.example.app"]);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let names = vec![
            "com.example.RemoteManager".to_string(),
            "com.android.systemui".to_string(),
            "init".to_string(),
        ];
        assert_eq!(
            filter_names(&names, "remotemanager"),
            vec!["com.example.RemoteManager"]
        );
        assert_eq!(
            filter_names(&names, "com."),
            vec!["com.example.RemoteManager", "com.android.systemui"]
        );
        assert!(filter_names(&names, "zzz").is_empty());
    }
}

//! 产物路径布局与 pid 单例守卫。
//!
//! 配置根固定 `~/.config/jj-android-device`；`logs` 采集落 `logs/<serial>/`、
//! `screenshot` 截屏落 `screenshots/<serial>/`、`netwatch` 会话落 `netwatch/<serial>/`，
//! 均按设备序列号隔离子目录。同一设备同一时刻仅允许一个采集进程（pid 文件 + 存活探测）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Local;

/// 一次采集会话的全部产物路径与起始时间。
pub struct Session {
    pub log_path: PathBuf,
    pub events_path: PathBuf,
    pub heartbeat_path: PathBuf,
    pub pid_path: PathBuf,
    pub readme_path: PathBuf,
    /// 会话起始本地时间（用于启动摘要）
    pub started_local: String,
}

/// 配置根目录：`$HOME/.config/jj-android-device`（各子命令产物的公共父目录）。
pub fn config_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("无法确定 HOME 环境变量，无法定位输出目录")?;
    Ok(PathBuf::from(home).join(".config").join("jj-android-device"))
}

/// `logs` 采集输出根目录：`<config_root>/logs`。
pub fn default_root() -> Result<PathBuf> {
    Ok(config_root()?.join("logs"))
}

/// `screenshot` 截屏产物目录：`<config_root>/screenshots/<serial>/`（未创建）。
pub fn screenshot_dir(serial: &str) -> Result<PathBuf> {
    Ok(config_root()?.join("screenshots").join(sanitize(serial)))
}

/// `netwatch` 会话日志目录：`<config_root>/netwatch/<serial>/`（未创建）。
pub fn netwatch_dir(serial: &str) -> Result<PathBuf> {
    Ok(config_root()?.join("netwatch").join(sanitize(serial)))
}

/// 将序列号规整为安全的目录名（TCP 序列号含 `:` 等）。
fn sanitize(serial: &str) -> String {
    serial
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

/// 创建 `<root>/<serial>/` 并规划本次会话文件名（按时间戳隔离）。
pub fn setup(root: &Path, serial: &str) -> Result<Session> {
    let dir = root.join(sanitize(serial));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建输出目录失败: {}", dir.display()))?;

    let now = Local::now();
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let started_local = now.format("%Y-%m-%dT%H:%M:%S%:z").to_string();

    Ok(Session {
        log_path: dir.join(format!("session-{stamp}.log")),
        events_path: dir.join(format!("session-{stamp}.events.log")),
        heartbeat_path: dir.join(".heartbeat"),
        pid_path: dir.join("session.pid"),
        readme_path: dir.join("readme.md"),
        started_local,
    })
}

/// pid 单例守卫：构造时写入本进程 pid，Drop 时移除。
pub struct PidGuard {
    path: PathBuf,
}

impl PidGuard {
    /// 获取单例。已存在且对应进程仍存活 -> 拒绝启动；陈旧 pid 文件则覆盖。
    pub fn acquire(path: &Path) -> Result<PidGuard> {
        if let Some(pid) = read_pid(path) {
            if pid_alive(pid) {
                bail!(
                    "该设备已有采集进程在运行（pid={pid}）；同一设备同一会话仅允许一个进程。\n\
                     若确认为残留，删除 {} 后重试",
                    path.display()
                );
            }
        }
        std::fs::write(path, format!("{}\n", std::process::id()))
            .with_context(|| format!("写入 pid 文件失败: {}", path.display()))?;
        Ok(PidGuard { path: path.to_path_buf() })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 探测本机进程是否存活：`kill(pid, 0)` 成功或返回 EPERM（存在但无权限）即视为存活。
fn pid_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_tcp_serial() {
        assert_eq!(sanitize("10.0.3.26:5555"), "10.0.3.26_5555");
        assert_eq!(sanitize("VA07258740751"), "VA07258740751");
    }

    #[test]
    fn pid_guard_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("jjad-test-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        {
            let _g = PidGuard::acquire(&tmp).expect("首次获取应成功");
            // 本进程存活 -> 再次获取应被拒绝
            assert!(PidGuard::acquire(&tmp).is_err());
        }
        // Drop 后文件应被清理
        assert!(!tmp.exists());
    }
}

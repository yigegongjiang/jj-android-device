//! `screenshot` 子命令：一次性抓取设备当前屏幕，PNG 落盘。
//!
//! 选设备 -> `adb exec-out screencap -p` 取原始 PNG -> 校验 magic bytes ->
//! 写入 `~/.config/jj-android-device/screenshots/<serial>/screenshot-<stamp>.png`。
//! 一次性命令，无守护 / 无 pid 守卫；捕获路径 `cargo test` 无法覆盖，PNG 头校验
//! 即唯一运行时护栏（兜住 exec-out 缺失 / secure surface 返回非 PNG 等失败）。

use anyhow::{bail, Context, Result};
use chrono::Local;

use crate::cli::ScreenshotArgs;
use crate::{adb, device, session, util};

/// PNG 文件签名（8 字节 magic）。
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

pub async fn run(args: ScreenshotArgs) -> Result<()> {
    // 1. 选择目标设备
    let devices = device::list().await.context("枚举 adb 设备失败")?;
    let target = device::select_target(devices, args.serial.as_deref())?;
    let serial = target.serial.clone();

    // 2. 抓屏（原始 PNG 字节）
    let png = adb::screencap_png(&serial).await?;

    // 3. 校验：非 PNG 头即视为捕获失败（secure surface / exec-out 缺失等），不落坏文件
    if !png.starts_with(PNG_MAGIC) {
        bail!(
            "抓取的数据不是有效 PNG（{} 字节）；可能当前界面禁止截屏（DRM/安全窗口）或设备不支持 screencap",
            png.len()
        );
    }

    // 4. 规划产物路径并落盘
    let dir = session::screenshot_dir(&serial)?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("创建截图目录失败: {}", dir.display()))?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let path = dir.join(format!("screenshot-{stamp}.png"));
    tokio::fs::write(&path, &png)
        .await
        .with_context(|| format!("写入截图失败: {}", path.display()))?;

    // 5. 摘要（stdout）
    let resolution = png_dimensions(&png)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "未知".to_string());
    println!("── jj-android-device screenshot ───────────────────────────");
    println!("device serial={} model={} connection={}", serial, target.model_label(), target.connection_label());
    println!("saved      = {}", path.display());
    println!("size       = {}  resolution={}", util::human_bytes(png.len() as u64), resolution);
    println!("───────────────────────────────────────────────────────────");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    Ok(())
}

/// 从 PNG 字节解析 (宽, 高)。IHDR 紧随签名：8B 签名 + 4B 长度 + 4B "IHDR" + 4B 宽 + 4B 高。
/// 非 PNG / 截断 / IHDR 缺失返回 None。
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(PNG_MAGIC) || data.len() < 24 || &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造仅含合法 PNG 签名 + IHDR 头的最小样本。
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(PNG_MAGIC);
        v.extend_from_slice(&[0, 0, 0, 13]); // IHDR 长度
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn parse_dimensions() {
        assert_eq!(png_dimensions(&png_header(1080, 2400)), Some((1080, 2400)));
        assert_eq!(png_dimensions(&png_header(720, 1600)), Some((720, 1600)));
    }

    #[test]
    fn reject_non_png() {
        assert_eq!(png_dimensions(b"not a png at all......."), None);
        // 合法签名但被截断（无完整 IHDR）
        assert_eq!(png_dimensions(PNG_MAGIC), None);
    }
}

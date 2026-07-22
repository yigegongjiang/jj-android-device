//! 设备模型与选择逻辑。
//!
//! 选择逻辑 [`resolve`] 为纯函数（输入设备列表 + 请求序列号），便于在无真机时
//! 用合成的 `adb devices -l` 文本单测多设备场景。交互式挑选留给调用方（IO 隔离）。

use std::io::{self, Write};

use crate::adb;
use anyhow::{bail, Context, Result};

/// 一台 adb 设备的连接信息（来自 `adb devices -l`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// 连接状态：`device` / `unauthorized` / `offline` / ...
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
    /// `usb:` 字段（USB 连接时存在）
    pub usb: Option<String>,
    pub transport_id: Option<String>,
}

/// 连接方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Usb,
    /// TCP/IP 连接，携带端口
    Tcp { port: String },
    Unknown,
}

impl Device {
    /// 由序列号形态与 `usb:` 字段推断连接方式。TCP 序列号形如 `10.0.0.5:5555`。
    pub fn connection(&self) -> Connection {
        if let Some((_host, port)) = self.serial.rsplit_once(':') {
            if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
                return Connection::Tcp { port: port.to_string() };
            }
        }
        if self.usb.is_some() {
            Connection::Usb
        } else {
            Connection::Unknown
        }
    }

    /// 连接方式的人类可读描述，如 `USB` / `TCP:5555`。
    pub fn connection_label(&self) -> String {
        match self.connection() {
            Connection::Usb => "USB".to_string(),
            Connection::Tcp { port } => format!("TCP:{port}"),
            Connection::Unknown => "未知".to_string(),
        }
    }

    pub fn model_label(&self) -> &str {
        self.model.as_deref().unwrap_or("<unknown>")
    }
}

/// [`resolve`] 的结果：调用方据此直接采集 / 交互挑选 / 报错。
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// 唯一确定的目标设备
    One(Device),
    /// 多台在线设备，需交互挑选
    Choose(Vec<Device>),
    /// 无任何在线设备
    NoneOnline,
    /// 指定的序列号不存在
    NotFound(String),
    /// 指定/唯一的设备处于非 `device` 状态（如 unauthorized / offline）
    Unusable(Device),
}

/// 解析 `adb devices -l` 输出为设备列表。
///
/// 跳过首行标题与空行；容忍未来字段扩展（按 `key:value` 提取已知键）。
pub fn parse_devices(raw: &str) -> Vec<Device> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(serial) = it.next() else { continue };
        let Some(state) = it.next() else { continue };
        let mut dev = Device {
            serial: serial.to_string(),
            state: state.to_string(),
            model: None,
            product: None,
            usb: None,
            transport_id: None,
        };
        for tok in it {
            if let Some((k, v)) = tok.split_once(':') {
                match k {
                    "model" => dev.model = Some(v.to_string()),
                    "product" => dev.product = Some(v.to_string()),
                    "usb" => dev.usb = Some(v.to_string()),
                    "transport_id" => dev.transport_id = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        out.push(dev);
    }
    out
}

/// 纯选择逻辑：给定设备列表与可选请求序列号，决定采集目标。
pub fn resolve(devices: Vec<Device>, requested: Option<&str>) -> Selection {
    if let Some(req) = requested {
        return match devices.into_iter().find(|d| d.serial == req) {
            Some(d) if d.state == "device" => Selection::One(d),
            Some(d) => Selection::Unusable(d),
            None => Selection::NotFound(req.to_string()),
        };
    }
    let online: Vec<Device> = devices.iter().filter(|d| d.state == "device").cloned().collect();
    match online.len() {
        0 => {
            // 无在线设备：若存在非 device 状态的唯一设备，给出更具体的原因
            if devices.len() == 1 {
                Selection::Unusable(devices.into_iter().next().unwrap())
            } else {
                Selection::NoneOnline
            }
        }
        1 => Selection::One(online.into_iter().next().unwrap()),
        _ => Selection::Choose(online),
    }
}

/// 拉取并解析当前设备列表。
pub async fn list() -> Result<Vec<Device>> {
    Ok(parse_devices(&adb::devices_raw().await?))
}

/// 跨子命令共享的设备选择：单台直用、多台交互挑选、异常给出清晰原因。
pub fn select_target(devices: Vec<Device>, requested: Option<&str>) -> Result<Device> {
    match resolve(devices, requested) {
        Selection::One(d) => Ok(d),
        Selection::Choose(list) => prompt_choice(list),
        Selection::NoneOnline => {
            bail!("未发现处于 device 状态的设备；检查连线与授权后重试（adb devices）")
        }
        Selection::NotFound(s) => bail!("未找到序列号为 {s} 的设备"),
        Selection::Unusable(d) => {
            bail!("设备 {} 当前状态为 {}，无法操作（需在设备端授权，使其显示为 device）", d.serial, d.state)
        }
    }
}

/// 多设备交互挑选（提示走 stderr，保持 stdout 洁净供结构化/二进制输出）。
fn prompt_choice(list: Vec<Device>) -> Result<Device> {
    eprintln!("检测到多台在线设备，请选择目标：");
    for (i, d) in list.iter().enumerate() {
        eprintln!("  [{}] {} ({}) {}", i + 1, d.serial, d.model_label(), d.connection_label());
    }
    eprint!("输入序号 (1-{}): ", list.len());
    io::stderr().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line).context("读取选择输入失败")?;
    let n: usize = line.trim().parse().context("无效序号")?;
    let idx = n.checked_sub(1).context("序号需从 1 起")?;
    list.get(idx).cloned().context("序号超出范围")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "\
List of devices attached
AAAA1111               device usb:1-1 product:foo model:Pixel_7 device:foo transport_id:1
BBBB2222               device usb:2-1 product:bar model:Galaxy_S22 device:bar transport_id:3
";

    const ONE_UNAUTH: &str = "\
List of devices attached
CCCC3333               unauthorized usb:1-2 transport_id:2
";

    const TCP: &str = "\
List of devices attached
10.0.3.26:5555         device product:baz model:V3 device:baz transport_id:5
";

    #[test]
    fn parse_two_devices() {
        let d = parse_devices(TWO);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].serial, "AAAA1111");
        assert_eq!(d[0].model.as_deref(), Some("Pixel_7"));
        assert_eq!(d[1].serial, "BBBB2222");
        assert_eq!(d[1].model.as_deref(), Some("Galaxy_S22"));
    }

    #[test]
    fn resolve_multi_needs_choice() {
        let d = parse_devices(TWO);
        match resolve(d, None) {
            Selection::Choose(v) => assert_eq!(v.len(), 2),
            other => panic!("期望 Choose，得到 {other:?}"),
        }
    }

    #[test]
    fn resolve_requested_hits() {
        let d = parse_devices(TWO);
        assert_eq!(resolve(d, Some("BBBB2222")), {
            let d2 = parse_devices(TWO);
            Selection::One(d2.into_iter().find(|x| x.serial == "BBBB2222").unwrap())
        });
    }

    #[test]
    fn resolve_requested_missing() {
        let d = parse_devices(TWO);
        assert_eq!(resolve(d, Some("ZZZZ")), Selection::NotFound("ZZZZ".to_string()));
    }

    #[test]
    fn resolve_single_unauthorized() {
        let d = parse_devices(ONE_UNAUTH);
        match resolve(d, None) {
            Selection::Unusable(dev) => assert_eq!(dev.state, "unauthorized"),
            other => panic!("期望 Unusable，得到 {other:?}"),
        }
    }

    #[test]
    fn resolve_empty() {
        assert_eq!(resolve(vec![], None), Selection::NoneOnline);
    }

    #[test]
    fn tcp_connection_detected() {
        let d = parse_devices(TCP);
        assert_eq!(d[0].connection(), Connection::Tcp { port: "5555".to_string() });
        assert_eq!(d[0].connection_label(), "TCP:5555");
    }

    #[test]
    fn usb_connection_detected() {
        let d = parse_devices(TWO);
        assert_eq!(d[0].connection(), Connection::Usb);
        assert_eq!(d[0].connection_label(), "USB");
    }
}

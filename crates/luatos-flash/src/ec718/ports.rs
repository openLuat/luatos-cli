//! 下载模式会改变 VID 和端口名；用 USB 插口位置关联重枚举前后的设备。
use super::{ec718_iface_matches, is_eigencomm_usb, reboot_at, FlashProgress, ProgressCallback, BOOT_VID};
use anyhow::{bail, ensure, Context, Result};
use serialport::SerialPortType;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Port {
    name: String,
    boot: bool,
    diag: bool,
    location: Option<String>,
}

fn snapshot() -> Result<Vec<Port>> {
    let mut ports = Vec::new();
    for p in serialport::available_ports().context("无法枚举串口")? {
        if let SerialPortType::UsbPort(usb) = p.port_type {
            if is_eigencomm_usb(usb.vid, usb.pid) {
                ports.push(Port {
                    location: location(&p.port_name),
                    name: p.port_name,
                    boot: usb.vid == BOOT_VID,
                    diag: ec718_iface_matches(usb.interface, 2),
                });
            }
        }
    }
    Ok(ports)
}

// macOS 的 cu.* / tty.* 是同一接口的两个节点，自动选择时优先 cu.*。
fn aliases(a: &str, b: &str) -> bool {
    let canon = |s: &str| s.strip_prefix("/dev/cu.").or_else(|| s.strip_prefix("/dev/tty.")).map(str::to_owned);
    a == b || (cfg!(windows) && a.eq_ignore_ascii_case(b)) || (cfg!(target_os = "macos") && canon(a).is_some() && canon(a) == canon(b))
}

fn one<'a>(mut ports: Vec<&'a Port>, purpose: &str) -> Result<&'a Port> {
    let names: Vec<_> = ports.iter().map(|p| p.name.as_str()).collect();
    ensure!(!ports.is_empty(), "找不到{purpose}，请手动进 Boot 并指定下载口");
    ensure!(
        ports.iter().all(|p| aliases(&p.name, &ports[0].name)),
        "{purpose}不唯一：{}。请用 --port 指定目标板端口",
        names.join("、")
    );
    ports.sort_by_key(|p| p.name.starts_with("/dev/tty."));
    Ok(ports[0])
}

#[derive(Debug, PartialEq)]
enum Target {
    Direct(String),
    Reboot { diag: String, location: String },
    Wait,
}

fn select(ports: &[Port], specified: Option<&str>) -> Result<Target> {
    let target = if let Some(name) = specified {
        let Some(p) = ports.iter().find(|p| aliases(&p.name, name)) else {
            // 显式 UART 或自定义设备路径，仅尝试此口。
            return Ok(Target::Direct(name.to_owned()));
        };
        p
    } else {
        let Some(first) = ports.first() else { return Ok(Target::Wait) };
        if !ports
            .iter()
            .all(|p| aliases(&p.name, &first.name) || (p.location.is_some() && p.location == first.location && p.boot == first.boot))
        {
            let names = ports.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("、");
            bail!("无法唯一确定目标板（候选端口：{names}），请用 --port 指定目标板端口");
        }
        first
    };
    if target.boot {
        return Ok(Target::Direct(target.name.clone()));
    }
    let location = target
        .location
        .as_ref()
        .with_context(|| format!("无法获取 {} 的 USB 物理位置，请手动进 Boot 并指定下载口", target.name))?;
    let diag = one(
        ports.iter().filter(|p| !p.boot && p.diag && p.location.as_ref() == Some(location)).collect(),
        "目标板的 DIAG 口",
    )?;
    Ok(Target::Reboot {
        diag: diag.name.clone(),
        location: location.clone(),
    })
}

fn paired_boot(ports: &[Port], location: &str) -> Result<Option<String>> {
    let matched: Vec<_> = ports.iter().filter(|p| p.boot && p.location.as_deref() == Some(location)).collect();
    if matched.is_empty() {
        Ok(None)
    } else {
        Ok(Some(one(matched, "同一 USB 位置的下载口")?.name.clone()))
    }
}

pub(super) fn reboot_unique(progress: &ProgressCallback) -> Result<bool> {
    Ok(match select(&snapshot()?, None)? {
        Target::Reboot { diag, .. } => reboot_at(&diag, progress),
        _ => false,
    })
}

pub(super) fn enter(user_port: Option<&str>, progress: &ProgressCallback) -> Result<String> {
    let specified = crate::device::normalize_serial_port(user_port);
    let mut target = select(&snapshot()?, specified)?;
    if target == Target::Wait {
        progress(&FlashProgress::info("Waiting", 1.0, "请连接目标模组或手动进入 Boot 模式"));
        let start = Instant::now();
        while target == Target::Wait && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(200));
            target = select(&snapshot()?, None)?;
        }
    }
    match target {
        Target::Direct(port) => Ok(port),
        Target::Wait => bail!("等待目标模组超时"),
        Target::Reboot { diag, location } => {
            ensure!(reboot_at(&diag, progress), "{diag} 进入下载模式失败，请手动进 Boot 并指定下载口");
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                if let Some(name) = paired_boot(&snapshot()?, &location)? {
                    progress(&FlashProgress::info("Detecting", 3.0, &format!("目标板 {diag} 已进入下载模式：{name}")));
                    return Ok(name);
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            bail!("未在 {diag} 原 USB 位置发现下载口，请手动进 Boot 并指定目标板的下载口");
        }
    }
}

#[cfg(target_os = "linux")]
fn location(name: &str) -> Option<String> {
    use std::path::Path;
    let path = Path::new("/sys/class/tty").join(Path::new(name).file_name()?).join("device").canonicalize().ok()?;
    path.ancestors()
        .find(|p| p.join("idVendor").is_file() && p.join("idProduct").is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn location(name: &str) -> Option<String> {
    use std::{mem, ptr};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    // SetupAPI 返回的 MULTI_SZ 中第一项为完整路径；运行态接口末尾还带有 #USBMI(n)。
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVCLASS_PORTS, ptr::null(), ptr::null_mut(), DIGCF_PRESENT);
        if set == -1 {
            return None;
        }
        let suffix = format!("({name})");
        let mut result = None;
        let mut dev: SP_DEVINFO_DATA = mem::zeroed();
        dev.cbSize = mem::size_of_val(&dev) as u32;
        for i in 0.. {
            if SetupDiEnumDeviceInfo(set, i, &mut dev) == 0 {
                break;
            }
            let property = |key| {
                let mut buf = [0u16; 2048];
                let ok = SetupDiGetDeviceRegistryPropertyW(set, &dev, key, ptr::null_mut(), buf.as_mut_ptr().cast(), mem::size_of_val(&buf) as u32, ptr::null_mut());
                (ok != 0).then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|x| *x == 0).unwrap_or(buf.len())]))
            };
            if property(SPDRP_FRIENDLYNAME).is_some_and(|s| s.ends_with(&suffix)) {
                result = property(SPDRP_LOCATION_PATHS).and_then(|p| {
                    let p = p.to_ascii_uppercase();
                    let base = p.split("#USBMI(").next()?;
                    base.contains("#USB(").then(|| base.to_owned())
                });
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        result
    }
}

#[cfg(target_os = "macos")]
fn location(name: &str) -> Option<String> {
    use core_foundation::{
        base::{CFType, TCFType},
        number::CFNumber,
        string::CFString,
    };
    use io_kit_sys::*;
    use std::ptr;
    // 获取该串口最近的 USB 设备的 locationID；不能沿用上游 Hub 的位置。
    struct Object(u32);
    impl Drop for Object {
        fn drop(&mut self) {
            unsafe { IOObjectRelease(self.0) };
        }
    }
    unsafe {
        let property = |obj, key: &str| {
            let raw = IORegistryEntryCreateCFProperty(obj, CFString::new(key).as_concrete_TypeRef(), ptr::null(), 0);
            (!raw.is_null()).then(|| CFType::wrap_under_create_rule(raw))
        };
        let mut iter = 0;
        if IOServiceGetMatchingServices(0, IOServiceMatching(c"IOSerialBSDClient".as_ptr()), &mut iter) != 0 {
            return None;
        }
        let iter = Object(iter);
        loop {
            let mut node = Object(IOIteratorNext(iter.0));
            if node.0 == 0 {
                break;
            }
            if !["IOCalloutDevice", "IODialinDevice"]
                .iter()
                .any(|key| property(node.0, key).and_then(|v| v.downcast::<CFString>()).is_some_and(|s| s.to_string() == name))
            {
                continue;
            }
            loop {
                if IOObjectConformsTo(node.0, c"IOUSBHostDevice".as_ptr()) != 0 || IOObjectConformsTo(node.0, c"IOUSBDevice".as_ptr()) != 0 {
                    return property(node.0, "locationID")?
                        .downcast::<CFNumber>()?
                        .to_i64()
                        .filter(|id| *id != 0)
                        .map(|id| id.to_string());
                }
                let mut parent = 0;
                if IORegistryEntryGetParentEntry(node.0, c"IOService".as_ptr(), &mut parent) != 0 {
                    return None;
                }
                node = Object(parent);
            }
        }
    }
    None
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn location(_: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, location: &str, boot: bool, diag: bool) -> Port {
        Port {
            name: name.into(),
            location: Some(location.into()),
            boot,
            diag,
        }
    }

    #[test]
    fn 指定运行板不被已在下载模式的另一板抢占() {
        let ports = [port("COM4", "B", true, false), port("COM5", "A", false, true), port("COM6", "A", false, false)];
        assert!(select(&ports, None).is_err());
        assert_eq!(
            select(&ports, Some("COM6")).unwrap(),
            Target::Reboot {
                diag: "COM5".into(),
                location: "A".into()
            }
        );
        assert_eq!(select(&ports, Some("COM8")).unwrap(), Target::Direct("COM8".into()));
        assert_eq!(paired_boot(&ports, "A").unwrap(), None);
        let after = [port("COM4", "B", true, false), port("COM7", "A", true, false)];
        assert_eq!(paired_boot(&after, "A").unwrap().as_deref(), Some("COM7"));
        assert!(select(&after, None).is_err());
    }
}

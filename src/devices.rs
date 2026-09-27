//! Find devices that might be a rig: serial ports, paired Bluetooth devices,
//! and software modems (modem73) answering on their control port.
//! `identify` opens a device briefly and sends only "ID".

use std::time::Duration;

use crate::engine::Target;
use crate::link::Link;
use crate::softmodem::ModemControl;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub target: Target,
    pub name: String,
    /// Offers a serial link (Bluetooth SPP or a serial port that may be one).
    pub serial: bool,
    /// Model reported by the device, when identified.
    pub model: Option<String>,
}

/// The most recent scan, kept so device numbers survive a restart.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LastScan {
    pub utc_secs: u64,
    pub devices: Vec<Device>,
}

impl LastScan {
    pub fn load() -> Self {
        crate::config::load("scan.json")
    }
    pub fn save(devices: &[Device]) -> std::io::Result<()> {
        let utc_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        crate::config::save("scan.json", &LastScan { utc_secs, devices: devices.to_vec() })
    }
}

const SPP_UUID: &str = "00001101-0000-1000-8000-00805F9B34FB";

pub fn serial_ports() -> Vec<Device> {
    let Ok(ports) = serialport::available_ports() else { return Vec::new() };
    ports
        .into_iter()
        .filter_map(|p| {
            let name = match &p.port_type {
                serialport::SerialPortType::UsbPort(u) => u.product.clone().unwrap_or_else(|| "USB serial".into()),
                serialport::SerialPortType::BluetoothPort => "Bluetooth serial".into(),
                _ => String::new(),
            };
            let lower = p.port_name.to_ascii_lowercase();
            let is_bt = matches!(p.port_type, serialport::SerialPortType::BluetoothPort) || lower.contains("rfcomm")
                || (lower.starts_with("/dev/cu.") && !lower.contains("bluetooth-incoming"));
            // macOS lists every port twice (tty.* and cu.*); keep cu.* only
            if lower.starts_with("/dev/tty.") {
                return None;
            }
            Some(Device { target: Target::Serial(p.port_name), name, serial: is_bt, model: None })
        })
        .collect()
}

#[cfg(windows)]
pub fn bluetooth_devices() -> Vec<Device> {
    use windows_sys::Win32::System::Registry::*;
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut out = Vec::new();
    unsafe {
        let mut root: HKEY = std::ptr::null_mut();
        let path = wide(r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Devices");
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut root) != 0 {
            return out;
        }
        let mut i = 0;
        loop {
            let mut name = [0u16; 64];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(root, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(),
                             std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                break;
            }
            i += 1;
            let hex = String::from_utf16_lossy(&name[..len as usize]);
            let mac: String = hex.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_ascii_uppercase())
                .collect::<Vec<_>>().join(":");
            let mut dev: HKEY = std::ptr::null_mut();
            let mut label = String::new();
            if RegOpenKeyExW(root, name.as_ptr(), 0, KEY_READ, &mut dev) == 0 {
                let mut buf = [0u8; 256];
                let mut blen = buf.len() as u32;
                let value = wide("Name");
                if RegQueryValueExW(dev, value.as_ptr(), std::ptr::null(), std::ptr::null_mut(), buf.as_mut_ptr(), &mut blen) == 0 {
                    label = String::from_utf8_lossy(&buf[..blen as usize]).trim_end_matches('\0').to_string();
                }
                RegCloseKey(dev);
            }
            out.push(Device { target: Target::Bluetooth(mac), name: label, serial: false, model: None });
        }
        RegCloseKey(root);
    }
    out
}

/// Windows COM ports created for a paired device's Serial Port service: [(COMn, MAC)].
/// Incoming-only ports (no remote address) are left out.
#[cfg(windows)]
pub fn bluetooth_com_ports() -> Vec<(String, String)> {
    use windows_sys::Win32::System::Registry::*;
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut out = Vec::new();
    unsafe {
        let enum_key = |key: HKEY, i: u32| -> Option<String> {
            let mut name = [0u16; 260];
            let mut len = name.len() as u32;
            (RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(),
                           std::ptr::null_mut(), std::ptr::null_mut()) == 0)
                .then(|| String::from_utf16_lossy(&name[..len as usize]))
        };
        let open = |parent: HKEY, sub: &str| -> Option<HKEY> {
            let mut k: HKEY = std::ptr::null_mut();
            (RegOpenKeyExW(parent, wide(sub).as_ptr(), 0, KEY_READ, &mut k) == 0).then_some(k)
        };
        let Some(bthenum) = open(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Enum\BTHENUM") else { return out };
        let mut i = 0;
        while let Some(svc) = enum_key(bthenum, i) {
            i += 1;
            if !svc.to_ascii_uppercase().starts_with(&format!("{{{SPP_UUID}}}")) {
                continue;
            }
            let Some(svc_key) = open(bthenum, &svc) else { continue };
            let mut j = 0;
            while let Some(inst) = enum_key(svc_key, j) {
                j += 1;
                let mac_hex = inst.rsplit('&').next().unwrap_or("").split('_').next().unwrap_or("").to_string();
                if mac_hex.len() != 12 || mac_hex.chars().all(|c| c == '0') {
                    continue;
                }
                if let Some(params) = open(svc_key, &format!(r"{inst}\Device Parameters")) {
                    let mut buf = [0u16; 64];
                    let mut blen = (buf.len() * 2) as u32;
                    if RegQueryValueExW(params, wide("PortName").as_ptr(), std::ptr::null(), std::ptr::null_mut(),
                                        buf.as_mut_ptr() as *mut u8, &mut blen) == 0 {
                        let port = String::from_utf16_lossy(&buf[..(blen as usize / 2)]).trim_end_matches('\0').to_string();
                        let mac = mac_hex.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_ascii_uppercase())
                            .collect::<Vec<_>>().join(":");
                        out.push((port, mac));
                    }
                    RegCloseKey(params);
                }
            }
            RegCloseKey(svc_key);
        }
        RegCloseKey(bthenum);
    }
    out
}

#[cfg(target_os = "linux")]
pub fn bluetooth_devices() -> Vec<Device> {
    let Ok(out) = std::process::Command::new("bluetoothctl").args(["devices", "Paired"]).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, ' ');
            let (_, mac, name) = (parts.next()?, parts.next()?, parts.next().unwrap_or(""));
            crate::bt::is_mac(mac).then(|| Device { target: Target::Bluetooth(mac.to_ascii_uppercase()), name: name.into(), serial: false, model: None })
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn bluetooth_devices() -> Vec<Device> {
    Vec::new() // macOS: paired radios appear as /dev/cu.* serial ports
}

/// modem73 instances answering on their control port.
pub fn software_modems(candidates: &[(&str, &str)]) -> Vec<Device> {
    candidates
        .iter()
        .filter_map(|(control, kiss)| {
            let cfg = ModemControl::new(control).request("get_config", serde_json::json!({})).ok()?;
            let call = cfg.get("callsign").and_then(|v| v.as_str()).unwrap_or("");
            Some(Device {
                target: Target::Modem73 { kiss: kiss.to_string(), control: control.to_string() },
                name: format!("modem73 {call}").trim().to_string(),
                serial: true,
                model: Some("MODEM73".into()),
            })
        })
        .collect()
}

/// Everything that might be a rig; Bluetooth devices are checked for a Serial Port service.
pub fn scan() -> Vec<Device> {
    let mut devs = software_modems(&[("127.0.0.1:8073", "127.0.0.1:8001")]);
    let mut bt = bluetooth_devices();
    #[cfg(any(windows, target_os = "linux"))]
    for d in bt.iter_mut() {
        if let Target::Bluetooth(mac) = &d.target {
            d.serial = crate::bt::spp_channel(mac).ok().flatten().is_some();
        }
    }
    devs.append(&mut bt);
    let mut ports = serial_ports();
    #[cfg(windows)]
    {
        let bt_ports = bluetooth_com_ports();
        for p in ports.iter_mut() {
            if let Target::Serial(name) = &p.target
                && let Some((_, mac)) = bt_ports.iter().find(|(com, _)| com.eq_ignore_ascii_case(name)) {
                    p.serial = true;
                    p.name = format!("Bluetooth serial port of {mac}");
                }
        }
    }
    devs.extend(ports);
    devs
}

/// Open the device, send ID, and return the model it reports.
pub fn identify(d: &Device, open: &dyn Fn(&Target) -> std::io::Result<Box<dyn Link>>) -> Option<String> {
    let mut link = open(&d.target).ok()?;
    std::thread::sleep(Duration::from_millis(300));
    crate::cat::drain(link.as_mut(), Duration::from_millis(300));
    let r = crate::cat::cat(link.as_mut(), "ID", Duration::from_secs(2), false).ok()?;
    r.strip_prefix("ID ").map(|m| m.trim().to_string())
}

/// Radios first, then serial-capable devices, then the rest.
pub fn rank(mut devs: Vec<Device>) -> Vec<Device> {
    devs.sort_by_key(|d| (d.model.is_none(), !d.serial));
    devs
}

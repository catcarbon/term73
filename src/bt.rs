//! Direct Bluetooth RFCOMM links and Serial Port Profile (SPP) channel lookup.
//!
//! Windows: Winsock AF_BTH sockets and WSALookupService (NS_BTH) for SDP.
//! Linux: AF_BLUETOOTH RFCOMM sockets; the SPP channel comes from `sdptool`.
//! macOS: not available here; paired radios appear as /dev/cu.* serial ports.

#[cfg(any(windows, target_os = "linux"))]
use std::io;

#[cfg(any(windows, target_os = "linux"))]
use crate::link::Link;

/// "AA:BB:CC:DD:EE:FF" (or without separators) -> 48-bit address.
pub fn parse_mac(s: &str) -> Option<u64> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

pub fn is_mac(s: &str) -> bool {
    parse_mac(s).is_some() && s.chars().filter(|c| *c == ':' || *c == '-').count() == 5
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::Once;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, NS_BTH, SOCKADDR_BTH};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Networking::WinSock::*;

    static INIT: Once = Once::new();

    fn startup() {
        INIT.call_once(|| unsafe {
            let mut data: WSADATA = std::mem::zeroed();
            WSAStartup(0x0202, &mut data);
        });
    }

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    /// Bluetooth base UUID with a 16-bit service class: 0000xxxx-0000-1000-8000-00805F9B34FB.
    fn bt_uuid(uuid16: u16) -> GUID {
        GUID { data1: uuid16 as u32, data2: 0, data3: 0x1000, data4: [0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB] }
    }

    /// Service records of one class on a remote device: [(service name, RFCOMM channel)].
    pub fn sdp_lookup(mac: &str, uuid16: u16) -> io::Result<Vec<(String, u32)>> {
        startup();
        let mut guid = bt_uuid(uuid16);
        let mut ctx: Vec<u16> = format!("({})", mac.to_ascii_uppercase()).encode_utf16().chain([0]).collect();
        let mut q: WSAQUERYSETW = unsafe { std::mem::zeroed() };
        q.dwSize = std::mem::size_of::<WSAQUERYSETW>() as u32;
        q.lpServiceClassId = &mut guid;
        q.dwNameSpace = NS_BTH;
        q.lpszContext = ctx.as_mut_ptr();
        let flags = LUP_FLUSHCACHE | LUP_RETURN_NAME | LUP_RETURN_ADDR;
        let mut handle: HANDLE = std::ptr::null_mut();
        if unsafe { WSALookupServiceBeginW(&q, flags, &mut handle) } != 0 {
            return Err(last_error());
        }
        let mut found = Vec::new();
        let mut buf = vec![0u64; 1024]; // 8 KiB, 8-byte aligned
        loop {
            let mut size = (buf.len() * 8) as u32;
            let res = buf.as_mut_ptr() as *mut WSAQUERYSETW;
            if unsafe { WSALookupServiceNextW(handle, flags, &mut size, res) } != 0 {
                let err = unsafe { WSAGetLastError() };
                if err == WSA_E_NO_MORE || err == WSAENOMORE {
                    break;
                }
                unsafe { WSALookupServiceEnd(handle) };
                return Err(io::Error::from_raw_os_error(err));
            }
            let r = unsafe { &*res };
            let name = if r.lpszServiceInstanceName.is_null() {
                String::new()
            } else {
                unsafe {
                    let p = r.lpszServiceInstanceName;
                    let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
                }
            };
            for i in 0..r.dwNumberOfCsAddrs as usize {
                let cs = unsafe { &*r.lpcsaBuffer.add(i) };
                let sa = unsafe { &*(cs.RemoteAddr.lpSockaddr as *const SOCKADDR_BTH) };
                found.push((name.clone(), sa.port));
            }
        }
        unsafe { WSALookupServiceEnd(handle) };
        Ok(found)
    }

    pub struct RfcommLink {
        sock: SOCKET,
    }

    unsafe impl Send for RfcommLink {}

    impl RfcommLink {
        pub fn connect(mac: &str, channel: u32) -> io::Result<Self> {
            startup();
            let addr = parse_mac(mac).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad Bluetooth address"))?;
            let sock = unsafe { socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
            if sock == INVALID_SOCKET {
                return Err(last_error());
            }
            let mut sa: SOCKADDR_BTH = unsafe { std::mem::zeroed() };
            sa.addressFamily = AF_BTH;
            sa.btAddr = addr;
            sa.port = channel;
            let rc = unsafe {
                connect(sock, &sa as *const SOCKADDR_BTH as *const SOCKADDR, std::mem::size_of::<SOCKADDR_BTH>() as i32)
            };
            if rc != 0 {
                let e = last_error();
                unsafe { closesocket(sock) };
                return Err(e);
            }
            let timeout_ms: u32 = 50;
            unsafe {
                setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &timeout_ms as *const u32 as *const u8, 4);
            }
            Ok(RfcommLink { sock })
        }
    }

    impl Link for RfcommLink {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            let mut off = 0;
            while off < data.len() {
                let n = unsafe { send(self.sock, data[off..].as_ptr(), (data.len() - off) as i32, 0) };
                if n <= 0 {
                    return Err(last_error());
                }
                off += n as usize;
            }
            Ok(())
        }

        fn read(&mut self) -> io::Result<Vec<u8>> {
            let mut buf = [0u8; 4096];
            let n = unsafe { recv(self.sock, buf.as_mut_ptr(), buf.len() as i32, 0) };
            if n > 0 {
                return Ok(buf[..n as usize].to_vec());
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "radio closed the Bluetooth link"));
            }
            let err = unsafe { WSAGetLastError() };
            if err == WSAETIMEDOUT {
                Ok(Vec::new())
            } else {
                Err(io::Error::from_raw_os_error(err))
            }
        }
    }

    impl Drop for RfcommLink {
        fn drop(&mut self) {
            unsafe { closesocket(self.sock) };
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::process::Command;

    const AF_BLUETOOTH: libc::c_int = 31;
    const BTPROTO_RFCOMM: libc::c_int = 3;

    #[repr(C)]
    struct SockaddrRc {
        family: libc::sa_family_t,
        bdaddr: [u8; 6],
        channel: u8,
    }

    pub fn sdp_lookup(mac: &str, uuid16: u16) -> io::Result<Vec<(String, u32)>> {
        let svc = if uuid16 == 0x1101 { "SP".to_string() } else { format!("0x{uuid16:04x}") };
        let out = Command::new("sdptool").args(["search", "--bdaddr", mac, &svc]).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut found = Vec::new();
        let mut name = String::new();
        for line in text.lines() {
            if let Some(n) = line.trim().strip_prefix("Service Name:") {
                name = n.trim().to_string();
            }
            if let Some(c) = line.trim().strip_prefix("Channel:") {
                if let Ok(ch) = c.trim().parse() {
                    found.push((name.clone(), ch));
                }
            }
        }
        Ok(found)
    }

    pub struct RfcommLink {
        fd: libc::c_int,
    }

    impl RfcommLink {
        pub fn connect(mac: &str, channel: u32) -> io::Result<Self> {
            let addr = parse_mac(mac).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad Bluetooth address"))?;
            let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_STREAM, BTPROTO_RFCOMM) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let b = addr.to_be_bytes();
            let sa = SockaddrRc {
                family: AF_BLUETOOTH as libc::sa_family_t,
                bdaddr: [b[7], b[6], b[5], b[4], b[3], b[2]], // little-endian on the wire
                channel: channel as u8,
            };
            let rc = unsafe {
                libc::connect(fd, &sa as *const SockaddrRc as *const libc::sockaddr,
                              std::mem::size_of::<SockaddrRc>() as libc::socklen_t)
            };
            if rc != 0 {
                let e = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(e);
            }
            let tv = libc::timeval { tv_sec: 0, tv_usec: 50_000 };
            unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void,
                                 std::mem::size_of::<libc::timeval>() as libc::socklen_t);
            }
            Ok(RfcommLink { fd })
        }
    }

    impl Link for RfcommLink {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            let mut off = 0;
            while off < data.len() {
                let n = unsafe { libc::write(self.fd, data[off..].as_ptr() as *const libc::c_void, data.len() - off) };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                off += n as usize;
            }
            Ok(())
        }

        fn read(&mut self) -> io::Result<Vec<u8>> {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                return Ok(buf[..n as usize].to_vec());
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "radio closed the Bluetooth link"));
            }
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) { Ok(Vec::new()) } else { Err(e) }
        }
    }

    impl Drop for RfcommLink {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub use imp::{sdp_lookup, RfcommLink};

pub const SPP_UUID16: u16 = 0x1101;
pub const HSP_AG_UUID16: u16 = 0x1112;

/// RFCOMM channel of the radio's Serial Port service.
#[cfg(any(windows, target_os = "linux"))]
pub fn spp_channel(mac: &str) -> io::Result<Option<u32>> {
    Ok(sdp_lookup(mac, SPP_UUID16)?.first().map(|(_, ch)| *ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parsing() {
        assert_eq!(parse_mac("00:11:22:33:44:55"), Some(0x001122334455));
        assert!(is_mac("00:11:22:33:44:55"));
        assert!(!is_mac("COM10"));
        assert!(!is_mac("/dev/rfcomm0"));
    }
}

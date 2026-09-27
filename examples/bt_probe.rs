//! Read-only check: find a radio's Serial Port channel, connect, and ask for ID and FV.
//! Usage: cargo run --example bt_probe -- <bluetooth address>
#[cfg(any(windows, target_os = "linux"))]
fn main() {
    use std::time::Duration;
    let mac = std::env::args().nth(1).expect("usage: bt_probe <bluetooth address>");
    {
        let ch = term73::bt::spp_channel(&mac).expect("SDP lookup failed").expect("no Serial Port service");
        println!("Serial Port on RFCOMM channel {ch}");
        let mut link = term73::bt::RfcommLink::connect(&mac, ch).expect("connect failed");
        for c in ["ID", "FV", "TN"] {
            println!("{c} -> {:?}", term73::cat::cat(&mut link, c, Duration::from_secs(2), false).unwrap());
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("direct Bluetooth is not available on this platform; use the radio's serial port");
}

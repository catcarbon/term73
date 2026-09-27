//! Turn "the terminal is going away" into an orderly quit, so the rig is always released.
//!
//! Windows: closing the console window, logoff, shutdown or Ctrl+Break call our handler on its own
//! thread, and the process is killed once it returns (after about 5 s for a close). The handler asks
//! the main loop to quit and waits for `finished`.
//! Unix: SIGHUP (terminal closed), SIGTERM and SIGINT ask the main loop to quit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// How long a station may take to acknowledge the disconnect when we are being closed.
pub const CLOSE_WAIT: Duration = Duration::from_millis(2500);

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Cleanup is complete; a waiting close handler may let the process end.
pub fn finished() {
    DONE.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
pub fn install() {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_kind: u32) -> BOOL {
        STOP.store(true, Ordering::SeqCst);
        let end = std::time::Instant::now() + Duration::from_millis(4500);
        while !DONE.load(Ordering::SeqCst) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        1
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(unix)]
pub fn install() {
    extern "C" fn handler(_sig: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    let h = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGHUP, h);
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}

#[cfg(not(any(windows, unix)))]
pub fn install() {}

//! The thing that reboots the host when we stop petting it.
//!
//! See README "Which watchdog device" for why the default is a *second*
//! watchdog rather than the softdog that Proxmox's watchdog-mux holds.

use crate::config::Backend;
use crate::sys;
use std::io;
use std::os::fd::RawFd;

// <linux/watchdog.h>: _IOR('W', 5, int), _IOWR('W', 6, int), _IOWR('W', 7, int)
const WDIOC_KEEPALIVE: libc::c_ulong = 0x8004_5705;
const WDIOC_SETTIMEOUT: libc::c_ulong = 0xC004_5706;
const WDIOC_GETTIMEOUT: libc::c_ulong = 0x8004_5707;

pub enum Watchdog {
    DryRun,
    Device { fd: RawFd },
    Mux { fd: RawFd },
}

impl Watchdog {
    /// Opening a watchdog device ARMS it. Call this last in startup, after
    /// everything that might fail has succeeded.
    pub fn open(backend: &Backend, timeout: u32, dry_run: bool) -> io::Result<Watchdog> {
        if dry_run {
            return Ok(Watchdog::DryRun);
        }
        match backend {
            Backend::Device(path) => {
                let fd = sys::open(path, libc::O_WRONLY)?;
                let mut t: libc::c_int = timeout as _;
                if unsafe { libc::ioctl(fd, WDIOC_SETTIMEOUT as _, &mut t) } < 0 {
                    let e = io::Error::last_os_error();
                    // Disarm before bailing, else the failed start reboots us.
                    let _ = sys::write_all(fd, b"V");
                    sys::close(fd);
                    return Err(e);
                }
                let mut got: libc::c_int = 0;
                unsafe { libc::ioctl(fd, WDIOC_GETTIMEOUT as _, &mut got) };
                crate::log!("watchdog {path} armed, timeout {got} s (asked {timeout})");
                Ok(Watchdog::Device { fd })
            }
            Backend::Mux(path) => {
                // pve-ha-lrm's protocol: connect, write any byte to pet, write
                // 'V' then close to disconnect cleanly. Timeout fixed at 60 s.
                let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut a: libc::sockaddr_un = unsafe { core::mem::zeroed() };
                a.sun_family = libc::AF_UNIX as _;
                if path.len() >= a.sun_path.len() {
                    sys::close(fd);
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                for (i, b) in path.bytes().enumerate() {
                    a.sun_path[i] = b as _;
                }
                let r = unsafe {
                    libc::connect(fd, (&a as *const libc::sockaddr_un).cast(), core::mem::size_of::<libc::sockaddr_un>() as _)
                };
                if r < 0 {
                    let e = io::Error::last_os_error();
                    sys::close(fd);
                    return Err(e);
                }
                crate::log!("connected to watchdog-mux at {path} (60 s client timeout)");
                Ok(Watchdog::Mux { fd })
            }
        }
    }

    pub fn pet(&self) -> io::Result<()> {
        match *self {
            Watchdog::DryRun => Ok(()),
            Watchdog::Device { fd } => {
                let mut dummy: libc::c_int = 0;
                if unsafe { libc::ioctl(fd, WDIOC_KEEPALIVE as _, &mut dummy) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            Watchdog::Mux { fd } => sys::write_all(fd, b"\0"),
        }
    }

    /// Magic close: a clean stop must not reboot the host. Requires the
    /// driver not to be in nowayout mode (softdog's `nowayout=0`, default).
    pub fn disarm(self) {
        match self {
            Watchdog::DryRun => {}
            Watchdog::Device { fd } | Watchdog::Mux { fd } => {
                if let Err(e) = sys::write_all(fd, b"V") {
                    crate::log!("magic close failed: {e}; the host WILL reboot");
                }
                sys::close(fd);
            }
        }
    }
}

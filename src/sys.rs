//! Thin syscall wrappers. Everything here is opened ONCE at startup; the
//! steady-state loop only issues ioctl/pread/write/poll/connect on these fds.

use std::ffi::CString;
use std::io;
use std::net::SocketAddr;
use std::os::fd::RawFd;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn open(path: &str, flags: libc::c_int) -> io::Result<RawFd> {
    let p = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    cvt(unsafe { libc::open(p.as_ptr(), flags | libc::O_CLOEXEC) })
}

pub fn write_all(fd: RawFd, buf: &[u8]) -> io::Result<()> {
    let r = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if r as usize != buf.len() {
        return Err(io::Error::from(io::ErrorKind::WriteZero));
    }
    Ok(())
}

pub fn pread(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let r = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
}

pub fn close(fd: RawFd) {
    unsafe { libc::close(fd) };
}

/// Monotonic seconds, CLOCK_BOOTTIME so a suspend counts as elapsed time.
pub fn now() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as u64
}

pub fn now_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

/// poll() one fd; Ok(true) if any of `events` fired within `ms`.
pub fn poll1(fd: RawFd, events: libc::c_short, ms: i32) -> io::Result<libc::c_short> {
    let mut p = libc::pollfd { fd, events, revents: 0 };
    loop {
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(if r == 0 { 0 } else { p.revents });
    }
}

pub fn mlockall() -> io::Result<()> {
    cvt(unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) }).map(|_| ())
}

/// SSH banner probe: success iff the first bytes read are `SSH-`.
///
/// Since OpenSSH 9.8 sshd forks+execs `sshd-session` before the banner, so a
/// banner proves fork, exec and page-in still work — which is what died in
/// the wedge while ping kept answering. The socket is created per probe (a
/// syscall, not a heap allocation) because a TCP socket cannot be reused.
pub fn ssh_probe(addr: &SocketAddr, timeout_ms: u64) -> bool {
    let deadline = now_ms() + timeout_ms;
    let (fam, sa, len) = sockaddr(addr);
    let fd = unsafe { libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return false;
    }
    let ok = (|| {
        let r = unsafe { libc::connect(fd, (&sa as *const libc::sockaddr_storage).cast(), len) };
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return false;
        }
        let left = |d: u64| d.saturating_sub(now_ms()).min(i32::MAX as u64) as i32;
        if poll1(fd, libc::POLLOUT, left(deadline)).unwrap_or(0) & libc::POLLOUT == 0 {
            return false;
        }
        let mut soerr: libc::c_int = 0;
        let mut sl = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
        unsafe {
            libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, (&mut soerr as *mut libc::c_int).cast(), &mut sl)
        };
        if soerr != 0 {
            return false;
        }
        let mut buf = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            if poll1(fd, libc::POLLIN, left(deadline)).unwrap_or(0) & libc::POLLIN == 0 {
                return false;
            }
            let r = unsafe { libc::read(fd, buf[got..].as_mut_ptr().cast(), 4 - got) };
            if r <= 0 {
                return false;
            }
            got += r as usize;
        }
        &buf == b"SSH-"
    })();
    close(fd);
    ok
}

fn sockaddr(addr: &SocketAddr) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
    let mut ss: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                sin_zero: [0; 8],
            };
            unsafe { core::ptr::write((&mut ss as *mut libc::sockaddr_storage).cast(), sin) };
            (libc::AF_INET, ss, core::mem::size_of::<libc::sockaddr_in>() as _)
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: a.scope_id(),
            };
            unsafe { core::ptr::write((&mut ss as *mut libc::sockaddr_storage).cast(), sin6) };
            (libc::AF_INET6, ss, core::mem::size_of::<libc::sockaddr_in6>() as _)
        }
    }
}

/// sd_notify without libsystemd: one datagram to $NOTIFY_SOCKET. The address
/// is resolved at startup; sending afterwards allocates nothing.
pub struct Notify {
    fd: RawFd,
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
}

impl Notify {
    pub fn from_env() -> Option<Notify> {
        let path = std::env::var_os("NOTIFY_SOCKET")?;
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
        let mut addr: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        if bytes.is_empty() || bytes.len() >= addr.sun_path.len() {
            return None;
        }
        for (i, b) in bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        if bytes[0] == b'@' {
            addr.sun_path[0] = 0; // abstract namespace
        }
        let len = (core::mem::size_of::<libc::sa_family_t>() + bytes.len()) as libc::socklen_t;
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
        (fd >= 0).then_some(Notify { fd, addr, len })
    }

    pub fn send(&self, msg: &str) {
        unsafe {
            libc::sendto(
                self.fd,
                msg.as_ptr().cast(),
                msg.len(),
                libc::MSG_NOSIGNAL,
                (&self.addr as *const libc::sockaddr_un).cast(),
                self.len,
            )
        };
    }
}

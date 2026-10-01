//! Allocation-free logging to stderr.
//!
//! stderr is journald's socket under systemd. If journald is wedged (it was,
//! in the incident this exists for), a blocking write would stall the pet
//! loop and turn a healthy daemon into a reboot. So fd 2 is switched to
//! O_NONBLOCK at startup and a message that does not fit is dropped.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

static DROPPED: AtomicU64 = AtomicU64::new(0);

pub struct Buf {
    data: [u8; 512],
    len: usize,
}

impl Buf {
    pub const fn new() -> Self {
        Buf { data: [0; 512], len: 0 }
    }
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.data.len() - 1 - self.len; // keep one byte for '\n'
        let n = s.len().min(room);
        self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

pub fn set_stderr_nonblocking() {
    unsafe {
        let fl = libc::fcntl(2, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(2, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

pub fn emit(args: fmt::Arguments) {
    let mut b = Buf::new();
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        let _ = write!(b, "({dropped} log lines dropped) ");
    }
    let _ = b.write_fmt(args);
    b.data[b.len] = b'\n';
    b.len += 1;
    let r = unsafe { libc::write(2, b.data.as_ptr().cast(), b.len) };
    if r < 0 {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::emit(format_args!($($t)*)) };
}

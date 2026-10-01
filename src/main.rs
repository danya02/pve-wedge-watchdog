//! pve-wedge-watchdog: pet a kernel watchdog only while the host is healthy.
//!
//! The rescuer must not need the wedged system to work. So the reboot is done
//! by the kernel's watchdog timer, and this process's only job is to *stop*
//! petting it. If this process stalls too — page-in starvation, a blocked
//! write, a crash — the effect is the same: no pet, reboot.
//!
//! Steady state touches only fds opened at startup, allocates nothing on the
//! heap, never forks, never writes files, never resolves names.

mod config;
mod health;
mod log;
mod psi;
mod sys;
mod watchdog;

use config::Config;
use health::{Health, Rules};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use watchdog::Watchdog;

const DEFAULT_CONFIG: &str = "/etc/pve-wedge-watchdog.conf";
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn install_signals() {
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        // No SA_RESTART: poll() must return EINTR so the loop sees STOP.
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, core::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: pve-wedge-watchdog [--config PATH] [--dry-run] [--check-config] [--version]\n\
         Pets a kernel watchdog only while the host is healthy. See README."
    );
    std::process::exit(2);
}

fn main() {
    let mut path = DEFAULT_CONFIG.to_string();
    let (mut force_dry, mut check_only) = (false, false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => path = args.next().unwrap_or_else(|| usage()),
            "--dry-run" | "-n" => force_dry = true,
            "--check-config" => check_only = true,
            "--version" | "-V" => {
                println!("pve-wedge-watchdog {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => usage(),
        }
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{path}: not found; using defaults (dry_run = true)");
            String::new()
        }
        Err(e) => fatal(format_args!("{path}: {e}")),
    };
    let mut cfg = Config::parse(&text).unwrap_or_else(|e| fatal(format_args!("{path}: {e}")));
    if force_dry {
        cfg.dry_run = true;
    }
    if check_only {
        println!("{cfg:#?}");
        return;
    }
    std::process::exit(run(cfg));
}

fn fatal(a: core::fmt::Arguments) -> ! {
    eprintln!("pve-wedge-watchdog: {a}");
    std::process::exit(1);
}

struct Psi {
    fd: RawFd,
    trigger: bool,
}

fn open_psi(cfg: &Config) -> Option<Psi> {
    let trigger = !cfg.psi_trigger.is_empty();
    let flags = if trigger { libc::O_RDWR | libc::O_NONBLOCK } else { libc::O_RDONLY };
    let fd = match sys::open(&cfg.psi_path, flags) {
        Ok(fd) => fd,
        Err(e) => {
            log!("{}: {e}; PSI rules disabled, probe-only", cfg.psi_path);
            return None;
        }
    };
    if trigger {
        // psi_write() overwrites the LAST byte written with NUL, so the
        // terminator must be sent explicitly or the window loses a digit.
        let mut t = cfg.psi_trigger.clone().into_bytes();
        t.push(0);
        if let Err(e) = sys::write_all(fd, &t) {
            log!("PSI trigger {:?} rejected: {e}; falling back to periodic reads", cfg.psi_trigger);
            return Some(Psi { fd, trigger: false });
        }
        log!("PSI trigger armed: {:?}", cfg.psi_trigger);
    }
    Some(Psi { fd, trigger })
}

fn read_psi(psi: &Psi, buf: &mut [u8; 256]) -> Option<psi::Pressure> {
    let n = sys::pread(psi.fd, buf).ok()?;
    psi::parse(&buf[..n])
}

fn run(cfg: Config) -> i32 {
    log::set_stderr_nonblocking();
    install_signals();
    let notify = sys::Notify::from_env();

    if let Err(e) = sys::mlockall() {
        // Not fatal: unlocked, a stall here just means a reboot (fail-safe).
        log!("mlockall failed: {e}; continuing unlocked (set LimitMEMLOCK=infinity)");
    }

    let psi = open_psi(&cfg);
    let sysrq = if cfg.sysrq_reboot && !cfg.dry_run {
        // Writes to /proc/sysrq-trigger are not gated by kernel.sysrq (that
        // mask only applies to the keyboard), so opening it is the precheck.
        match sys::open("/proc/sysrq-trigger", libc::O_WRONLY) {
            Ok(fd) => Some(fd),
            Err(e) => {
                log!("/proc/sysrq-trigger: {e}; sysrq_reboot disabled, relying on the watchdog");
                None
            }
        }
    } else {
        None
    };

    let start = sys::now();
    let mut health = Health::new(
        Rules {
            probe_failures: cfg.probe_failures,
            psi_avg10_threshold: cfg.psi_avg10_threshold,
            psi_hard_threshold: cfg.psi_hard_threshold,
            psi_hard_seconds: cfg.psi_hard_seconds as u64,
            startup_grace: cfg.startup_grace as u64,
        },
        // Grace counts from BOOT (CLOCK_BOOTTIME), not from process start:
        // under Restart=always a crash loop must not re-earn the grace and
        // keep re-arming the pet forever while the host is wedged.
        0,
    );

    // Arm last: everything that can fail at startup has already failed.
    let wd = match Watchdog::open(&cfg.watchdog, cfg.watchdog_timeout, cfg.dry_run) {
        Ok(w) => w,
        Err(e) => {
            log!("cannot open watchdog {:?}: {e} (driver not loaded? PVE blacklists watchdog modules; see README)", cfg.watchdog);
            return 1;
        }
    };
    if cfg.dry_run {
        log!("DRY RUN: watchdog {:?} not opened; verdicts are logged only", cfg.watchdog);
    }
    if let Some(n) = &notify {
        n.send("READY=1\nSTATUS=petting");
    }
    log!(
        "running: pet every {} s, probe {:?} every {} s, psi full avg10 thresholds {}/{} ({} s)",
        cfg.pet_interval, cfg.probe_addr, cfg.probe_interval,
        cfg.psi_avg10_threshold, cfg.psi_hard_threshold, cfg.psi_hard_seconds
    );

    let mut buf = [0u8; 256];
    let mut last_pet = 0u64;
    let mut next_probe = start;
    let mut next_psi_log = start;
    let mut tripped_at: Option<u64> = None;
    let mut sysrq_sent = false;

    while !STOP.load(Ordering::Relaxed) {
        let now = sys::now();

        let p = psi.as_ref().and_then(|p| read_psi(p, &mut buf));
        if psi.is_some() {
            health.on_psi(now, p.map(|p| p.full.avg10));
        }
        if let Some(p) = p {
            if p.full.avg10 >= cfg.psi_avg10_threshold && now >= next_psi_log {
                log!("memory pressure: full avg10={:.2} avg60={:.2} some avg10={:.2}",
                     p.full.avg10, p.full.avg60, p.some.avg10);
                next_psi_log = now + 10;
            }
        }

        if let Some(addr) = cfg.probe_addr {
            if now >= next_probe && health.verdict().is_none() {
                let ok = sys::ssh_probe(&addr, cfg.probe_timeout as u64 * 1000);
                let now = sys::now();
                health.on_probe(now, ok);
                if !ok {
                    log!("ssh probe {addr} FAILED ({} in a row)", health.failures());
                }
                next_probe = now + cfg.probe_interval as u64;
            }
        }

        let now = sys::now();
        match health.verdict() {
            None => {
                if now >= last_pet + cfg.pet_interval as u64 {
                    if let Err(e) = wd.pet() {
                        log!("watchdog pet failed: {e}");
                    }
                    last_pet = now;
                }
            }
            Some(reason) => {
                let at = *tripped_at.get_or_insert_with(|| {
                    if cfg.dry_run {
                        log!("DRY RUN: UNHEALTHY ({reason:?}); would stop petting, reboot in ~{} s", cfg.watchdog_timeout);
                    } else {
                        log!("UNHEALTHY ({reason:?}): stopped petting; the kernel watchdog reboots in <= {} s", cfg.watchdog_timeout);
                        if let Some(n) = &notify {
                            n.send("STATUS=unhealthy, not petting");
                        }
                    }
                    now
                });
                if let (false, Some(fd)) = (cfg.dry_run, sysrq) {
                    if !sysrq_sent && now >= at + cfg.sysrq_grace as u64 {
                        sysrq_sent = true;
                        log!("sysrq_grace elapsed: writing 'b' to /proc/sysrq-trigger");
                        let _ = sys::write_all(fd, b"b");
                    }
                }
            }
        }

        // Sleep until the next due event, waking early on a PSI trigger.
        let now_ms = sys::now_ms();
        let due = [last_pet + cfg.pet_interval as u64, next_probe]
            .into_iter()
            .min()
            .unwrap()
            .max(now + 1);
        let wait_ms = (due * 1000).saturating_sub(now_ms).clamp(100, 5000) as i32;
        match &psi {
            Some(Psi { fd, trigger: true }) => {
                let mut pfd = libc::pollfd { fd: *fd, events: libc::POLLPRI, revents: 0 };
                let r = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
                if r > 0 && pfd.revents & libc::POLLPRI != 0 && health.verdict().is_none() {
                    log!("PSI trigger fired ({:?})", cfg.psi_trigger);
                    // The trigger is edge-ish (fires at most once per window);
                    // don't spin on it, the next iteration re-reads averages.
                    unsafe { libc::poll(core::ptr::null_mut(), 0, 500) };
                }
                if r > 0 && pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    unsafe { libc::poll(core::ptr::null_mut(), 0, wait_ms) };
                }
            }
            _ => unsafe {
                libc::poll(core::ptr::null_mut(), 0, wait_ms);
            },
        }
    }

    // Clean stop. If we already decided the host is wedged, do NOT disarm:
    // `systemctl stop` during a wedge must not cancel the reboot.
    if tripped_at.is_some() && !cfg.dry_run {
        log!("stopping while unhealthy: leaving the watchdog armed");
        return 0;
    }
    if let Some(n) = &notify {
        n.send("STOPPING=1");
    }
    wd.disarm();
    log!("stopped; watchdog disarmed");
    0
}

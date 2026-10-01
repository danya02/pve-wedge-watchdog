//! `/etc/pve-wedge-watchdog.conf`: `key = value` lines, `#` comments.
//!
//! Loaded once at startup; the daemon never re-reads it. An unknown key is an
//! error rather than a warning, because a typo in a safety threshold should
//! stop the service from starting, not silently run with the default.

use std::fmt;
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// A watchdog character device driven with WDIOC_* ioctls.
    Device(String),
    /// Proxmox's watchdog-mux client socket (last resort; see README).
    Mux(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub watchdog: Backend,
    pub dry_run: bool,
    /// Seconds; programmed with WDIOC_SETTIMEOUT.
    pub watchdog_timeout: u32,
    pub pet_interval: u32,
    pub psi_path: String,
    /// Kernel PSI trigger, e.g. `full 500000 2000000`; empty disables it.
    pub psi_trigger: String,
    /// A probe failure while `full avg10` is above this is fatal at once.
    pub psi_avg10_threshold: f32,
    /// `full avg10` above this for `psi_hard_seconds` is fatal on its own.
    pub psi_hard_threshold: f32,
    pub psi_hard_seconds: u32,
    pub probe_addr: Option<SocketAddr>,
    pub probe_interval: u32,
    pub probe_timeout: u32,
    pub probe_failures: u32,
    /// Probe failures are ignored this long after start (sshd may be late).
    pub startup_grace: u32,
    pub sysrq_reboot: bool,
    pub sysrq_grace: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            watchdog: Backend::Device("/dev/watchdog1".into()),
            dry_run: true,
            watchdog_timeout: 60,
            pet_interval: 5,
            psi_path: "/proc/pressure/memory".into(),
            psi_trigger: "full 500000 2000000".into(),
            psi_avg10_threshold: 20.0,
            psi_hard_threshold: 40.0,
            psi_hard_seconds: 180,
            probe_addr: Some("127.0.0.1:22".parse().unwrap()),
            probe_interval: 30,
            probe_timeout: 10,
            probe_failures: 3,
            startup_grace: 120,
            sysrq_reboot: false,
            sysrq_grace: 30,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ConfigError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.msg)
        } else {
            write!(f, "line {}: {}", self.line, self.msg)
        }
    }
}

fn err(line: usize, msg: impl Into<String>) -> ConfigError {
    ConfigError { line, msg: msg.into() }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let mut c = Config::default();
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| err(ln, "expected key = value"))?;
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            macro_rules! num {
                ($t:ty) => {
                    v.parse::<$t>().map_err(|_| err(ln, format!("{k}: bad number {v:?}")))?
                };
            }
            let boolean = || parse_bool(v).ok_or_else(|| err(ln, format!("{k}: bad bool {v:?}")));
            match k {
                "watchdog" => {
                    c.watchdog = match v.strip_prefix("mux:") {
                        Some(p) => Backend::Mux(p.into()),
                        None => Backend::Device(v.into()),
                    }
                }
                "dry_run" => c.dry_run = boolean()?,
                "watchdog_timeout" => c.watchdog_timeout = num!(u32),
                "pet_interval" => c.pet_interval = num!(u32),
                "psi_path" => c.psi_path = v.into(),
                "psi_trigger" => c.psi_trigger = v.into(),
                "psi_avg10_threshold" => c.psi_avg10_threshold = num!(f32),
                "psi_hard_threshold" => c.psi_hard_threshold = num!(f32),
                "psi_hard_seconds" => c.psi_hard_seconds = num!(u32),
                "probe_addr" => {
                    c.probe_addr = if v.is_empty() || v == "none" {
                        None
                    } else {
                        Some(v.parse().map_err(|_| {
                            err(ln, format!("probe_addr: want ip:port (no DNS), got {v:?}"))
                        })?)
                    }
                }
                "probe_interval" => c.probe_interval = num!(u32),
                "probe_timeout" => c.probe_timeout = num!(u32),
                "probe_failures" => c.probe_failures = num!(u32),
                "startup_grace" => c.startup_grace = num!(u32),
                "sysrq_reboot" => c.sysrq_reboot = boolean()?,
                "sysrq_grace" => c.sysrq_grace = num!(u32),
                _ => return Err(err(ln, format!("unknown key {k:?}"))),
            }
        }
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let e = |m: &str| Err(err(0, m));
        if self.pet_interval == 0 || self.probe_interval == 0 || self.probe_timeout == 0 {
            return e("intervals and timeouts must be > 0");
        }
        if self.probe_failures == 0 {
            return e("probe_failures must be >= 1");
        }
        // The probe runs inline in the pet loop, so a probe that hangs for its
        // full timeout must still leave room to pet before the watchdog fires.
        if self.watchdog_timeout < self.pet_interval + self.probe_timeout + 10 {
            return e("watchdog_timeout must be >= pet_interval + probe_timeout + 10");
        }
        if let Backend::Mux(_) = self.watchdog {
            if self.watchdog_timeout != 60 {
                return e("watchdog-mux has a fixed 60 s client timeout; set watchdog_timeout = 60");
            }
        }
        if self.psi_hard_threshold < self.psi_avg10_threshold {
            return e("psi_hard_threshold must be >= psi_avg10_threshold");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default_and_dry_run() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert!(c.dry_run, "an unconfigured install must not arm a watchdog");
    }

    #[test]
    fn full_file() {
        let c = Config::parse(
            "# comment\n\
             watchdog = /dev/watchdog2\n\
             dry_run = no   # trailing\n\
             watchdog_timeout = 90\n\
             psi_trigger = \"some 150000 1000000\"\n\
             psi_avg10_threshold = 12.5\n\
             probe_addr = [::1]:2222\n\
             sysrq_reboot = true\n",
        )
        .unwrap();
        assert_eq!(c.watchdog, Backend::Device("/dev/watchdog2".into()));
        assert!(!c.dry_run);
        assert_eq!(c.watchdog_timeout, 90);
        assert_eq!(c.psi_trigger, "some 150000 1000000");
        assert_eq!(c.psi_avg10_threshold, 12.5);
        assert_eq!(c.probe_addr, Some("[::1]:2222".parse().unwrap()));
        assert!(c.sysrq_reboot);
    }

    #[test]
    fn mux_backend_and_disabled_probe() {
        let c = Config::parse("watchdog = mux:/run/watchdog-mux.sock\nprobe_addr = none").unwrap();
        assert_eq!(c.watchdog, Backend::Mux("/run/watchdog-mux.sock".into()));
        assert_eq!(c.probe_addr, None);
    }

    #[test]
    fn errors() {
        assert_eq!(Config::parse("\nbogus = 1").unwrap_err().line, 2);
        assert!(Config::parse("noequals").is_err());
        assert!(Config::parse("pet_interval = x").is_err());
        assert!(Config::parse("dry_run = maybe").is_err());
        assert!(Config::parse("probe_addr = localhost:22").is_err(), "no DNS");
        assert!(Config::parse("watchdog_timeout = 20").is_err());
        assert!(Config::parse("watchdog = mux:/x\nwatchdog_timeout = 90").is_err());
        assert!(Config::parse("probe_failures = 0").is_err());
    }
}

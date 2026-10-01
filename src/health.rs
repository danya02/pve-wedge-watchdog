//! The health decision, as a pure state machine.
//!
//! Time is injected (monotonic seconds) and so are probe results and PSI
//! samples, so every rule is unit-testable without a host. `main.rs` only
//! feeds it and acts on the verdict.
//!
//! The verdict LATCHES: once unhealthy, the daemon never pets again. Resuming
//! after the stall clears would be easy, but the incident this exists for had
//! long stretches that looked half-alive; a host that wedged badly enough to
//! trip a rule once is rebooted rather than given a second chance to wedge.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// `probe_failures` probes failed in a row.
    ConsecutiveProbeFailures(u32),
    /// A probe failed while `full avg10` exceeded `psi_avg10_threshold`.
    ProbeFailedUnderPressure(f32),
    /// `full avg10` stayed above `psi_hard_threshold` for this many seconds.
    SustainedPressure(u64),
}

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    pub probe_failures: u32,
    pub psi_avg10_threshold: f32,
    pub psi_hard_threshold: f32,
    pub psi_hard_seconds: u64,
    pub startup_grace: u64,
}

#[derive(Debug)]
pub struct Health {
    rules: Rules,
    started: u64,
    failures: u32,
    last_avg10: Option<f32>,
    hard_since: Option<u64>,
    verdict: Option<Reason>,
}

impl Health {
    pub fn new(rules: Rules, now: u64) -> Self {
        Health { rules, started: now, failures: 0, last_avg10: None, hard_since: None, verdict: None }
    }

    pub fn verdict(&self) -> Option<Reason> {
        self.verdict
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    fn trip(&mut self, r: Reason) {
        if self.verdict.is_none() {
            self.verdict = Some(r);
        }
    }

    /// A PSI sample, `full avg10`. `None` means the read failed: the
    /// sustained-pressure clock is left as it is rather than reset, so a
    /// flaky read cannot keep postponing the verdict.
    pub fn on_psi(&mut self, now: u64, full_avg10: Option<f32>) {
        let Some(v) = full_avg10 else { return };
        self.last_avg10 = Some(v);
        if v >= self.rules.psi_hard_threshold {
            let since = *self.hard_since.get_or_insert(now);
            let held = now.saturating_sub(since);
            if held >= self.rules.psi_hard_seconds {
                self.trip(Reason::SustainedPressure(held));
            }
        } else {
            self.hard_since = None;
        }
    }

    pub fn on_probe(&mut self, now: u64, ok: bool) {
        if ok {
            self.failures = 0;
            return;
        }
        // sshd may come up after us at boot; failures then mean nothing.
        if now.saturating_sub(self.started) < self.rules.startup_grace {
            return;
        }
        self.failures += 1;
        if self.failures >= self.rules.probe_failures {
            self.trip(Reason::ConsecutiveProbeFailures(self.failures));
        } else if let Some(v) = self.last_avg10.filter(|v| *v >= self.rules.psi_avg10_threshold) {
            self.trip(Reason::ProbeFailedUnderPressure(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules {
            probe_failures: 3,
            psi_avg10_threshold: 20.0,
            psi_hard_threshold: 40.0,
            psi_hard_seconds: 180,
            startup_grace: 60,
        }
    }

    #[test]
    fn healthy_stays_healthy() {
        let mut h = Health::new(rules(), 0);
        for t in (0..10_000).step_by(30) {
            h.on_psi(t, Some(5.0));
            h.on_probe(t, true);
        }
        assert_eq!(h.verdict(), None);
    }

    #[test]
    fn consecutive_failures_trip() {
        let mut h = Health::new(rules(), 0);
        h.on_probe(100, false);
        h.on_probe(130, false);
        assert_eq!(h.verdict(), None);
        h.on_probe(160, false);
        assert_eq!(h.verdict(), Some(Reason::ConsecutiveProbeFailures(3)));
    }

    #[test]
    fn success_resets_count() {
        let mut h = Health::new(rules(), 0);
        for t in [100, 130, 160, 190, 220, 250] {
            h.on_probe(t, t % 60 != 40); // fail, ok, fail, ok ...
        }
        assert_eq!(h.verdict(), None);
    }

    #[test]
    fn single_failure_under_pressure_trips() {
        // The 2026-10-01 wedge: full avg10 ~30 and sshd silent.
        let mut h = Health::new(rules(), 0);
        h.on_psi(100, Some(30.0));
        h.on_probe(100, false);
        assert_eq!(h.verdict(), Some(Reason::ProbeFailedUnderPressure(30.0)));
    }

    #[test]
    fn failure_under_low_pressure_does_not() {
        let mut h = Health::new(rules(), 0);
        h.on_psi(100, Some(19.9));
        h.on_probe(100, false);
        assert_eq!(h.verdict(), None);
    }

    #[test]
    fn startup_grace_ignores_failures() {
        let mut h = Health::new(rules(), 1000);
        h.on_psi(1000, Some(90.0));
        for t in [1000, 1010, 1020, 1030, 1059] {
            h.on_probe(t, false);
        }
        assert_eq!(h.verdict(), None);
        h.on_probe(1060, false);
        assert!(h.verdict().is_some());
    }

    #[test]
    fn sustained_pressure_trips_without_probe() {
        let mut h = Health::new(rules(), 0);
        for t in (0..180).step_by(5) {
            h.on_psi(t, Some(45.0));
        }
        assert_eq!(h.verdict(), None);
        h.on_psi(180, Some(45.0));
        assert_eq!(h.verdict(), Some(Reason::SustainedPressure(180)));
    }

    #[test]
    fn pressure_dip_resets_clock() {
        let mut h = Health::new(rules(), 0);
        h.on_psi(0, Some(45.0));
        h.on_psi(170, Some(45.0));
        h.on_psi(175, Some(10.0));
        h.on_psi(180, Some(45.0));
        h.on_psi(355, Some(45.0));
        assert_eq!(h.verdict(), None);
        h.on_psi(360, Some(45.0));
        assert!(h.verdict().is_some());
    }

    #[test]
    fn failed_read_does_not_reset_clock() {
        let mut h = Health::new(rules(), 0);
        h.on_psi(0, Some(45.0));
        h.on_psi(100, None);
        h.on_psi(180, Some(45.0));
        assert!(h.verdict().is_some());
    }

    #[test]
    fn verdict_latches() {
        let mut h = Health::new(rules(), 0);
        h.on_psi(100, Some(30.0));
        h.on_probe(100, false);
        let v = h.verdict();
        h.on_psi(110, Some(0.0));
        h.on_probe(110, true);
        assert_eq!(h.verdict(), v);
    }
}

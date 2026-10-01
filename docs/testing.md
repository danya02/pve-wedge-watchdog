# Testing safely

**Never test the armed daemon first on the production host.** A wrong device,
a wrong threshold or a bug means a reboot of a shared machine. Dry-run on
production is fine and is the last step, not the first.

## 1. Unit tests (no host)

```bash
cargo test
```

Covers config parsing (including the shipped file being dry-run), PSI parsing,
and every rule of the health state machine with injected time and probe
results.

The guard hookscript and `pve-passthrough-guard-sync` have a shell harness
with a fake `qm`, fake `/etc/pve/qemu-server`, fake `storage.cfg` and
`/proc/meminfo` (all paths overridable via `PVE_GUARD_*` env vars):

```bash
shellcheck contrib/passthrough-memory-guard.sh contrib/pve-passthrough-guard-sync tests/guard.sh
tests/guard.sh
```

It covers: no hookscript → `qm set`; ours → no-op; foreign → warning, untouched;
non-pinned VM (and `hostpci` only in a snapshot section) → ignored; hugepages
VM → guarded; second run quiet; drifted snippet refreshed; `local` without
snippets → skip; refuse/admit decisions; both bypass paths logged.

On a throwaway PVE VM (nested): install the .deb, check
`journalctl -u pve-passthrough-guard-sync` and `qm config <id> | grep hookscript`
for a VM with `hugepages: 2`, then start it with more memory than is available
and confirm the task log shows `REFUSING`; `touch
/etc/pve-passthrough-guard.d/<id>.skip` and confirm `BYPASS`.

## 2. Dry run anywhere

```bash
cargo build --release
./target/release/pve-wedge-watchdog --dry-run -c pve-wedge-watchdog.conf
```

Point the probe at a closed port to see a verdict without any pressure:

```bash
printf 'probe_addr=127.0.0.1:1\nprobe_interval=2\nprobe_timeout=1\nstartup_grace=0\n' > /tmp/t.conf
./target/release/pve-wedge-watchdog --dry-run -c /tmp/t.conf
# → "DRY RUN: UNHEALTHY (ConsecutiveProbeFailures(3)); would stop petting"
```

## 3. Armed, in a throwaway VM

Use a disposable Debian 13 VM (a nested VM on PVE is fine) with 2-4 GiB RAM,
some swap, and a serial console or VNC so you can watch it reboot.

```bash
modprobe softdog soft_margin=60      # stands in for the host's second device
ls /dev/watchdog*                    # softdog alone gives /dev/watchdog0
apt install ./pve-wedge-watchdog_*.deb openssh-server stress-ng
```

In the VM, `/dev/watchdog0` is free (no watchdog-mux), so set
`watchdog = /dev/watchdog0`, `dry_run = false`, then:

**a. Clean stop must not reboot.**
`systemctl start pve-wedge-watchdog; sleep 30; systemctl stop pve-wedge-watchdog`,
wait 2 minutes. The VM must stay up; the journal says "watchdog disarmed".
(If it reboots, the driver is in `nowayout` mode — check
`cat /sys/module/softdog/parameters/nowayout`.)

**b. A crash must reboot.** `kill -9 $(pidof pve-wedge-watchdog)` and
`systemctl mask --runtime pve-wedge-watchdog` so it does not restart. The VM
reboots after `watchdog_timeout`.

**c. A dead sshd must reboot.** `systemctl stop ssh`. After `startup_grace`
and 3 probes, the journal shows `UNHEALTHY`, and the VM reboots ~60 s later.

**d. The real thing: memory wedge.** With swap enabled:

```bash
stress-ng --vm 4 --vm-bytes 95% --vm-keep --timeout 0
```

Watch `cat /proc/pressure/memory` from the console. Expect a verdict
(`ProbeFailedUnderPressure` or `SustainedPressure`) and a reboot. If the VM
recovers on its own instead (the kernel OOM-kills stress-ng), raise the
pressure with `--vm-bytes 110%` and more swap, or lower the thresholds for the
test.

**e. A stop while wedged must not disarm.** Trigger (c), then
`systemctl stop pve-wedge-watchdog` after the verdict: the VM must still reboot.

## 4. On the host, dry-run only, then arm

1. Install, choose the device per README "Which watchdog device", keep
   `dry_run = true`, enable. Leave it for days, including heavy legitimate
   workloads, and grep the journal for `UNHEALTHY`.
2. Only with zero false verdicts: `dry_run = false`, restart, and be reachable
   for on-site recovery the first time.

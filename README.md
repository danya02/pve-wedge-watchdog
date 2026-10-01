# pve-wedge-watchdog

A small daemon that **pets a kernel watchdog only while the Proxmox VE host is
healthy**. When the host wedges under memory pressure, it stops petting and the
kernel reboots the machine. If the daemon itself stalls, the kernel reboots the
machine anyway.

# ⚠️ Vibecode Alert ⚠️

The code for this project is mostly written by LLM so it may have unexplainable issues.

## Why

A 251 GiB PVE 9 host wedged when a VM with VFIO GPU passthrough was started
with 96 GiB while 86 GiB was available. Passthrough pins all guest RAM, so it
cannot be ballooned or swapped. What followed:

- `MemAvailable` plateaued at 16 GiB, above earlyoom's 4% trigger. earlyoom
  never fired.
- Memory PSI `full avg10` sat at ~30; swap grew.
- sshd, ZeroTier and journald stopped responding. **Ping still worked.**
- A shell-loop watcher (`full avg60 >= 40` for 3 minutes → `systemctl reboot`)
  never fired. It needed fork, exec and a working systemd, none of which it had.
- Recovery was an on-site power cycle.

**A rescuer that needs the wedged system to work cannot be trusted.** So the
reboot here is done by a kernel timer, and the daemon's only job is to *keep
it from firing* while things are fine. Anything that stops the daemon (the
wedge itself, a crash, a page-in stall) has the same effect as a verdict.

## How it decides

Every `pet_interval` seconds it pets the watchdog unless one of these has
happened (thresholds configurable):

| rule | default |
|---|---|
| `probe_failures` consecutive SSH probe failures | 3 failures, probe every 30 s |
| a probe failure while PSI `full avg10 >= psi_avg10_threshold` | 20 |
| PSI `full avg10 >= psi_hard_threshold` for `psi_hard_seconds` | 40 for 180 s |

The **SSH probe** connects to `127.0.0.1:22` and succeeds iff the first bytes
are `SSH-`. Since OpenSSH 9.8, sshd forks and execs `sshd-session` *before*
sending the banner, so a banner proves that fork, exec and page-in still work —
exactly what died while ping kept answering.

**PSI** is read by `pread` on a descriptor opened at startup, and a kernel
PSI trigger (`full 500000 2000000` = 0.5 s of full stall in a 2 s window) wakes
the loop early via `POLLPRI`.

The verdict **latches**: once unhealthy, the daemon never pets again. Probe
failures are ignored for `startup_grace` seconds after *boot* (not after
process start, so a restart loop cannot keep re-earning it).

Optionally (`sysrq_reboot = true`), `sysrq_grace` seconds after the verdict it
also writes `b` to a `/proc/sysrq-trigger` descriptor opened at startup — an
immediate reboot without sync, as a belt to the watchdog's braces.

### Robustness

- `mlockall(MCL_CURRENT|MCL_FUTURE)` at startup; all fds opened at startup.
- Steady state: no forks, no file writes, no heap allocation, no DNS
  (`probe_addr` must be a literal IP).
- Logging writes to a **non-blocking** stderr from a stack buffer; if journald
  is wedged, lines are dropped (and counted) rather than stalling the loop.
- Runs `SCHED_FIFO` 50, `OOMScoreAdjust=-1000`, `MemoryMin=64M`.
- `systemctl stop` does a magic close (`V`) and disarms — **unless** the daemon
  has already decided the host is wedged, in which case it leaves the watchdog
  armed so a stop cannot cancel the reboot.
- `Restart=always` is safe: a crash closes the device without the magic `V`,
  so the timer keeps counting across restarts.

## Which watchdog device

PVE's `watchdog-mux` (always running, HA or not) loads `softdog` and holds
`/dev/watchdog0`. A watchdog device has one opener at a time, so this daemon
needs **its own** device. The options, in order of preference:

1. **A hardware chipset watchdog** — **the default choice.** On AMD EPYC
   that is `sp5100_tco`, on Intel `iTCO_wdt`. It shows up as `/dev/watchdog1`
   beside softdog's `/dev/watchdog0`. It is independent of softdog, and better
   than a second software timer because it fires even if the kernel itself is
   hung. PVE's kernel **blacklists all watchdog modules** by default
   (`/lib/modprobe.d/blacklist_pve-kernel-*.conf`) so that only one is ever
   loaded for HA. **`/etc/modules-load.d/` does not work for this**:
   `systemd-modules-load` honours the blacklist ("Module 'sp5100_tco' is
   deny-listed (by kmod)") and the service then has no device at boot. An
   explicit `modprobe` does not honour it, so load it from the unit:

   ```bash
   mkdir -p /etc/systemd/system/pve-wedge-watchdog.service.d
   cat > /etc/systemd/system/pve-wedge-watchdog.service.d/module.conf <<'EOF'
   [Service]
   ExecStartPre=-/usr/sbin/modprobe sp5100_tco
   EOF
   systemctl daemon-reload
   ls -l /sys/class/watchdog/*/  ; cat /sys/class/watchdog/watchdog1/identity
   ```

   (Use `iTCO_wdt` on Intel.) The package does not ship this drop-in because
   the module name depends on the chipset. Verify with a real reboot that
   the service comes up armed; a crash loop on "cannot open watchdog" means
   the module did not load.

   Check `dmesg` — some boards disable the TCO in firmware, and the driver
   then refuses to load. Point `watchdog =` at whichever `/dev/watchdogN` has
   the right `identity` (numbering follows probe order).

2. **`ipmi_watchdog`** via the BMC, if the BMC works. It survives a hung
   kernel too. (On the motivating host IPMI is broken, so this is not the
   default.)

3. **A second software watchdog — not available.** `softdog` registers exactly
   one device per module instance and has no instance-count parameter, and the
   module cannot be loaded twice. Mainline has no other pure-software watchdog
   driver to pair it with. A renamed out-of-tree copy of softdog (DKMS) would
   work but is a kernel module to maintain; not done here.

4. **Last resort: `watchdog = mux:/run/watchdog-mux.sock`**, talking to
   watchdog-mux the way `pve-ha-lrm` does: connect, write a byte to pet, write
   `V` and close to detach. watchdog-mux's client timeout is a fixed 60 s, so
   `watchdog_timeout` must be 60. Downsides: it depends on watchdog-mux being
   alive (it is a small mlocked C program, but still userspace), and it shares
   its fate with HA's fencing — a client that times out makes watchdog-mux
   stop petting softdog, which reboots the node. That is the intended effect,
   but read `pve-ha-manager`'s docs before combining it with an HA cluster.

In every case, test on a throwaway VM first — see
[docs/testing.md](docs/testing.md).

## Install

Install the `.deb` **on the PVE node**. Grab it from
[Releases](https://github.com/danya02/pve-wedge-watchdog/releases), or from the
build artifacts of any CI run:

```bash
apt install ./pve-wedge-watchdog_0.1.0_amd64.deb
```

The package installs the service **disabled**, and the config it ships has
**`dry_run = true`**: an armed watchdog on a fresh install, before anyone has
picked a device, is how you get a reboot loop. To roll out:

```bash
$EDITOR /etc/pve-wedge-watchdog.conf            # pick the watchdog device
pve-wedge-watchdog --check-config
systemctl enable --now pve-wedge-watchdog       # still dry-run: logs verdicts only
journalctl -fu pve-wedge-watchdog               # watch for false verdicts for a few days
# then: dry_run = false; systemctl restart pve-wedge-watchdog
```

`--dry-run` on the command line forces dry-run regardless of the config; it
never opens the device.

## Configuration

[`pve-wedge-watchdog.conf`](pve-wedge-watchdog.conf), installed to
`/etc/pve-wedge-watchdog.conf`: `key = value`, `#` comments, read once at
startup. Unknown keys are an error, so a typo in a threshold stops the service
instead of silently using the default.

## Prevention: contrib/

These reduce how often the watchdog has to act.

**`contrib/passthrough-memory-guard.sh`** — a PVE hookscript that refuses to
start a VM with `hostpci*` whose `memory` exceeds `MemAvailable` minus a margin
(16 GiB by default; set `PVE_GUARD_MARGIN_MIB` in
`/etc/default/pve-passthrough-guard`):

```bash
cp /usr/share/doc/pve-wedge-watchdog/examples/passthrough-memory-guard.sh /var/lib/vz/snippets/
chmod +x /var/lib/vz/snippets/passthrough-memory-guard.sh
qm set <vmid> --hookscript local:snippets/passthrough-memory-guard.sh
```

**earlyoom** — still worth running, but **without `--prefer kvm`** (or any
`--prefer` matching QEMU). A passthrough VM's RAM is pinned; killing the QEMU
process is the one kill that frees a lot of memory, but earlyoom preferring it
means a transient spike shoots a production VM. Let the OOM score decide, and
consider `-m 10` so it fires before the plateau seen above. It is a mitigation
for the common case; this daemon is for the case where it does not fire.

## Development

```bash
cargo test                 # pure modules: config, PSI parsing, health state machine
cargo build --release
./target/release/pve-wedge-watchdog --dry-run -c pve-wedge-watchdog.conf
```

The only dependency is `libc`. The `.deb` is built by CI in a `debian:trixie`
container against Debian's packaged crates. For a fully static binary:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

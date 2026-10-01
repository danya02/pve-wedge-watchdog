## What this is

`pve-wedge-watchdog` — a Rust daemon that pets a kernel watchdog device only
while a Proxmox VE 9 host is healthy (memory PSI + local SSH banner probe).
Runs on the PVE node as a systemd service. Rationale and incident: README.

## Layout

| module | role |
|---|---|
| `src/config.rs` | `key = value` config, **pure**, unit-tested |
| `src/psi.rs` | allocation-free `/proc/pressure/*` parser, **pure** |
| `src/health.rs` | the verdict state machine, **pure**; time and probe results injected |
| `src/watchdog.rs` | device (WDIOC_* ioctls, magic close) and watchdog-mux backends |
| `src/sys.rs` | libc wrappers: SSH probe, sd_notify, clocks |
| `src/log.rs` | stack-buffer logging to non-blocking stderr |
| `src/main.rs` | startup (open everything, mlockall, arm last) and the loop |
| `contrib/` | guard hookscript (passthrough/hugepages VMs) + `pve-passthrough-guard-sync`; shipped, sync enabled |
| `tests/guard.sh` | fake-`qm` harness for both scripts (run by CI with shellcheck) |

**Keep decisions in `health.rs` and I/O out of it.** That is what makes every
rule testable without a host.

## Invariants — do not re-litigate

1. **The rescuer must not need the wedged system.** The kernel watchdog does the
   reboot; the daemon only withholds pets. Never replace this with
   `systemctl reboot`, a fork, or anything that needs userspace to work.
2. **Steady state: no fork, no file writes, no heap allocation, no DNS, no
   blocking log write.** Everything is opened at startup. `probe_addr` is a
   literal IP on purpose.
3. **Ship safe.** Package installs the unit disabled and not started; the
   shipped config is `dry_run = true`; a test enforces the latter.
4. **The verdict latches**, and a stop after a verdict does **not** disarm.
5. **Startup grace counts from boot**, so `Restart=always` cannot postpone a
   reboot by crash-looping.
6. **Not /dev/watchdog0**: watchdog-mux holds softdog there. See README
   "Which watchdog device".
7. Dependencies: `libc` only. No tokio, no serde.

## Kernel quirks learned the hard way

- **PSI trigger writes need an explicit trailing NUL**: `psi_write()` replaces
  the last byte written with NUL, so `full 500000 2000000` without one became
  window `200000` and was rejected with EINVAL. Verified on Linux 7.2.
- PSI triggers need the fd opened `O_RDWR`; poll for `POLLPRI`.

## Testing and packaging

```bash
cargo test && cargo build --release
```

Testing on a host: [docs/testing.md](docs/testing.md) — throwaway VM first,
never the production host first.

The `.deb` is built by `.github/workflows/build-deb.yml` in `debian:trixie`
against Debian's `librust-libc-dev` (offline; `debian/rules` drops Cargo.lock
for that build). **Bumping the version means editing `Cargo.toml` and
`debian/changelog`**; CI fails on a mismatch. No local deb build (Arch has no
debhelper) — let CI build it.

The production host is a shared lab server; see the sibling
`pve-pci-assignment-manager` CLAUDE.md for access. Ask before arming anything
there.

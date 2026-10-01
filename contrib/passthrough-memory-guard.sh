#!/bin/bash
# Proxmox hookscript: refuse to start a VM whose RAM is pinned up front --
# PCI passthrough (hostpci*) or hugepages -- when that RAM would not fit in
# MemAvailable minus a margin.
#
# Why only these VMs: a normal VM allocates lazily and its RAM can be
# ballooned or swapped, so overcommit shows up as gradual pressure that
# earlyoom/the OOM killer and pve-wedge-watchdog handle. Passthrough pins ALL
# guest RAM at start (VFIO must map it for DMA); a `hugepages:` VM likewise
# takes all of it up front from unswappable huge pages. Neither can give it
# back. On 2026-10-01 a 96 GiB passthrough VM started with 86 GiB available
# wedged the host.
#
# Installed by the pve-wedge-watchdog package to
# /usr/share/pve-wedge-watchdog/, copied to /var/lib/vz/snippets/ and attached
# to every pinned VM by pve-passthrough-guard-sync. By hand:
#   qm set <vmid> --hookscript local:snippets/passthrough-memory-guard.sh
#
# /etc/default/pve-passthrough-guard (shell syntax):
#   PVE_GUARD_MARGIN_PCT  margin as % of MemTotal (default 10)
#   PVE_GUARD_MARGIN_MIB  absolute margin in MiB; wins if set
#   PVE_GUARD_SKIP="200 9000"   vmids allowed past the guard
# Or create /etc/pve-passthrough-guard.d/<vmid>.skip to bypass one VM.
# Every bypass is logged to stderr, i.e. the PVE task log.
#
# Paths are overridable via env for testing: PVE_GUARD_DEFAULTS,
# PVE_GUARD_SKIP_DIR, PVE_GUARD_MEMINFO, PVE_GUARD_QM.

set -u
vmid="$1"
phase="$2"

[ "$phase" = "pre-start" ] || exit 0

DEFAULTS="${PVE_GUARD_DEFAULTS:-/etc/default/pve-passthrough-guard}"
SKIP_DIR="${PVE_GUARD_SKIP_DIR:-/etc/pve-passthrough-guard.d}"
MEMINFO="${PVE_GUARD_MEMINFO:-/proc/meminfo}"
QM="${PVE_GUARD_QM:-qm}"

PVE_GUARD_MARGIN_PCT=10
PVE_GUARD_SKIP=""
# shellcheck disable=SC1090
[ -r "$DEFAULTS" ] && . "$DEFAULTS"

for s in $PVE_GUARD_SKIP; do
    if [ "$s" = "$vmid" ]; then
        echo "guard: !!! BYPASS: VM $vmid is listed in PVE_GUARD_SKIP ($DEFAULTS);" >&2
        echo "guard: !!! starting WITHOUT the pinned-memory check." >&2
        exit 0
    fi
done
if [ -e "$SKIP_DIR/$vmid.skip" ]; then
    echo "guard: !!! BYPASS: $SKIP_DIR/$vmid.skip exists;" >&2
    echo "guard: !!! starting VM $vmid WITHOUT the pinned-memory check." >&2
    exit 0
fi

conf=$("$QM" config "$vmid") || { echo "guard: cannot read config of VM $vmid" >&2; exit 1; }

why=""
grep -q '^hostpci[0-9]*:' <<<"$conf" && why="PCI passthrough"
grep -q '^hugepages:' <<<"$conf" && why="${why:+$why and }hugepages"
[ -n "$why" ] || exit 0

total_kib=$(awk '/^MemTotal:/ {print $2}' "$MEMINFO")
MARGIN_MIB="${PVE_GUARD_MARGIN_MIB:-$((total_kib * PVE_GUARD_MARGIN_PCT / 100 / 1024))}"

# PVE 8.2+ also accepts the property-string form "memory: current=N".
mem_mib=$(sed -n 's/^memory: *\(current=\)\{0,1\}\([0-9]*\).*/\2/p' <<<"$conf")
mem_mib="${mem_mib:-512}"   # PVE default when unset
avail_kib=$(awk '/^MemAvailable:/ {print $2}' "$MEMINFO")
avail_mib=$((avail_kib / 1024))
budget_mib=$((avail_mib - MARGIN_MIB))

if [ "$mem_mib" -gt "$budget_mib" ]; then
    echo "guard: REFUSING to start VM $vmid: it has $why, which pins all" >&2
    echo "guard: ${mem_mib} MiB of guest RAM, but MemAvailable is ${avail_mib} MiB and the" >&2
    echo "guard: margin is ${MARGIN_MIB} MiB (budget ${budget_mib} MiB)." >&2
    echo "guard: to override: touch $SKIP_DIR/$vmid.skip" >&2
    exit 1
fi
echo "guard: VM $vmid ($why): ${mem_mib} MiB pinned fits (available ${avail_mib} MiB, margin ${MARGIN_MIB} MiB)"
exit 0

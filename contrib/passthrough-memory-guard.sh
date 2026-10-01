#!/bin/bash
# Proxmox hookscript: refuse to start a VM with PCI passthrough (hostpci*)
# whose memory would not fit in MemAvailable minus a margin.
#
# Passthrough pins ALL guest RAM up front (VFIO must map it for DMA), so
# unlike a normal VM it cannot be ballooned or swapped later. On 2026-10-01 a
# 96 GiB passthrough VM started with 86 GiB available wedged the host.
#
# Install:
#   cp passthrough-memory-guard.sh /var/lib/vz/snippets/
#   chmod +x /var/lib/vz/snippets/passthrough-memory-guard.sh
#   qm set <vmid> --hookscript local:snippets/passthrough-memory-guard.sh
#
# Margin: PVE_GUARD_MARGIN_MIB in /etc/default/pve-passthrough-guard
# (default 16384). A VM can be forced past the guard by removing the
# hookscript (qm set <vmid> --delete hookscript).

set -u
vmid="$1"
phase="$2"

[ "$phase" = "pre-start" ] || exit 0

MARGIN_MIB=16384
[ -r /etc/default/pve-passthrough-guard ] && . /etc/default/pve-passthrough-guard
MARGIN_MIB="${PVE_GUARD_MARGIN_MIB:-$MARGIN_MIB}"

conf=$(qm config "$vmid") || { echo "guard: cannot read config of VM $vmid" >&2; exit 1; }

if ! grep -q '^hostpci[0-9]*:' <<<"$conf"; then
    exit 0
fi

# PVE 8.2+ also accepts the property-string form "memory: current=N".
mem_mib=$(sed -n 's/^memory: *\(current=\)\{0,1\}\([0-9]*\).*/\2/p' <<<"$conf")
mem_mib="${mem_mib:-512}"   # PVE default when unset
avail_kib=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
avail_mib=$((avail_kib / 1024))
budget_mib=$((avail_mib - MARGIN_MIB))

if [ "$mem_mib" -gt "$budget_mib" ]; then
    echo "guard: REFUSING to start VM $vmid: it has PCI passthrough, which pins all" >&2
    echo "guard: ${mem_mib} MiB of guest RAM, but MemAvailable is ${avail_mib} MiB and the" >&2
    echo "guard: margin is ${MARGIN_MIB} MiB (budget ${budget_mib} MiB)." >&2
    exit 1
fi
echo "guard: VM $vmid: ${mem_mib} MiB pinned fits (available ${avail_mib} MiB, margin ${MARGIN_MIB} MiB)"
exit 0

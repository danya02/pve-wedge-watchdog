#!/bin/bash
# Shell harness for the guard hookscript and the sync script. No PVE needed.
# check() evals its condition later, so single quotes are intended.
# shellcheck disable=SC2016,SC2034
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
fail=0
ok() { echo "ok   - $1"; }
bad() { echo "FAIL - $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# --- fake PVE -------------------------------------------------------------
mkdir -p "$t/conf" "$t/snip" "$t/skip" "$t/bin"
cat > "$t/bin/qm" <<'Q'
#!/bin/bash
case "$1" in
  config) sed '/^\[/,$d' "$FAKE_CONF/$2.conf" ;;
  set) echo "qm $*" >> "$FAKE_LOG"; echo "hookscript: $4" >> "$FAKE_CONF/$2.conf" ;;
esac
Q
chmod +x "$t/bin/qm"
export FAKE_CONF="$t/conf" FAKE_LOG="$t/qm.log"
export PVE_GUARD_QM="$t/bin/qm" PVE_GUARD_CONF_DIR="$t/conf" PVE_GUARD_SNIPPETS_DIR="$t/snip" \
       PVE_GUARD_SRC="$here/contrib/passthrough-memory-guard.sh" PVE_GUARD_STORAGE_CFG="$t/storage.cfg" \
       PVE_GUARD_DEFAULTS="$t/defaults" PVE_GUARD_SKIP_DIR="$t/skip" PVE_GUARD_MEMINFO="$t/meminfo"
printf 'dir: local\n\tpath /var/lib/vz\n\tcontent iso,vztmpl,backup,snippets\n\nlvmthin: local-lvm\n\tcontent images\n' > "$t/storage.cfg"
printf 'memory: 4096\nhostpci0: 0000:01:00\n' > "$t/conf/100.conf"
printf 'memory: 4096\nhostpci0: 0000:02:00\nhookscript: local:snippets/passthrough-memory-guard.sh\n' > "$t/conf/101.conf"
printf 'memory: 4096\nhostpci0: 0000:03:00\nhookscript: local:snippets/other.pl\n' > "$t/conf/102.conf"
printf 'memory: 4096\nnet0: virtio\n[snap1]\nhostpci0: 0000:04:00\n' > "$t/conf/103.conf"
printf 'memory: 2048\nhugepages: 1024\n' > "$t/conf/104.conf"
sync="$here/contrib/pve-passthrough-guard-sync"

# --- sync -----------------------------------------------------------------
out=$("$sync" 2>"$t/err"); rc=$?
check "sync exits 0" '[ $rc -eq 0 ]'
check "snippet installed" 'cmp -s "$PVE_GUARD_SRC" "$t/snip/passthrough-memory-guard.sh" && [ -x "$t/snip/passthrough-memory-guard.sh" ]'
check "no hook -> set (100)" 'grep -qx "qm set 100 --hookscript local:snippets/passthrough-memory-guard.sh" "$t/qm.log"'
check "hugepages -> set (104)" 'grep -q "qm set 104 " "$t/qm.log"'
check "ours -> noop (101)" '! grep -q "qm set 101" "$t/qm.log"'
check "foreign -> untouched (102)" '! grep -q "qm set 102" "$t/qm.log"'
check "foreign -> warning" 'grep -q "VM 102 .*different hookscript" "$t/err"'
check "non-pinned / snapshot-only hostpci -> ignored (103)" '! grep -q "qm set 103" "$t/qm.log"'
: > "$t/qm.log"
out=$("$sync" 2>/dev/null)
check "second run: no qm set" '[ ! -s "$t/qm.log" ]'
check "second run: quiet" '[ -z "$out" ]'
echo "tampered" >> "$t/snip/passthrough-memory-guard.sh"
out=$("$sync" 2>/dev/null)
check "drifted snippet refreshed" 'cmp -s "$PVE_GUARD_SRC" "$t/snip/passthrough-memory-guard.sh" && grep -q installed <<<"$out"'
printf 'dir: local\n\tcontent iso,vztmpl\n' > "$t/storage.cfg"; rm -rf "$t/snip"; : > "$t/qm.log"
printf 'memory: 1\nhostpci0: x\n' > "$t/conf/105.conf"
out=$("$sync" 2>&1); rc=$?
check "no snippets content -> skip, no changes" '[ $rc -eq 0 ] && [ ! -e "$t/snip" ] && [ ! -s "$t/qm.log" ] && grep -q "no .snippets." <<<"$out"'

# --- guard ----------------------------------------------------------------
guard="$here/contrib/passthrough-memory-guard.sh"
printf 'MemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n' > "$t/meminfo"   # 16 GiB / 8 GiB, margin 1638 MiB
printf 'memory: 16384\nhostpci0: x\n' > "$t/conf/200.conf"
printf 'memory: 16384\nhugepages: 2\n' > "$t/conf/201.conf"
printf 'memory: 16384\n' > "$t/conf/202.conf"
printf 'memory: 4096\nhostpci0: x\n' > "$t/conf/203.conf"
"$guard" 200 pre-start 2>"$t/err"; check "guard refuses oversize passthrough" '[ $? -eq 1 ] && grep -q REFUSING "$t/err"'
"$guard" 201 pre-start 2>"$t/err"; check "guard refuses oversize hugepages" '[ $? -eq 1 ] && grep -q hugepages "$t/err"'
"$guard" 202 pre-start >/dev/null 2>&1; check "guard ignores normal VM" '[ $? -eq 0 ]'
"$guard" 203 pre-start >/dev/null 2>&1; check "guard admits fitting VM" '[ $? -eq 0 ]'
"$guard" 200 post-stop >/dev/null 2>&1; check "guard ignores other phases" '[ $? -eq 0 ]'
echo 'PVE_GUARD_SKIP="9000 200"' > "$t/defaults"
"$guard" 200 pre-start 2>"$t/err"; check "PVE_GUARD_SKIP bypass, logged" '[ $? -eq 0 ] && grep -q "BYPASS.*PVE_GUARD_SKIP" "$t/err"'
"$guard" 201 pre-start 2>/dev/null; check "PVE_GUARD_SKIP only that vmid" '[ $? -eq 1 ]'
rm "$t/defaults"; touch "$t/skip/201.skip"
"$guard" 201 pre-start 2>"$t/err"; check "skip file bypass, logged" '[ $? -eq 0 ] && grep -q "BYPASS.*201.skip" "$t/err"'
echo 'PVE_GUARD_MARGIN_MIB=0' > "$t/defaults"
printf 'memory: current=8000\nhostpci0: x\n' > "$t/conf/204.conf"
"$guard" 204 pre-start >/dev/null 2>&1; check "absolute margin + current= form" '[ $? -eq 0 ]'

exit $fail

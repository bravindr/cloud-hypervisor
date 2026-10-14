#!/bin/bash
# restore_cells2.sh <snapshot-dir> <tag> <cell>...   cell = name:cpus:workers:uffd_dma:dcbm:args
# Restores the same checkpoint into a fresh CH over receive-migration, the
# daemon pinned to <cpus>, and records wall, daemon CPU, daemon log lines and
# the uffd_dma / dcbm counters. REPS (default 3). PERF=1 records the daemon.
set -u
SNAPDIR=${1:?snapshot dir}; TAG=${2:?tag}; shift 2
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote
OD=${OFFLOAD_BIN:-$HOME/cloud-hypervisor/target-iaa2/release/offload_daemon}
OUT=$HOME/chlogs/restore2-$TAG-$(date +%m%d_%H%M); mkdir -p "$OUT"
RSOCK=$OUT/api.sock; RDATA=$OUT/data.sock
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
REPS=${REPS:-3}
log() { echo "$(date +%H:%M:%S) $*" | tee -a "$OUT/summary.txt"; }
cfg() { python3 -c "import json,sys;c=json.load(open('$SNAPDIR/migration_config.json'))['vm_config'];print($1)"; }
DISK=$(cfg "c['disks'][0]['path']"); CONSOLE=$(cfg "c['serial']['file']")
SESSION=$(basename "$DISK" | sed 's/-\(4k\|2m\)\.run\.ext4$//')
TEMPLATE=$HOME/vmbench/rootfs_traj_$SESSION.ext4
log "snapshot $SNAPDIR disk $DISK console $CONSOLE binary $OD"
ctr() { for a in copies_done copies_refused copies_failed bytes_dma pin_us_total map_us_total wait_ns_total src_contig striped_copies dst_cached_segs dst_mapped_segs src_registered refused_small refused_unaligned refused_stalled refused_saturated refused_noclaim src_map_us_total dst_map_us_total regbufs_invalidated; do printf "%s=%s " "$a" "$(cat /sys/kernel/uffd_dma/$a)"; done; printf "dcbm_cleared=%s" "$(cat /sys/module/dcbm/parameters/folios_cleared)"; echo; }
knob() { echo "$2" | sudo tee "$1" >/dev/null; }
restore_one() { # name cpus workers args...
  local name=$1 cpus=$2 workers=$3; shift 3
  cp --sparse=always "$TEMPLATE" "$DISK"
  mkdir -p "$(dirname "$CONSOLE")"; : > "$CONSOLE"
  rm -f "$RSOCK" "$RDATA"
  numactl -C 0-3 -m 0 "$CH" --api-socket "$RSOCK" > "$OUT/ch_$name.log" 2>&1 &
  local chpid=$!
  for _ in $(seq 1 200); do [ -S "$RSOCK" ] && break; sleep 0.05; done
  "$REM" --api-socket "$RSOCK" receive-migration "receiver_url=unix:$RDATA" > /dev/null 2>&1 & local recv=$!
  for _ in $(seq 1 200); do [ -S "$RDATA" ] && break; sleep 0.05; done
  local before; before=$(ctr)
  local pfx=""; [ "${PERF:-0}" = 1 ] && pfx="sudo -E perf record -F 1999 -g -o $OUT/perf_$name.data --"
  local t0=$(date +%s%N)
  $pfx /usr/bin/time -f "%e %U %S" -o "$OUT/t_$name.txt" taskset -c "$cpus" "$OD" restore --socket "$RDATA" --input-dir "$SNAPDIR" --resume --workers "$workers" "$@" > "$OUT/d_$name.log" 2>&1
  local rc=$? t1=$(date +%s%N); wait "$recv" 2>/dev/null
  local gok=0 g0=$(date +%s%N) gms=""; for _ in $(seq 1 600); do grep -aq "AGENT_DIFF_POINT" "$CONSOLE" 2>/dev/null && { gok=1; gms=$(( ($(date +%s%N) - g0) / 1000000 )); break; }; sleep 0.1; done
  local tm; tm=$(tail -1 "$OUT/t_$name.txt")
  log "$name rc=$rc wall $(( (t1 - t0) / 1000000 )) ms, daemon user/sys $(echo "$tm" | cut -d" " -f2,3), guest ok=$gok${gms:+ after $gms ms}"
  grep -hE "Decompressed|Populated|Applied|crc verify|rror" "$OUT/d_$name.log" | sed -E "s/^.*offload_daemon(::[a-z_]+)?\] /    /" | cut -c1-200 | tee -a "$OUT/summary.txt"
  local after; after=$(ctr)
  grep -v "^none" /sys/module/mm_offload_dma/parameters/map_cache_stats 2>/dev/null | sed "s/^/    cache: /" | tee -a "$OUT/summary.txt"
  python3 - "$before" "$after" <<'PY' | tee -a "$OUT/summary.txt"
import sys
b=dict(x.split("=") for x in sys.argv[1].split()); a=dict(x.split("=") for x in sys.argv[2].split())
d={k:int(a[k])-int(b[k]) for k in a}
if any(d.values()): print("    counters:", " ".join(f"{k}={v}" for k,v in d.items() if v))
PY
  "$REM" --api-socket "$RSOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$chpid" 2>/dev/null; wait "$chpid" 2>/dev/null
}
for cell in "$@"; do
  IFS=: read -r name cpus workers uffd dcbm args <<<"$cell"
  knob /sys/kernel/uffd_dma/offloading "$uffd"; knob /sys/module/dcbm/parameters/offloading "$dcbm"
  log "== cell $name: cpus=$cpus workers=$workers uffd_dma=$uffd dcbm=$dcbm args='$args'"
  echo 1 | sudo tee /sys/kernel/mm_offload_dma/window >/dev/null 2>&1
  for r in $(seq 1 "$REPS"); do restore_one "${name}_r$r" "$cpus" "$workers" $args; done
  { echo "    -- mm_offload_dma window over the cell:"; sed "s/^/    /" /sys/kernel/mm_offload_dma/window 2>/dev/null; } | tee -a "$OUT/summary.txt"
done
knob /sys/kernel/uffd_dma/offloading 0; knob /sys/module/dcbm/parameters/offloading 0
log "DONE -> $OUT"

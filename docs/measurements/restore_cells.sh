#!/bin/bash
# restore_cells.sh <snapshot-dir> -- restore cells without re-snapshotting:
# populate none/cpu/dsa on 4 KiB and 2 MiB memfds, each with --verify-crc,
# checking the restored guest progresses; then a manifest-CRC corruption.
set -u
SNAPDIR=${1:?snapshot dir}
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote; OD=$B/offload_daemon
OUT=$HOME/chlogs/restorecells_$(date +%Y%m%d_%H%M); mkdir -p $OUT
RSOCK=/tmp/chrc_api.sock; RDATA=/tmp/chrc_data.sock
DTOENV="DTO_WQ_LIST=wq0.0;wq2.0;wq4.0;wq6.0 DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info"
log() { echo "$(date +%H:%M:%S) $*" | tee -a $OUT/summary.txt; }
CONSOLE=$(sudo python3 -c "import json;c=json.load(open('$SNAPDIR/config.json'));print(c['serial']['file'])" 2>/dev/null)
[ -n "$CONSOLE" ] || CONSOLE=$(sudo grep -oE '"file":"[^"]+console.log"' $SNAPDIR/*.json | head -1 | cut -d'"' -f4)
log "snapshot $SNAPDIR, guest console $CONSOLE"
restore() {  # restore <name> <args...>
  local name=$1; shift
  sudo rm -f $RSOCK $RDATA; sudo truncate -s 0 $CONSOLE 2>/dev/null
  sudo numactl -N0 -m0 $CH --api-socket $RSOCK -v > $OUT/ch_$name.log 2>&1 &
  local RPID=$!; for i in $(seq 1 100); do sudo test -S $RSOCK && break; sleep 0.1; done
  sudo $REM --api-socket $RSOCK receive-migration receiver_url=unix:$RDATA > $OUT/recv_$name.log 2>&1 &
  local RECV=$!; for i in $(seq 1 100); do sudo test -S $RDATA && break; sleep 0.1; done
  local t0=$(date +%s%N)
  sudo env $DTOENV /usr/bin/time -f "wall=%e user=%U sys=%S" -o $OUT/time_$name.txt numactl -N0 -m0 \
    $OD restore --socket $RDATA --input-dir $SNAPDIR --resume --workers 8 "$@" > $OUT/daemon_$name.log 2>&1
  local rc=$?; local t1=$(date +%s%N); wait $RECV 2>/dev/null
  local prog=n/a
  if [ $rc -eq 0 ]; then prog=no; for i in $(seq 1 300); do sudo grep -q AGENT_DIFF_POINT $CONSOLE 2>/dev/null && { prog=yes; break; }; sleep 0.1; done; fi
  log "$name: rc=$rc wall $(( (t1-t0)/1000000 )) ms $(tail -1 $OUT/time_$name.txt) guest-progressed=$prog"
  grep -hE "Populated|populate:|crc verify|Decompressed|Error" $OUT/daemon_$name.log | sed -E "s/^.*(INFO|ERROR) +offload_daemon(::compression)?\] /    /" | cut -c1-170 | tee -a $OUT/summary.txt
  sudo $REM --api-socket $RSOCK shutdown-vmm >/dev/null 2>&1; sleep 1; sudo kill $RPID 2>/dev/null; wait $RPID 2>/dev/null
}
restore p4k_none     --verify-crc
restore p4k_cpu      --verify-crc --populate cpu
restore p2m_none     --verify-crc --hugetlb
restore p2m_cpu      --verify-crc --populate cpu --hugetlb
restore p2m_dsa      --verify-crc --populate dsa --hugetlb
restore p4k_dsa      --verify-crc --populate dsa
# manifest CRC corruption: must be refused by the verifier
CORR=${SNAPDIR}_crccorrupt; sudo rm -rf $CORR; sudo cp -r $SNAPDIR $CORR
sudo python3 - $CORR <<'PY'
import json,sys,os
p=os.path.join(sys.argv[1],"memory-1.index.json"); m=json.load(open(p))
c=[x for x in m["chunks"] if not x.get("zero") and "crc32c" in x][3]; c["crc32c"]^=0x1; json.dump(m,open(p,"w"))
print("  corrupted manifest crc of chunk at", hex(c["uncompressed_offset"]))
PY
SNAPDIR=$CORR restore crc_corrupt --verify-crc
log "DONE -> $OUT"

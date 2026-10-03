#!/bin/bash
# verify_resident.sh [session] [pages] -- resident daemons (cpu, dsa diff):
# base + two diffs, then restore each chain and compare with a raw dump.
set -u
S=${1:-write-compressor_30b}; PAGES=${2:-4k}
B=$HOME/cloud-hypervisor/target-iaa/release; OD=$B/offload_daemon; RAW=$HOME/ch-iaa-base/target/release/offload_daemon
K=$HOME/vmbench/guest-kernel; T=$HOME/cloud-hypervisor/benchmark/agent/traj/$S.traj.json
OUT=$HOME/chlogs/verifyres/$S-$PAGES; rm -rf -- "${OUT:?}"; mkdir -p $OUT
SNAP=/mnt/chsnap/verifyres/$S-$PAGES; rm -rf -- "${SNAP:?}"; mkdir -p $SNAP
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
VARIANTS=${VARIANTS:-"cpu dsa"}; STEP_A=${STEP_A:-3}; STEP_B=${STEP_B:-6}
cp --sparse=always $HOME/vmbench/rootfs_traj_$S.ext4 $OUT/disk.ext4
STEPS=$(python3 -c "import json;t=json.load(open('$T'));print(','.join('traj-$S:%d'%x['turn'] for x in t['turns']))")
MEM="size=4G,shared=on"; [ $PAGES = 2m ] && MEM="$MEM,hugepages=on,hugepage_size=2M"
declare -A DPID
for v in $VARIANTS; do
  cmp=${v%dedup}; extra=""; [ "$cmp" != "$v" ] && extra="--dedup-image $HOME/vmbench/rootfs_traj_$S.ext4"
  $OD serve --socket $OUT/$v.sock --output-root $SNAP/$v --compression qpl-hardware-static-async --workers 8 --classify dsa \
    --reference-dir $SNAP/$v/ref --diff-compare $cmp $extra > $OUT/daemon_$v.log 2>&1 &
  DPID[$v]=$!
done
for v in $VARIANTS; do until [ -S $OUT/$v.sock ]; do sleep 0.05; done; done
$B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$OUT/disk.ext4,image_type=raw --cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=$STEPS" \
  --cpus boot=2 --memory $MEM --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CH=$!
waitm() { until grep -aq "$1" $OUT/console.log 2>/dev/null; do kill -0 $CH || { echo "CH died"; exit 1; }; sleep 0.2; done; }
ckpt() {
  $B/ch-remote --api-socket $OUT/api.sock pause
  local n=0
  local nv; nv=$(echo $VARIANTS | wc -w)
  for v in $VARIANTS; do n=$((n+1)); local dl=keep; [ $n -eq $nv ] && dl=consume
    python3 $HOME/cloud-hypervisor/benchmark/agent/ckpt_send.py $B/ch-remote $OUT/api.sock $OUT/$v.sock $dl $OUT/daemon_$v.log ${DPID[$v]} | sed -E "s/^([0-9.]+) ([0-9.]+) ([0-9]+) Checkpoint ([0-9]+) [^:]*: /  $v ckpt \4 (wall \1 ms, cpu \2 s, rc \3): /" | cut -c1-260
  done
}
waitm AGENT_BASE_POINT; ckpt; $B/ch-remote --api-socket $OUT/api.sock resume
waitm "AGENT_STEP_DONE $STEP_A "; ckpt; $B/ch-remote --api-socket $OUT/api.sock resume
waitm "AGENT_STEP_DONE $STEP_B "; ckpt
rm -f $OUT/off.sock; $RAW snapshot --socket $OUT/off.sock --output-dir $SNAP/truth > $OUT/truth.log 2>&1 & TD=$!
until [ -S $OUT/off.sock ]; do sleep 0.05; done
$B/ch-remote --api-socket $OUT/api.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $TD
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CH 2>/dev/null; wait $CH 2>/dev/null
for v in $VARIANTS; do kill ${DPID[$v]} 2>/dev/null; done
for v in $VARIANTS; do
  rm -f $OUT/r.sock $OUT/rd.sock
  $B/cloud-hypervisor --api-socket $OUT/r.sock > $OUT/ch_r_$v.log 2>&1 & R=$!
  until [ -S $OUT/r.sock ]; do sleep 0.05; done
  $B/ch-remote --api-socket $OUT/r.sock receive-migration receiver_url=unix:$OUT/rd.sock > /dev/null 2>&1 & RV=$!
  until [ -S $OUT/rd.sock ]; do sleep 0.05; done
  $OD restore --socket $OUT/rd.sock --input-dir $SNAP/$v/ckpt-000002 --workers 32 > $OUT/restore_$v.log 2>&1 || echo "restore $v failed: $(tail -3 $OUT/restore_$v.log)"
  wait $RV
  grep -aoE "Applied .*" $OUT/restore_$v.log | sed "s/^/  $v restore: /"
  rm -f $OUT/off.sock; $RAW snapshot --socket $OUT/off.sock --output-dir $SNAP/check-$v > /dev/null 2>&1 & CD=$!
  until [ -S $OUT/off.sock ]; do sleep 0.05; done
  $B/ch-remote --api-socket $OUT/r.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $CD
  $B/ch-remote --api-socket $OUT/r.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $R 2>/dev/null; wait $R 2>/dev/null
  python3 - $SNAP $v <<'PY'
import mmap, sys, numpy as np
S, v = sys.argv[1], sys.argv[2]
def pages(p):
    f = open(p, "rb"); m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    return np.frombuffer(m, dtype=np.uint8).reshape(-1, 4096)
for a, b in ((0, 2), (1, 3)):
    t = pages(f"{S}/truth/memory-{a}"); c = pages(f"{S}/check-{v}/memory-{b}")
    d = np.flatnonzero((t != c).any(axis=1))
    print(f"  {v} slot {a}: {len(d)} pages differ from ground truth {[hex(int(x) * 4096) for x in d[:4]]}")
PY
done
rm -rf -- "${SNAP:?}"

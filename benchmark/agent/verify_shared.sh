#!/bin/bash
# verify_shared.sh -- shared vs per-slot pipeline: snapshot one paused guest
# both ways (plus a raw dump), restore each, compare restored memory to it.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; OD=$B/offload_daemon; RAW=$HOME/ch-iaa-base/target/release/offload_daemon
K=$HOME/vmbench/guest-kernel; OUT=$HOME/chlogs/verifyshared; SNAP=/mnt/chsnap/verifyshared
rm -rf -- "${OUT:?}" "${SNAP:?}"; mkdir -p $OUT $SNAP
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
$B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$HOME/vmbench/rootfs_full.ext4,readonly=on,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda ro init=/agent_init agent_wl=idle agent_warm=all agent_steps=" \
  --cpus boot=2 --memory size=4G,shared=on --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CH=$!
until grep -aq AGENT_BASE_POINT $OUT/console.log 2>/dev/null; do sleep 0.2; done
$B/ch-remote --api-socket $OUT/api.sock pause
snap() { # api dir daemon args...
  local api=$1 dir=$2; shift 2; rm -f $OUT/off.sock
  "$@" --socket $OUT/off.sock --output-dir $dir > $dir.log 2>&1 & local d=$!
  until [ -S $OUT/off.sock ]; do sleep 0.05; done
  local t0=$(date +%s%N)
  $B/ch-remote --api-socket $api send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $d
  echo "  $(basename $dir): $(( ($(date +%s%N) - t0) / 1000000 )) ms, $(grep -c slots_sharing_pool=2 $dir.log) slots in one pool"
}
snap $OUT/api.sock $SNAP/truth $RAW snapshot
for v in shared-dsa shared-cpu perslot-dsa; do
  p=shared; [[ $v == perslot-* ]] && p=per-slot
  snap $OUT/api.sock $SNAP/$v $OD snapshot --compression qpl-hardware-static-async --workers 8 --classify ${v#*-} --pipeline $p
done
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CH 2>/dev/null; wait $CH 2>/dev/null
for v in shared-dsa shared-cpu perslot-dsa; do
  rm -f $OUT/r.sock $OUT/rd.sock
  $B/cloud-hypervisor --api-socket $OUT/r.sock > $OUT/ch_r_$v.log 2>&1 & R=$!
  until [ -S $OUT/r.sock ]; do sleep 0.05; done
  $B/ch-remote --api-socket $OUT/r.sock receive-migration receiver_url=unix:$OUT/rd.sock > /dev/null 2>&1 & RV=$!
  until [ -S $OUT/rd.sock ]; do sleep 0.05; done
  $OD restore --socket $OUT/rd.sock --input-dir $SNAP/$v --workers 32 > $OUT/restore_$v.log 2>&1 || echo "restore $v failed"
  wait $RV
  snap $OUT/r.sock $SNAP/check-$v $RAW snapshot > /dev/null
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
    print(f"  {v} slot {a}: {len(d)} pages differ from ground truth {[hex(int(x) * 4096) for x in d[:3]]}")
PY
done
rm -rf -- "${SNAP:?}"

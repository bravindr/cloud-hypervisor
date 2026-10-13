#!/bin/bash
# cmp_restore.sh <snapdir>: restore the checkpoint twice into PAUSED VMMs, once
# through mapping writes and once through UFFDIO_COPY with the DSA provider,
# cached destinations and registered sources (after a warm-up restore), raw-dump
# both restored memories and compare them page by page.
set -u
SNAPDIR=${1:?}; B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote
OD=$HOME/cloud-hypervisor/target-iaa2/release/offload_daemon; RAW=$HOME/ch-iaa-base/target/release/offload_daemon
OUT=$HOME/chlogs/cmp_restore_$(date +%m%d_%H%M); mkdir -p $OUT; SNAP=/mnt/chsnap/cmp_restore; rm -rf $SNAP; mkdir -p $SNAP
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
DISK=$(python3 -c "import json;print(json.load(open('$SNAPDIR/migration_config.json'))['vm_config']['disks'][0]['path'])")
TEMPLATE=$HOME/vmbench/rootfs_traj_write-compressor_30b.ext4
one() { # name resume(0/1) args...
  local name=$1 resume=$2; shift 2; local rs=""; [ $resume = 1 ] && rs="--resume"
  cp --sparse=always $TEMPLATE $DISK; rm -f $OUT/api.sock $OUT/data.sock $OUT/off.sock
  numactl -C 0-3 -m 0 $CH --api-socket $OUT/api.sock > $OUT/ch_$name.log 2>&1 & local chpid=$!
  for _ in $(seq 1 200); do [ -S $OUT/api.sock ] && break; sleep 0.05; done
  $REM --api-socket $OUT/api.sock receive-migration receiver_url=unix:$OUT/data.sock >/dev/null 2>&1 & local recv=$!
  for _ in $(seq 1 200); do [ -S $OUT/data.sock ] && break; sleep 0.05; done
  taskset -c 4,5 $OD restore --socket $OUT/data.sock --input-dir $SNAPDIR --workers 32 --hugetlb $rs "$@" > $OUT/d_$name.log 2>&1; local rc=$?; wait $recv
  echo "$name: restore rc=$rc $(grep -c registered $OUT/d_$name.log) registration lines"
  if [ $resume = 0 ]; then
    $RAW snapshot --socket $OUT/off.sock --output-dir $SNAP/$name > $OUT/raw_$name.log 2>&1 & local rd=$!
    until [ -S $OUT/off.sock ]; do sleep 0.05; done
    $REM --api-socket $OUT/api.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null 2>&1; wait $rd
    ls -la $SNAP/$name | grep memory | awk "{print \"  \", \$5, \$9}"
  fi
  $REM --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $chpid 2>/dev/null; wait $chpid 2>/dev/null
}
echo 8192 | sudo tee /sys/module/mm_offload_dma/parameters/map_cache_cap_mb >/dev/null
echo 1 | sudo tee /sys/kernel/uffd_dma/offloading /sys/kernel/uffd_dma/dev_by_pfn /sys/kernel/uffd_dma/dst_map_cache >/dev/null
one plain 0
one warmup 1 --uffd-copy
one uffd 0 --uffd-copy
cat /sys/kernel/uffd_dma/copies_done /sys/kernel/uffd_dma/copies_refused /sys/kernel/uffd_dma/copies_failed | paste - - -
echo 0 | sudo tee /sys/kernel/uffd_dma/offloading >/dev/null
python3 - $SNAP <<'PY'
import mmap, sys, numpy as np, os
S = sys.argv[1]
def pages(p):
    f = open(p, "rb"); m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    return np.frombuffer(m, dtype=np.uint8).reshape(-1, 4096)
for slot in (0, 1):
    a = pages(f"{S}/plain/memory-{slot+2}"); b = pages(f"{S}/uffd/memory-{slot+2}")
    print(f"slot {slot}: {a.shape[0]} pages, sizes equal={a.shape==b.shape}", end=" ")
    d = np.flatnonzero((a != b).any(axis=1)) if a.shape == b.shape else None
    print(f"differ={len(d) if d is not None else 'n/a'} {[hex(int(x)*4096) for x in d[:5]] if d is not None else ''}")
PY
echo CMP_DONE

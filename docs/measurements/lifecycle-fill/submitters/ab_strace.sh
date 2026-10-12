#!/bin/bash
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; OD=$B/offload_daemon
K=$HOME/vmbench/guest-kernel; OUT=$HOME/chlogs/abstrace; SNAP=/mnt/chsnap/abstrace
IMG=$HOME/chlogs/lifecycle-scratch/abs.run.ext4
rm -rf -- "${OUT:?}" "${SNAP:?}"; mkdir -p $OUT $SNAP
cp --sparse=always $HOME/vmbench/rootfs_traj_circuit-fibsqrt_7b.ext4 $IMG
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
numactl -C 0-3 -m 0 $B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$IMG,image_type=raw --cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=" \
  --cpus boot=2 --memory size=4G,shared=on --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CH=$!
until grep -aq AGENT_BASE_POINT $OUT/console.log 2>/dev/null; do sleep 0.2; done
$B/ch-remote --api-socket $OUT/api.sock pause
snap() { local name=$1; shift; local dir=$SNAP/$name; rm -rf "$dir"; rm -f $OUT/off.sock
  taskset -c 4,5 strace -f -c -w -o $OUT/$name.strace "$@" --socket $OUT/off.sock --output-dir $dir > $OUT/$name.log 2>&1 & local d=$!
  until [ -S $OUT/off.sock ]; do sleep 0.02; done
  local t0=$(date +%s%N)
  $B/ch-remote --api-socket $OUT/api.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $d
  echo "$name wall $(( ($(date +%s%N) - t0) / 1000000 )) ms"
  grep -E "pwrite64|futex|total|madvise|sched_yield|nanosleep" $OUT/$name.strace | awk '{printf "    %-12s calls %6s  total %8ss  avg %6s us\n", $NF, $4, $2, $3}'
}
C="--compression qpl-hardware-static-async --classify cpu --workers 16"
for rep in 1 2; do
  snap shared2-$rep $OD snapshot $C --pipeline shared --submitters 2
  snap perslot-$rep $OD snapshot $C --pipeline per-slot
done
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CH 2>/dev/null; wait $CH 2>/dev/null
rm -rf -- "${SNAP:?}" "$IMG"

#!/bin/bash
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; OD=$B/offload_daemon
K=$HOME/vmbench/guest-kernel; OUT=$HOME/chlogs/abworkers; SNAP=/mnt/chsnap/abworkers
IMG=$HOME/chlogs/lifecycle-scratch/abw.run.ext4
rm -rf -- "${OUT:?}" "${SNAP:?}"; mkdir -p $OUT $SNAP
cp --sparse=always $HOME/vmbench/rootfs_traj_circuit-fibsqrt_7b.ext4 $IMG
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=debug
numactl -C 0-3 -m 0 $B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$IMG,image_type=raw --cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=" \
  --cpus boot=2 --memory size=4G,shared=on --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CH=$!
until grep -aq AGENT_BASE_POINT $OUT/console.log 2>/dev/null; do sleep 0.2; done
$B/ch-remote --api-socket $OUT/api.sock pause
snap() { local name=$1; shift; local dir=$SNAP/$name; rm -rf "$dir"; rm -f $OUT/off.sock
  taskset -c 4,5 "$@" --socket $OUT/off.sock --output-dir $dir > $OUT/$name.log 2>&1 & local d=$!
  until [ -S $OUT/off.sock ]; do sleep 0.02; done
  local t0=$(date +%s%N)
  $B/ch-remote --api-socket $OUT/api.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $d
  echo "$name $(( ($(date +%s%N) - t0) / 1000000 ))" >> $OUT/walls.txt
}
C="--compression qpl-hardware-static-async --classify cpu"
for rep in 1 2 3 4; do
  snap sh2-w8-$rep  $OD snapshot $C --workers 8  --pipeline shared --submitters 2
  snap sh2-w16-$rep $OD snapshot $C --workers 16 --pipeline shared --submitters 2
  snap sh2-w32-$rep $OD snapshot $C --workers 32 --pipeline shared --submitters 2
  snap sh1-w16-$rep $OD snapshot $C --workers 16 --pipeline shared --submitters 1
  snap ps-w8-$rep   $OD snapshot $C --workers 8  --pipeline per-slot
  snap ps-w16-$rep  $OD snapshot $C --workers 16 --pipeline per-slot
  snap sh4-w16-$rep $OD snapshot $C --workers 16 --pipeline shared --submitters 4
done
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CH 2>/dev/null; wait $CH 2>/dev/null
echo "== wall ms sorted | compress_ms range | chunks/ms of the longest thread"
for l in sh2-w8 sh2-w16 sh2-w32 sh1-w16 ps-w8 ps-w16 sh4-w16; do
  echo -n "$l: "; grep "^$l-" $OUT/walls.txt | awk '{print $2}' | sort -n | tr "\n" " "
  echo -n "| compress "; grep -h "classify=" $OUT/$l-*.log | grep -o "compress_ms=[0-9.]*" | cut -d= -f2 | sort -n | awk '{a[NR]=$1} END{printf "%s..%s", a[1], a[NR]}'
  echo -n " | threads rep2: "; grep -h "submitter:" $OUT/$l-2.log | sed 's/.*compressed \([0-9]*\) chunks, finished at \([0-9.]*\) ms.*/\1@\2ms/' | tr "\n" " "; echo
done
rm -rf -- "${SNAP:?}" "$IMG"

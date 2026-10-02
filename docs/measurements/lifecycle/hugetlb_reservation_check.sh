#!/bin/bash
# Does a daemon checkpoint allocate the guest's untouched shmem pages?
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; OUT=$HOME/chlogs/rsscheck; mkdir -p $OUT; rm -f $OUT/*.sock
K=$HOME/vmbench/guest-kernel
$B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$HOME/vmbench/rootfs_traj_write-compressor_30b.ext4,readonly=on,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda ro init=/agent_init agent_wl=idle agent_warm= agent_steps=" \
  --cpus boot=2 --memory size=4G,shared=on,hugepages=on,hugepage_size=2M --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CHPID=$!
until grep -aq AGENT_BASE_POINT $OUT/console.log 2>/dev/null; do sleep 0.2; done
$B/ch-remote --api-socket $OUT/api.sock pause
shm() { echo "Rsvd=$(awk "/HugePages_Rsvd/{print \$2}" /proc/meminfo) Free=$(awk "/HugePages_Free/{print \$2}" /proc/meminfo) Total=$(awk "/HugePages_Total/{print \$2}" /proc/meminfo); in use: $(( $(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages) - $(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/free_hugepages) )) ($(( ($(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages) - $(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/free_hugepages)) * 2 )) MiB)"; }
echo "guest shmem resident at base point: $(shm)"
for v in base cpu dsa; do
  rm -rf $OUT/snap; rm -f $OUT/off.sock
  case $v in base) bin=$HOME/ch-iaa-base/target/release/offload_daemon; a="";; cpu) bin=$B/offload_daemon; a="--classify cpu";; dsa) bin=$B/offload_daemon; a="--classify dsa --crc";; esac
  $bin snapshot --socket $OUT/off.sock --output-dir $OUT/snap --compression qpl-hardware-static-async --workers 8 $a > $OUT/d_$v.log 2>&1 &
  D=$!; until [ -S $OUT/off.sock ]; do sleep 0.05; done
  $B/ch-remote --api-socket $OUT/api.sock send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $D
  echo "after $v checkpoint: $(shm)  $(grep -ohE "prefault=[0-9.]+ms|hole_chunks=[0-9]+|data_mib=[0-9]+" $OUT/d_$v.log | tr '\n' ' ')"
done
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CHPID 2>/dev/null

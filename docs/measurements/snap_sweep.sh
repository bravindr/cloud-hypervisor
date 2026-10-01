#!/bin/bash
# snap_sweep.sh -- one warm guest, DSA classify, sweep the IAA job budget.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote; OD=$B/offload_daemon
KERN=/boot/vmlinuz-6.8.0-142-generic; INITRD=/boot/initrd.img-6.8.0-142-generic; ROOTFS=$HOME/vmbench/rootfs_full.ext4
OUT=$HOME/chlogs/snapsweep_$(date +%Y%m%d_%H%M); mkdir -p $OUT
SOCK=/tmp/chsw.sock; OSOCK=/tmp/chsw_offload.sock; SNAP=/mnt/chsnap/sweepsnap; sudo mkdir -p $SNAP
DTOENV="DTO_WQ_LIST=wq0.0;wq2.0;wq4.0;wq6.0 DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info"
log() { echo "$(date +%H:%M:%S) $*" | tee -a $OUT/summary.txt; }
sudo rm -f $SOCK $OSOCK
sudo numactl -N0 -m0 $CH --api-socket $SOCK -v --kernel $KERN --initramfs $INITRD --disk path=$ROOTFS,readonly=on,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda ro init=/agent_init agent_wl=idle agent_warm=${WARM-all} agent_steps=" \
  --cpus boot=2 --memory size=4G,hugepages=on,hugepage_size=2M,shared=on --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CHPID=$!
for i in $(seq 1 1500); do sudo grep -q AGENT_BASE_POINT $OUT/console.log 2>/dev/null && break; kill -0 $CHPID 2>/dev/null || { log "CH exited"; exit 1; }; sleep 0.2; done
sudo $REM --api-socket $SOCK pause && log "guest paused"
CELLS=${CELLS:-dsa:8 dsa:16 dsa:32 dsa:64 dsa:128 cpu:32 dsa:32 dsa:64}
for cell in $CELLS; do
  cls=${cell%%:*}; w=${cell##*:}; name=${cls}_w$w
  sudo rm -rf $SNAP/$name; sudo rm -f $OSOCK
  sudo env $DTOENV /usr/bin/time -f "wall=%e user=%U sys=%S" -o $OUT/time_$name.txt numactl -N0 -m0 \
    $OD snapshot --socket $OSOCK --output-dir $SNAP/$name --compression qpl-hardware-static-async --chunk-size 1048576 --workers $w --classify $cls ${EXTRA:-} > $OUT/daemon_$name.log 2>&1 &
  DPID=$!; for i in $(seq 1 150); do [ -S $OSOCK ] && break; sleep 0.1; done
  t0=$(date +%s%N); sudo $REM --api-socket $SOCK send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on" > /dev/null 2>&1; wait $DPID; rc=$?; t1=$(date +%s%N)
  log "$name: wall $(( (t1-t0)/1000000 )) ms rc=$rc $(cat $OUT/time_$name.txt) | $(grep -ohE "slot 1: .*throughput=[0-9.]+" $OUT/daemon_$name.log | grep -oE "throughput=[0-9.]+") slot1 $(grep -ohE "slot 0: .*throughput=[0-9.]+" $OUT/daemon_$name.log | grep -oE "throughput=[0-9.]+") slot0"
done
sudo $REM --api-socket $SOCK shutdown-vmm >/dev/null 2>&1; sleep 1; sudo kill $CHPID 2>/dev/null
log "DONE -> $OUT"

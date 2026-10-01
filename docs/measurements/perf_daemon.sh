#!/bin/bash
# perf_daemon.sh -- boot a 4 GiB shared-memory guest on the iaa-integration
# build, pause it at AGENT_BASE_POINT, then snapshot it through offload_daemon
# once per codec cell: first under /usr/bin/time (wall/user/sys), then under
# perf record (where the daemon's CPU goes). preserve_source=on keeps the VM
# paused between cells so every cell sees the same memory image.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote; OD=${OD:-$B/offload_daemon}
PERF=/usr/lib/linux-tools-6.8.0-142/perf
KERN=/boot/vmlinuz-6.8.0-142-generic; INITRD=/boot/initrd.img-6.8.0-142-generic
ROOTFS=$HOME/vmbench/rootfs_full.ext4
WARM=${WARM-all}                       # all = warm synthetic (~77% zero); "" = idle boot (~90% zero)
CELLS=${CELLS:-"qpl-hardware-static-async:8 lz4:8 raw:0"}
OUT=$HOME/chlogs/perfdaemon_${WARM:-idle}_$(date +%Y%m%d_%H%M); mkdir -p $OUT
SOCK=/tmp/chiaa.sock; OSOCK=/tmp/chiaa_offload.sock
SNAPROOT=/mnt/nvme/scratch/perfsnap; sudo mkdir -p $SNAPROOT
log() { echo "$(date +%H:%M:%S) $*" | tee -a $OUT/summary.txt; }

sudo rm -f $SOCK $OSOCK
sudo numactl -N0 -m0 $CH --api-socket $SOCK -v --kernel $KERN --initramfs $INITRD \
  --disk path=$ROOTFS,readonly=on,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda ro init=/agent_init agent_wl=idle agent_warm=$WARM agent_steps=" \
  --cpus boot=2 --memory size=4G,hugepages=on,hugepage_size=2M,shared=on \
  --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CHPID=$!
for i in $(seq 1 1500); do
  sudo grep -q AGENT_BASE_POINT $OUT/console.log 2>/dev/null && break
  kill -0 $CHPID 2>/dev/null || { log "CH exited before base point"; tail -20 $OUT/ch.log; exit 1; }
  sleep 0.2
done
sudo $REM --api-socket $SOCK pause && log "guest paused at AGENT_BASE_POINT (warm=${WARM:-none})"
sudo grep -E "MemTotal|MemFree|AnonPages|^Cached|Shmem" $OUT/console.log | tail -5 | tee -a $OUT/summary.txt

for cell in $CELLS; do
  codec=${cell%%:*}; workers=${cell##*:}
  args=(snapshot --socket $OSOCK --output-dir $SNAPROOT/$codec)
  [ $codec != raw ] && args+=(--compression $codec --chunk-size 1048576 --workers $workers)
  for prof in time perf; do
    sudo rm -rf $SNAPROOT/$codec; sudo rm -f $OSOCK
    if [ $prof = time ]; then
      sudo RUST_LOG=info /usr/bin/time -f "wall=%e user=%U sys=%S maxrss=%MkB" -o $OUT/time_$codec.txt \
        numactl -N0 -m0 $OD "${args[@]}" > $OUT/daemon_${codec}_$prof.log 2>&1 &
    else
      sudo RUST_LOG=info numactl -N0 -m0 $PERF record --call-graph dwarf,16384 -F 499 -o $OUT/perf_$codec.data \
        -- $OD "${args[@]}" > $OUT/daemon_${codec}_$prof.log 2>&1 &
    fi
    DPID=$!
    for i in $(seq 1 150); do [ -S $OSOCK ] && break; sleep 0.1; done
    t0=$(date +%s%N)
    sudo $REM --api-socket $SOCK send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on" \
      > $OUT/remote_${codec}_$prof.log 2>&1 || log "  send-migration rc=$? ($codec/$prof)"
    wait $DPID; rc=$?
    t1=$(date +%s%N)
    ms=$(( (t1 - t0) / 1000000 ))
    if [ $prof = time ]; then
      log "$codec w=$workers: migration wall ${ms} ms, daemon rc=$rc, $(cat $OUT/time_$codec.txt), out=$(sudo du -sm $SNAPROOT/$codec | cut -f1) MiB"
      grep -E "Compressed slot|ratio|throughput" $OUT/daemon_${codec}_$prof.log | cut -c1-200 | tee -a $OUT/summary.txt
    else
      log "$codec w=$workers: perf cell wall ${ms} ms, rc=$rc"
      sudo chown $USER $OUT/perf_$codec.data
      $PERF report -i $OUT/perf_$codec.data --no-children --percent-limit 0.8 --stdio -g none 2>/dev/null \
        | grep -vE "^#|^$" | head -40 > $OUT/report_${codec}_self.txt
      $PERF report -i $OUT/perf_$codec.data --no-children --sort dso --stdio -g none 2>/dev/null \
        | grep -vE "^#|^$" | head -12 > $OUT/report_${codec}_dso.txt
      $PERF report -i $OUT/perf_$codec.data --children --percent-limit 2 --stdio -g none 2>/dev/null \
        | grep -vE "^#|^$" | head -40 > $OUT/report_${codec}_children.txt
    fi
  done
done
sudo $REM --api-socket $SOCK shutdown-vmm 2>/dev/null; sleep 1; sudo kill $CHPID 2>/dev/null
log "DONE -> $OUT"

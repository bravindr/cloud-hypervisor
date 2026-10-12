#!/bin/bash
# ab_layout2.sh -- lifecycle guest image, rotating layout order per repetition.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; OD=$B/offload_daemon
K=$HOME/vmbench/guest-kernel; OUT=$HOME/chlogs/ablayout2; SNAP=/mnt/chsnap/ablayout2
IMG=$HOME/chlogs/lifecycle-scratch/ab.run.ext4
rm -rf -- "${OUT:?}" "${SNAP:?}"; mkdir -p $OUT $SNAP
cp --sparse=always $HOME/vmbench/rootfs_traj_circuit-fibsqrt_7b.ext4 $IMG
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=debug
numactl -C 0-3 -m 0 $B/cloud-hypervisor --api-socket $OUT/api.sock --kernel $K/vmlinuz-6.8.0-142-generic --initramfs $K/initrd.img-6.8.0-142-generic \
  --disk path=$IMG,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=" \
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
run() { # layout cls rep
  case $1 in shared2) a="--pipeline shared --submitters 2";; shared1) a="--pipeline shared --submitters 1";; perslot) a="--pipeline per-slot";; esac
  snap $1-$2-$3 $OD snapshot --compression qpl-hardware-static-async --workers 8 --classify $2 $a
}
L=(shared2 shared1 perslot)
for rep in 1 2 3 4 5 6; do for cls in cpu dsa; do
  for k in 0 1 2; do run ${L[$(( (rep + k) % 3 ))]} $cls $rep; done
done; done
$B/ch-remote --api-socket $OUT/api.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $CH 2>/dev/null; wait $CH 2>/dev/null
echo "== wall ms, sorted (6 reps, order rotated)"
for cls in cpu dsa; do for l in shared2 shared1 perslot; do echo -n "$l-$cls: "; grep "^$l-$cls-" $OUT/walls.txt | awk '{print $2}' | sort -n | tr "\n" " "; echo; done; done
echo "== daemon phases (median over reps): prefault per slot, compress_ms"
for cls in cpu dsa; do for l in shared2 shared1 perslot; do
  echo -n "$l-$cls: prefault "; grep -h "classify=" $OUT/$l-$cls-*.log | grep -o "prefault=[0-9.]*" | cut -d= -f2 | sort -n | awk '{a[NR]=$1} END{printf "%s..%s ms  ", a[1], a[NR]}'
  echo -n "compress_ms "; grep -h "classify=" $OUT/$l-$cls-*.log | grep -o "compress_ms=[0-9.]*" | cut -d= -f2 | sort -n | awk '{a[NR]=$1} END{printf "%s..%s\n", a[1], a[NR]}'
done; done
rm -rf -- "${SNAP:?}" "$IMG"

#!/bin/bash
# lifecycle_diff.sh <session> [4k|2m] -- real-agent lifecycle with dirty-log
# diff checkpoints. At the base point every variant takes a full checkpoint
# (diff variants also initialise their reference). After every agent turn the
# VM is paused and each variant checkpoints the same image (rotating order):
#
#   full-dsa   full checkpoint, DSA zero classify (no dirty log)
#   diff-none  dirty log only: every dirty page is stored
#   diff-cpu   dirty pages compared against the reference with memcmp, copied with memcpy
#   diff-dsa   same with batched DSA COMPARE + DUALCAST (DIFF_BATCH pages per batch)
#   diff-dsa1  DSA, one descriptor per page (no batching), DIFF_DEPTH1 in flight
#
# All diff variants receive the same dirty set: every one sends
# dirty_log=keep except the last diff variant of the turn, which consumes it.
# After the last turn each variant's final checkpoint (a diff chain for the
# diff variants) is restored and the guest must reach AGENT_DIFF_POINT.
set -u
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)
SESSION=${1:?session}; PAGES=${2:-4k}
BIN_DIR=${BIN_DIR:-$REPO/target-iaa/release}
CH=$BIN_DIR/cloud-hypervisor; REM=$BIN_DIR/ch-remote; OD=${OFFLOAD_BIN:-$BIN_DIR/offload_daemon}
KERNEL=${KERNEL:-$HOME/vmbench/guest-kernel/vmlinuz-6.8.0-142-generic}; INITRD=${INITRD:-$HOME/vmbench/guest-kernel/initrd.img-6.8.0-142-generic}
ROOTFS_TEMPLATE=${ROOTFS_DIR:-$HOME/vmbench}/rootfs_traj_$SESSION.ext4
TRAJ=$HERE/traj/$SESSION.traj.json
read -r -a VARIANTS <<<"${VARIANTS:-full-dsa diff-none diff-cpu diff-dsa diff-dsa1}"
CODEC=${CODEC:-qpl-hardware-static-async}; WORKERS=${WORKERS:-8}; RESTORE_WORKERS=${RESTORE_WORKERS:-32}
DIFF_BATCH=${DIFF_BATCH:-256}; DIFF_DEPTH=${DIFF_DEPTH:-8}; DIFF_DEPTH1=${DIFF_DEPTH1:-32}
VM_CPUS=${VM_CPUS:-0-3}; DAEMON_CPUS=${DAEMON_CPUS:-4,5}
export DTO_WQ_LIST=${DTO_WQ_LIST:-"wq0.0;wq2.0;wq4.0;wq6.0"} DTO_IS_NUMA_AWARE=${DTO_IS_NUMA_AWARE:-1} DTO_LOG_LEVEL=${DTO_LOG_LEVEL:-1}
SNAP=${SNAP_ROOT:-/mnt/chsnap/lifecycle-diff}/$SESSION-$PAGES
SCRATCH=${SCRATCH:-$HOME/chlogs/lifecycle-scratch}
OUT=${OUT_ROOT:-$HOME/chlogs/lifecycle-diff}/$SESSION-$PAGES-$(date +%Y%m%d_%H%M%S)
[ -f "$ROOTFS_TEMPLATE" ] && [ -f "$TRAJ" ] || { echo "missing rootfs or trajectory for $SESSION"; exit 1; }
mkdir -p "$OUT" "$SCRATCH" "$SNAP"
for v in "${VARIANTS[@]}"; do rm -rf -- "${SNAP:?}/${v:?}"; mkdir -p "$SNAP/$v"; done
SOCK=$OUT/api.sock; OSOCK=$OUT/offload.sock; RSOCK=$OUT/restore-api.sock; RDATA=$OUT/restore-data.sock
RUNIMG=$SCRATCH/$SESSION-$PAGES.diffrun.ext4; FINALDISK=$SCRATCH/$SESSION-$PAGES.difffinal.ext4
CONSOLE=$OUT/console.log
log() { echo "$(date +%H:%M:%S) $*" | tee -a "$OUT/summary.txt"; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
STEPS=$(python3 - "$TRAJ" "$SESSION" <<'PY'
import json, sys
t = json.load(open(sys.argv[1])); s = sys.argv[2]; ts = [x["turn"] for x in t["turns"]]
full = ",".join(f"traj-{s}:{n}" for n in ts)
print(full if len(full) < 1700 else f"traj-{s}:{min(ts)}-{max(ts)}")
PY
)
NSTEPS=$(python3 -c "import json;print(len(json.load(open('$TRAJ'))['turns']))")
MEM="size=4G,shared=on"; [ "$PAGES" = 2m ] && MEM="$MEM,hugepages=on,hugepage_size=2M"
echo "session,pages,checkpoint,turn,variant,order,wall_ms,user_s,sys_s,out_bytes,dirty_pages,changed_pages,prepare_ms,compare_ms,gather_ms,compress_ms,dsa_ops,dsa_redo,rc" > "$OUT/checkpoints.csv"
echo "session,pages,variant,chain,restore_ms,user_s,sys_s,guest_ok,rc" > "$OUT/restores.csv"

variant_args() { # variant checkpoint-name parent-name
	local v=$1 name=$2 parent=$3 a=""
	case $v in
		full-dsa) echo "--classify dsa"; return ;;
		diff-none) a="--classify cpu --diff-compare none" ;;
		diff-cpu) a="--classify cpu --diff-compare cpu" ;;
		diff-dsa) a="--classify dsa --diff-compare dsa --diff-batch $DIFF_BATCH --diff-depth $DIFF_DEPTH" ;;
		diff-dsa1) a="--classify dsa --diff-compare dsa --diff-batch 1 --diff-depth $DIFF_DEPTH1" ;;
	esac
	echo "$a --reference-dir $SNAP/$v/ref${parent:+ --parent $SNAP/$v/$parent}"
}

wait_marker() {
	local deadline=$(( $(date +%s) + $2 ))
	until grep -aq "$1" "$CONSOLE" 2>/dev/null; do
		kill -0 "$CHPID" 2>/dev/null || { log "CH exited while waiting for '$1'"; return 1; }
		[ "$(date +%s)" -ge "$deadline" ] && { log "timeout waiting for '$1'"; return 1; }
		sleep 0.2
	done
}

checkpoint() { # name parent turn final
	local name=$1 parent=$2 turn=$3 final=$4 n=${#VARIANTS[@]} j v dir dlog dl last_diff="" t0 t1 rc tm out
	"$REM" --api-socket "$SOCK" pause >/dev/null || return 1
	for ((j = 0; j < n; j++)); do v=${VARIANTS[$(( (j + turn) % n ))]}; [[ $v == diff-* ]] && last_diff=$v; done
	for ((j = 0; j < n; j++)); do
		v=${VARIANTS[$(( (j + turn) % n ))]}
		dir=$SNAP/$v/$name; [ "$v" = full-dsa ] && [ "$name" != base ] && dir=$SNAP/$v/last
		rm -rf -- "${dir:?}"; rm -f -- "${OSOCK:?}" "${OSOCK:?}.lock"
		dlog=$OUT/daemon_${name}_$v.log
		dl=""; [[ $v == diff-* ]] && { dl=keep; [ "$v" = "$last_diff" ] && dl=consume; }
		/usr/bin/time -f "%e %U %S" -o "$OUT/t.txt" taskset -c "$DAEMON_CPUS" "$OD" snapshot --socket "$OSOCK" --output-dir "$dir" \
			--compression "$CODEC" --workers "$WORKERS" $(variant_args "$v" "$name" "$parent") > "$dlog" 2>&1 &
		local dpid=$!
		for _ in $(seq 1 200); do [ -S "$OSOCK" ] && break; sleep 0.05; done
		t0=$(date +%s%N)
		"$REM" --api-socket "$SOCK" send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on${dl:+,dirty_log=$dl}" > "$OUT/remote.log" 2>&1
		wait "$dpid"; rc=$?; t1=$(date +%s%N); tm=$(tail -1 "$OUT/t.txt")
		out=$(find "$dir" -maxdepth 1 \( -name "*.compressed" -o -name "*.diff" \) -printf "%s\n" 2>/dev/null | awk '{s+=$1} END{print s+0}')
		stats=$(grep -aoE "Diff slot [0-9]+: .*" "$dlog" | python3 -c "
import sys, re
k = ['dirty_pages','changed_pages','prepare_ms','compare_ms','gather_ms','compress_ms','dsa_ops','dsa_cpu_redo']
t = dict.fromkeys(k, 0.0); n = 0
for line in sys.stdin:
    n += 1
    for key in k:
        m = re.search(key + r'=([0-9.]+)', line)
        if m: t[key] += float(m.group(1))
print(','.join(('%g' % t[x]) if n else '' for x in k))")
		echo "$SESSION,$PAGES,$name,$turn,$v,$j,$(( (t1 - t0) / 1000000 )),$(echo "$tm" | cut -d' ' -f2),$(echo "$tm" | cut -d' ' -f3),$out,$stats,$rc" >> "$OUT/checkpoints.csv"
		[ $rc -ne 0 ] && { log "checkpoint $name $v failed rc=$rc"; tail -4 "$dlog" | tee -a "$OUT/summary.txt"; }
		if [ "$name" != base ] && [ "$final" = 0 ]; then rm -f -- "${dlog:?}"; fi
	done
	[ "$final" = 1 ] || "$REM" --api-socket "$SOCK" resume >/dev/null
}

restore() { # variant dir
	local v=$1 dir=$2 args="" t0 t1 rc tm gok=0 recv chain
	cp --sparse=always "$FINALDISK" "$RUNIMG"
	[ -f "$CONSOLE" ] && mv -f "$CONSOLE" "$OUT/console.before-restore-$v.log"
	rm -f -- "${RSOCK:?}" "${RDATA:?}"
	numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$RSOCK" > "$OUT/ch_restore_$v.log" 2>&1 &
	CHPID=$!
	for _ in $(seq 1 200); do [ -S "$RSOCK" ] && break; sleep 0.05; done
	"$REM" --api-socket "$RSOCK" receive-migration "receiver_url=unix:$RDATA" > /dev/null 2>&1 & recv=$!
	for _ in $(seq 1 200); do [ -S "$RDATA" ] && break; sleep 0.05; done
	[ "$PAGES" = 2m ] && args="--hugetlb"
	t0=$(date +%s%N)
	/usr/bin/time -f "%e %U %S" -o "$OUT/t.txt" taskset -c "$DAEMON_CPUS" "$OD" restore --socket "$RDATA" --input-dir "$dir" \
		--resume --workers "$RESTORE_WORKERS" $args > "$OUT/rdaemon_$v.log" 2>&1
	rc=$?; t1=$(date +%s%N); wait "$recv" 2>/dev/null; tm=$(tail -1 "$OUT/t.txt")
	chain=$(grep -aoE "Applied [0-9]+ diffs" "$OUT/rdaemon_$v.log" | head -1 | grep -oE "[0-9]+"); chain=${chain:-0}
	[ $rc -eq 0 ] && wait_marker AGENT_DIFF_POINT 60 && gok=1
	echo "$SESSION,$PAGES,$v,$chain,$(( (t1 - t0) / 1000000 )),$(echo "$tm" | cut -d' ' -f2),$(echo "$tm" | cut -d' ' -f3),$gok,$rc" >> "$OUT/restores.csv"
	log "restore $v (chain of $chain diffs): rc=$rc $(( (t1 - t0) / 1000000 )) ms, guest ok=$gok"
	"$REM" --api-socket "$RSOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
}

cp --sparse=always "$ROOTFS_TEMPLATE" "$RUNIMG"; rm -f -- "${SOCK:?}"
numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$SOCK" --kernel "$KERNEL" --initramfs "$INITRD" --disk "path=$RUNIMG,image_type=raw" \
	--cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=$STEPS" \
	--cpus boot=2 --memory "$MEM" --serial "file=$CONSOLE" --console off > "$OUT/ch.log" 2>&1 &
CHPID=$!
log "diff lifecycle $SESSION pages=$PAGES turns=$NSTEPS variants=${VARIANTS[*]} batch=$DIFF_BATCH depth=$DIFF_DEPTH"
wait_marker AGENT_BASE_POINT 300 || exit 1
checkpoint base "" 0 0
prev=base
for ((k = 1; k <= NSTEPS; k++)); do
	wait_marker "AGENT_STEP_DONE $k " 900 || exit 1
	checkpoint "step$k" "$prev" "$k" "$([ $k -eq "$NSTEPS" ] && echo 1 || echo 0)"
	prev=step$k
	[ $((k % 10)) -eq 0 ] && log "  turn $k/$NSTEPS"
done
"$REM" --api-socket "$SOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
cp --sparse=always "$RUNIMG" "$FINALDISK"
for v in "${VARIANTS[@]}"; do
	if [ "$v" = full-dsa ]; then restore "$v" "$SNAP/$v/last"; else restore "$v" "$SNAP/$v/step$NSTEPS"; fi
done
du -sm "$SNAP"/*/ | sed "s|$SNAP/||" | tee -a "$OUT/summary.txt"
rm -f -- "${RUNIMG:?}" "${FINALDISK:?}"
rm -rf -- "${SNAP:?}"
log "DONE -> $OUT"

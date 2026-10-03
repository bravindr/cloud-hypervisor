#!/bin/bash
# lifecycle_resident.sh <session> [4k|2m] -- per-process vs resident offload
# daemons on a real-agent lifecycle. Every checkpoint is taken by every
# variant from the same paused image (rotating order), all diff variants fed
# the same dirty set:
#
#   full-proc      one daemon process per checkpoint, full, DSA classify
#   full-res       resident daemon, full, DSA classify (keeps only the last)
#   diff-cpu-proc  one process per checkpoint, dirty-log diff, CPU compare/copy
#   diff-dsa-proc  one process per checkpoint, diff, batched DSA
#   diff-cpu-res   resident, diff, CPU
#   diff-dsa-res   resident, diff, batched DSA
#
#   *-res-dedup    resident diff + disk-block dedup against the task image
#
# Wall = send-migration duration (the VM stays paused for it). CPU = daemon
# CPU for that checkpoint: rusage of the one-shot process (us resolution), or
# the delta of every resident thread's run time from schedstat (ns).
# After the last turn each variant's final checkpoint (chain) is restored and
# the guest must reach AGENT_DIFF_POINT.
set -u
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)
SESSION=${1:?session}; PAGES=${2:-4k}
BIN_DIR=${BIN_DIR:-$REPO/target-iaa/release}
CH=$BIN_DIR/cloud-hypervisor; REM=$BIN_DIR/ch-remote; OD=${OFFLOAD_BIN:-$BIN_DIR/offload_daemon}
KERNEL=${KERNEL:-$HOME/vmbench/guest-kernel/vmlinuz-6.8.0-142-generic}; INITRD=${INITRD:-$HOME/vmbench/guest-kernel/initrd.img-6.8.0-142-generic}
ROOTFS_TEMPLATE=${ROOTFS_DIR:-$HOME/vmbench}/rootfs_traj_$SESSION.ext4
TRAJ=$HERE/traj/$SESSION.traj.json
read -r -a VARIANTS <<<"${VARIANTS:-full-proc full-res diff-cpu-proc diff-dsa-proc diff-cpu-res diff-dsa-res}"
CODEC=${CODEC:-qpl-hardware-static-async}; WORKERS=${WORKERS:-8}; RESTORE_WORKERS=${RESTORE_WORKERS:-32}
DIFF_BATCH=${DIFF_BATCH:-256}; DIFF_DEPTH=${DIFF_DEPTH:-8}
VM_CPUS=${VM_CPUS:-0-3}; DAEMON_CPUS=${DAEMON_CPUS:-4,5}
export DTO_WQ_LIST=${DTO_WQ_LIST:-"wq0.0;wq2.0;wq4.0;wq6.0"} DTO_IS_NUMA_AWARE=${DTO_IS_NUMA_AWARE:-1} DTO_LOG_LEVEL=${DTO_LOG_LEVEL:-1} RUST_LOG=${RUST_LOG:-info}
SNAP=${SNAP_ROOT:-/mnt/chsnap/lifecycle-res}/$SESSION-$PAGES
SCRATCH=${SCRATCH:-$HOME/chlogs/lifecycle-scratch}
OUT=${OUT_ROOT:-$HOME/chlogs/lifecycle-res}/$SESSION-$PAGES-$(date +%Y%m%d_%H%M%S)
[ -f "$ROOTFS_TEMPLATE" ] && [ -f "$TRAJ" ] || { echo "missing rootfs or trajectory for $SESSION"; exit 1; }
mkdir -p "$OUT" "$SCRATCH"; rm -rf -- "${SNAP:?}"; mkdir -p "$SNAP"
SOCK=$OUT/api.sock; OSOCK=$OUT/offload.sock; RSOCK=$OUT/restore-api.sock; RDATA=$OUT/restore-data.sock
RUNIMG=$SCRATCH/$SESSION-$PAGES.resrun.ext4; FINALDISK=$SCRATCH/$SESSION-$PAGES.resfinal.ext4
CONSOLE=$OUT/console.log
log() { echo "$(date +%H:%M:%S) $*" | tee -a "$OUT/summary.txt"; }
STEPS=$(python3 - "$TRAJ" "$SESSION" <<'PY'
import json, sys
t = json.load(open(sys.argv[1])); s = sys.argv[2]; ts = [x["turn"] for x in t["turns"]]
full = ",".join(f"traj-{s}:{n}" for n in ts)
print(full if len(full) < 1700 else f"traj-{s}:{min(ts)}-{max(ts)}")
PY
)
NSTEPS=$(python3 -c "import json;print(len(json.load(open('$TRAJ'))['turns']))")
MEM="size=4G,shared=on"; [ "$PAGES" = 2m ] && MEM="$MEM,hugepages=on,hugepage_size=2M"
echo "session,pages,checkpoint,turn,variant,order,wall_ms,cpu_s,out_bytes,dirty_pages,changed_pages,prepare_ms,compare_ms,gather_ms,compress_ms,populate_calls,dsa_ops,dsa_redo,dedup_pages,dedup_ms,rc" > "$OUT/checkpoints.csv"
echo "session,pages,variant,chain,restore_ms,guest_ok,rc" > "$OUT/restores.csv"

is_res() { [[ $1 == *-res || $1 == *-res-dedup ]]; }
is_diff() { [[ $1 == diff-* ]]; }
cmp_of() { case $1 in diff-cpu-*) echo cpu ;; diff-dsa-*) echo dsa ;; esac; }
common_args() { # variant
	local v=$1
	if is_diff "$v"; then
		local dd=""; [[ $v == *-dedup ]] && dd=" --dedup-image $ROOTFS_TEMPLATE"
		echo "--classify $(cmp_of "$v") --reference-dir $SNAP/$v/ref --diff-compare $(cmp_of "$v") --diff-batch $DIFF_BATCH --diff-depth $DIFF_DEPTH$dd"
	else
		echo "--classify dsa"
	fi
}
declare -A RPID
for v in "${VARIANTS[@]}"; do
	mkdir -p "$SNAP/$v"
	if is_res "$v"; then
		extra=""; is_diff "$v" || extra="--keep-only-last"
		taskset -c "$DAEMON_CPUS" "$OD" serve --socket "$OUT/$v.sock" --output-root "$SNAP/$v" --compression "$CODEC" \
			--workers "$WORKERS" $(common_args "$v") $extra > "$OUT/daemon_$v.log" 2>&1 &
		RPID[$v]=$!
	fi
done
for v in "${VARIANTS[@]}"; do is_res "$v" && { until [ -S "$OUT/$v.sock" ]; do sleep 0.05; done; }; done

thread_ns() { # pid -> total run time of all threads, ns
	cat /proc/"$1"/task/*/schedstat 2>/dev/null | awk '{s+=$1} END{printf "%d", s}'
}
wait_marker() {
	local deadline=$(( $(date +%s) + $2 ))
	until grep -aq "$1" "$CONSOLE" 2>/dev/null; do
		kill -0 "$CHPID" 2>/dev/null || { log "CH exited while waiting for '$1'"; return 1; }
		[ "$(date +%s)" -ge "$deadline" ] && { log "timeout waiting for '$1'"; return 1; }
		sleep 0.2
	done
}
parse_stats() { # log text on stdin -> dirty,changed,prepare,compare,gather,compress,populate,dsa_ops,redo
	python3 -c "
import sys, re
k = ['dirty_pages','changed_pages','prepare_ms','compare_ms','gather_ms','compress_ms','populate_calls','dsa_ops','dsa_cpu_redo','dedup_pages','dedup_ms']
t = dict.fromkeys(k, 0.0); seen = set()
for line in sys.stdin:
    for key in k:
        m = re.search(key + r'=([0-9.]+)', line)
        if m: t[key] += float(m.group(1)); seen.add(key)
print(','.join(('%g' % t[x]) if x in seen else '' for x in k))"
}

checkpoint() { # name parent turn final
	local name=$1 parent=$2 turn=$3 final=$4 n=${#VARIANTS[@]} j v dl last_diff="" t0 t1 rc cpu out dir stats
	"$REM" --api-socket "$SOCK" pause >/dev/null || return 1
	for ((j = 0; j < n; j++)); do v=${VARIANTS[$(( (j + turn) % n ))]}; is_diff "$v" && last_diff=$v; done
	for ((j = 0; j < n; j++)); do
		v=${VARIANTS[$(( (j + turn) % n ))]}
		dl=""; is_diff "$v" && { dl=keep; [ "$v" = "$last_diff" ] && dl=consume; }
		if is_res "$v"; then
			local res wall
			res=$(python3 "$HERE/ckpt_send.py" "$REM" "$SOCK" "$OUT/$v.sock" "${dl:--}" "$OUT/daemon_$v.log" "${RPID[$v]}")
			read -r wall cpu rc _ <<<"$res"
			stats=$(echo "$res" | parse_stats)
			t0=0; t1=$(awk -v w="$wall" 'BEGIN{printf "%d", w * 1e6}')
			dir=$SNAP/$v/ckpt-$(printf %06d "$turn")
		else
			dir=$SNAP/$v/$name; [ "$v" = full-proc ] && [ "$name" != base ] && dir=$SNAP/$v/last
			rm -rf -- "${dir:?}"; rm -f -- "${OSOCK:?}" "${OSOCK:?}.lock"
			local dlog=$OUT/daemon_${name}_$v.log args
			args=$(common_args "$v"); is_diff "$v" && [ -n "$parent" ] && args="$args --parent $SNAP/$v/$parent"
			python3 -c '
import resource, subprocess, sys
rc = subprocess.call(sys.argv[2:], stdout=open(sys.argv[1], "w"), stderr=subprocess.STDOUT)
u = resource.getrusage(resource.RUSAGE_CHILDREN)
open(sys.argv[1] + ".cpu", "w").write("%.6f %d" % (u.ru_utime + u.ru_stime, rc))
' "$dlog" taskset -c "$DAEMON_CPUS" "$OD" snapshot --socket "$OSOCK" --output-dir "$dir" --compression "$CODEC" --workers "$WORKERS" $args &
			local dpid=$!
			for _ in $(seq 1 400); do [ -S "$OSOCK" ] && break; sleep 0.02; done
			for _ in $(seq 1 5000); do
				t0=$(date +%s%N)
				"$REM" --api-socket "$SOCK" send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on${dl:+,dirty_log=$dl}" > "$OUT/remote.log" 2>&1 && break
				grep -q "already in progress" "$OUT/remote.log" || break
				sleep 0.002
			done
			wait "$dpid"; t1=$(date +%s%N)
			read -r cpu rc < "$dlog.cpu"
			stats=$(grep -aE "Diff slot [0-9]+: " "$dlog" | parse_stats)
			[ "$name" != base ] && [ "$final" = 0 ] && rm -f -- "${dlog:?}" "${dlog:?}.cpu"
		fi
		out=$(find "$dir" -maxdepth 1 \( -name "*.compressed" -o -name "*.diff" \) -printf "%s\n" 2>/dev/null | awk '{s+=$1} END{print s+0}')
		echo "$SESSION,$PAGES,$name,$turn,$v,$j,$(( (t1 - t0) / 1000000 )),$cpu,$out,$stats,$rc" >> "$OUT/checkpoints.csv"
		[ "$rc" != 0 ] && log "checkpoint $name $v failed rc=$rc"
	done
	[ "$final" = 1 ] || "$REM" --api-socket "$SOCK" resume >/dev/null
}

restore() { # variant dir
	local v=$1 dir=$2 args="" t0 t1 rc gok=0 recv chain
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
	taskset -c "$DAEMON_CPUS" "$OD" restore --socket "$RDATA" --input-dir "$dir" --resume --workers "$RESTORE_WORKERS" $args > "$OUT/rdaemon_$v.log" 2>&1
	rc=$?; t1=$(date +%s%N); wait "$recv" 2>/dev/null
	chain=$(grep -aoE "Applied [0-9]+ diffs" "$OUT/rdaemon_$v.log" | head -1 | grep -oE "[0-9]+"); chain=${chain:-0}
	[ $rc -eq 0 ] && wait_marker AGENT_DIFF_POINT 60 && gok=1
	echo "$SESSION,$PAGES,$v,$chain,$(( (t1 - t0) / 1000000 )),$gok,$rc" >> "$OUT/restores.csv"
	log "restore $v (chain of $chain diffs): rc=$rc $(( (t1 - t0) / 1000000 )) ms, guest ok=$gok"
	"$REM" --api-socket "$RSOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
}

cp --sparse=always "$ROOTFS_TEMPLATE" "$RUNIMG"; rm -f -- "${SOCK:?}"
numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$SOCK" --kernel "$KERNEL" --initramfs "$INITRD" --disk "path=$RUNIMG,image_type=raw" \
	--cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=$STEPS" \
	--cpus boot=2 --memory "$MEM" --serial "file=$CONSOLE" --console off > "$OUT/ch.log" 2>&1 &
CHPID=$!
log "resident lifecycle $SESSION pages=$PAGES turns=$NSTEPS variants=${VARIANTS[*]}"
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
for v in "${VARIANTS[@]}"; do is_res "$v" && kill "${RPID[$v]}" 2>/dev/null; done
cp --sparse=always "$RUNIMG" "$FINALDISK"
for v in "${VARIANTS[@]}"; do
	if is_res "$v"; then restore "$v" "$SNAP/$v/ckpt-$(printf %06d "$NSTEPS")"
	elif [ "$v" = full-proc ]; then restore "$v" "$SNAP/$v/last"
	else restore "$v" "$SNAP/$v/step$NSTEPS"; fi
done
du -sm "$SNAP"/*/ | sed "s|$SNAP/||" | tee -a "$OUT/summary.txt"
rm -f -- "${RUNIMG:?}" "${FINALDISK:?}"; rm -rf -- "${SNAP:?}"
log "DONE -> $OUT"

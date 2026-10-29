#!/bin/bash
# lifecycle_cadence.sh <session> [4k|2m] -- checkpoint cadence on a real-agent
# lifecycle with resident offload daemons. Derived from lifecycle_resident.sh:
# every variant is a resident daemon, every checkpoint is taken by every
# variant from the same paused image (rotating order), the diff variants are
# fed the same dirty set, and a variant's codec comes from its name:
#
#   full-res          resident, full checkpoint, DSA classify + IAA (keeps last)
#   diff-dsa-res      resident, dirty-log diff, batched DSA compare/gather + IAA
#   full-res-lz4      resident, full, CPU classify + lz4 (CPU only)
#   diff-cpu-res-lz4  resident, diff, CPU compare/copy + lz4 (CPU only)
#
# EVERY=k checkpoints after every k-th agent turn (and always after the last
# one); EVERY=0 checkpoints only after the last turn. Between checkpoints the
# KVM dirty log accumulates, so a diff at turn k covers turns k-EVERY+1..k.
# AGENT_THINK=<secs> is the guest's pause after each turn (the checkpoint is
# taken inside it, VM paused). Wall = send-migration duration; CPU = delta of
# every daemon thread's run time (schedstat). After the last turn each
# variant's final checkpoint (chain) is restored and the guest must reach
# AGENT_DIFF_POINT.
set -u
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)
SESSION=${1:?session}; PAGES=${2:-4k}
EVERY=${EVERY:-1}; AGENT_THINK=${AGENT_THINK:-2}; AGENT_FIRST_WAIT=${AGENT_FIRST_WAIT:-2}
BIN_DIR=${BIN_DIR:-$REPO/target-iaa/release}
CH=$BIN_DIR/cloud-hypervisor; REM=$BIN_DIR/ch-remote; OD=${OFFLOAD_BIN:-$BIN_DIR/offload_daemon}
KERNEL=${KERNEL:-$HOME/vmbench/guest-kernel/vmlinuz-6.8.0-142-generic}; INITRD=${INITRD:-$HOME/vmbench/guest-kernel/initrd.img-6.8.0-142-generic}
ROOTFS_TEMPLATE=${ROOTFS_DIR:-$HOME/vmbench}/rootfs_traj_$SESSION.ext4
TRAJ=$HERE/traj/$SESSION.traj.json
read -r -a VARIANTS <<<"${VARIANTS:-full-res diff-dsa-res full-res-lz4 diff-cpu-res-lz4}"
CODEC=${CODEC:-qpl-hardware-static-async}; WORKERS=${WORKERS:-8}; RESTORE_WORKERS=${RESTORE_WORKERS:-32}
DIFF_BATCH=${DIFF_BATCH:-256}; DIFF_DEPTH=${DIFF_DEPTH:-8}
VM_CPUS=${VM_CPUS:-0-3}; DAEMON_CPUS=${DAEMON_CPUS:-4,5}
export DTO_WQ_LIST=${DTO_WQ_LIST:-"wq0.0;wq2.0;wq4.0;wq6.0"} DTO_IS_NUMA_AWARE=${DTO_IS_NUMA_AWARE:-1} DTO_LOG_LEVEL=${DTO_LOG_LEVEL:-1} RUST_LOG=${RUST_LOG:-info}
SNAP=${SNAP_ROOT:-/mnt/chsnap/lifecycle-cad}/$SESSION-$PAGES
SCRATCH=${SCRATCH:-$HOME/chlogs/lifecycle-scratch}
OUT=${OUT_ROOT:-$HOME/chlogs/lifecycle-cad}/$SESSION-$PAGES-e$EVERY-$(date +%Y%m%d_%H%M%S)
[ -f "$ROOTFS_TEMPLATE" ] && [ -f "$TRAJ" ] || { echo "missing rootfs or trajectory for $SESSION"; exit 1; }
mkdir -p "$OUT" "$SCRATCH"; rm -rf -- "${SNAP:?}"; mkdir -p "$SNAP"
# unix socket paths must stay under SUN_LEN (108): the daemons' sockets live
# in a short temporary directory, not under the (long) run directory
SOCKDIR=$(mktemp -d /tmp/cad.XXXXXX); trap 'rm -rf -- "$SOCKDIR"' EXIT
SOCK=$SOCKDIR/api.sock; RSOCK=$SOCKDIR/restore-api.sock; RDATA=$SOCKDIR/restore-data.sock
RUNIMG=$SCRATCH/$SESSION-$PAGES.cadrun.ext4; FINALDISK=$SCRATCH/$SESSION-$PAGES.cadfinal.ext4
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
echo "session,pages,every,checkpoint,turn,variant,order,wall_ms,cpu_s,out_bytes,dirty_pages,changed_pages,prepare_ms,compare_ms,gather_ms,compress_ms,populate_calls,dsa_ops,dsa_redo,dedup_pages,dedup_ms,rc" > "$OUT/checkpoints.csv"
echo "session,pages,every,variant,chain,restore_ms,guest_ok,rc" > "$OUT/restores.csv"

is_diff() { [[ $1 == diff-* ]]; }
cmp_of() { case $1 in diff-cpu-*) echo cpu ;; diff-dsa-*) echo dsa ;; esac; }
codec_of() { case $1 in *-lz4) echo lz4 ;; *-zstd) echo zstd ;; *) echo "$CODEC" ;; esac; }
classify_of() { case $1 in *-lz4|*-zstd|*-cpu*) echo cpu ;; *) echo dsa ;; esac; }
common_args() { # variant
	local v=$1
	if is_diff "$v"; then
		local dd=""; [[ $v == *-dedup* ]] && dd=" --dedup-image $ROOTFS_TEMPLATE"
		echo "--classify $(cmp_of "$v") --reference-dir $SNAP/$v/ref --diff-compare $(cmp_of "$v") --diff-batch $DIFF_BATCH --diff-depth $DIFF_DEPTH$dd"
	else
		echo "--classify $(classify_of "$v")"
	fi
}
newest_ckpt() { ls -d "$SNAP/$1"/ckpt-* 2>/dev/null | sort | tail -1; }
declare -A RPID
for v in "${VARIANTS[@]}"; do
	mkdir -p "$SNAP/$v"
	extra=""; is_diff "$v" || extra="--keep-only-last"
	taskset -c "$DAEMON_CPUS" "$OD" serve --socket "$SOCKDIR/$v.sock" --output-root "$SNAP/$v" --compression "$(codec_of "$v")" \
		--workers "$WORKERS" $(common_args "$v") $extra ${CKPT_EXTRA_ARGS:-} > "$OUT/daemon_$v.log" 2>&1 &
	RPID[$v]=$!
done
for v in "${VARIANTS[@]}"; do until [ -S "$SOCKDIR/$v.sock" ]; do sleep 0.05; kill -0 "${RPID[$v]}" 2>/dev/null || { log "daemon $v died at start"; exit 1; }; done; done

wait_marker() {
	local deadline=$(( $(date +%s) + $2 ))
	until grep -aq "$1" "$CONSOLE" 2>/dev/null; do
		kill -0 "$CHPID" 2>/dev/null || { log "CH exited while waiting for '$1'"; return 1; }
		[ "$(date +%s)" -ge "$deadline" ] && { log "timeout waiting for '$1'"; return 1; }
		sleep 0.2
	done
}
parse_stats() { # log text on stdin -> dirty,changed,prepare,compare,gather,compress,populate,dsa_ops,redo,dedup_pages,dedup_ms
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

checkpoint() { # name turn final
	local name=$1 turn=$2 final=$3 n=${#VARIANTS[@]} j v dl last_diff="" wall cpu rc out dir stats res
	"$REM" --api-socket "$SOCK" pause >/dev/null || return 1
	for ((j = 0; j < n; j++)); do v=${VARIANTS[$(( (j + turn) % n ))]}; is_diff "$v" && last_diff=$v; done
	for ((j = 0; j < n; j++)); do
		v=${VARIANTS[$(( (j + turn) % n ))]}
		dl=""; is_diff "$v" && { dl=keep; [ "$v" = "$last_diff" ] && dl=consume; }
		res=$(python3 "$HERE/ckpt_send.py" "$REM" "$SOCK" "$SOCKDIR/$v.sock" "${dl:--}" "$OUT/daemon_$v.log" "${RPID[$v]}")
		read -r wall cpu rc _ <<<"$res"
		stats=$(echo "$res" | parse_stats)
		dir=$(newest_ckpt "$v")
		out=$(find "$dir" -maxdepth 1 \( -name "*.compressed" -o -name "*.diff" \) -printf "%s\n" 2>/dev/null | awk '{s+=$1} END{print s+0}')
		echo "$SESSION,$PAGES,$EVERY,$name,$turn,$v,$j,$(awk -v w="$wall" 'BEGIN{printf "%d", w}'),$cpu,$out,$stats,$rc" >> "$OUT/checkpoints.csv"
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
	taskset -c "$DAEMON_CPUS" "$OD" restore --socket "$RDATA" --input-dir "$dir" --resume --workers "$RESTORE_WORKERS" $args ${RESTORE_EXTRA_ARGS:-} > "$OUT/rdaemon_$v.log" 2>&1
	rc=$?; t1=$(date +%s%N); wait "$recv" 2>/dev/null
	chain=$(grep -aoE "Applied [0-9]+ diffs" "$OUT/rdaemon_$v.log" | head -1 | grep -oE "[0-9]+"); chain=${chain:-0}
	[ $rc -eq 0 ] && wait_marker AGENT_DIFF_POINT 60 && gok=1
	echo "$SESSION,$PAGES,$EVERY,$v,$chain,$(( (t1 - t0) / 1000000 )),$gok,$rc" >> "$OUT/restores.csv"
	log "restore $v (chain of $chain diffs): rc=$rc $(( (t1 - t0) / 1000000 )) ms, guest ok=$gok"
	"$REM" --api-socket "$RSOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
}

cp --sparse=always "$ROOTFS_TEMPLATE" "$RUNIMG"; rm -f -- "${SOCK:?}"
numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$SOCK" --kernel "$KERNEL" --initramfs "$INITRD" --disk "path=$RUNIMG,image_type=raw" \
	--cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_think=$AGENT_THINK agent_first_wait=$AGENT_FIRST_WAIT agent_steps=$STEPS" \
	--cpus boot=2 --memory "$MEM" --serial "file=$CONSOLE" --console off > "$OUT/ch.log" 2>&1 &
CHPID=$!
log "cadence lifecycle $SESSION pages=$PAGES turns=$NSTEPS every=$EVERY think=$AGENT_THINK variants=${VARIANTS[*]} uffd_dma=$(cat /sys/kernel/uffd_dma/offloading 2>/dev/null) dcbm=$(cat /sys/module/dcbm/parameters/offloading 2>/dev/null) governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
wait_marker AGENT_BASE_POINT 300 || exit 1
checkpoint base 0 0
nck=1
for ((k = 1; k <= NSTEPS; k++)); do
	wait_marker "AGENT_STEP_DONE $k " 900 || exit 1
	if [ "$k" -eq "$NSTEPS" ] || { [ "$EVERY" -gt 0 ] && [ $((k % EVERY)) -eq 0 ]; }; then
		checkpoint "step$k" "$k" "$([ "$k" -eq "$NSTEPS" ] && echo 1 || echo 0)"
		nck=$((nck + 1))
	fi
	[ $((k % 10)) -eq 0 ] && log "  turn $k/$NSTEPS ($nck checkpoints so far)"
done
"$REM" --api-socket "$SOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
for v in "${VARIANTS[@]}"; do kill "${RPID[$v]}" 2>/dev/null; done
cp --sparse=always "$RUNIMG" "$FINALDISK"
for v in "${VARIANTS[@]}"; do restore "$v" "$(newest_ckpt "$v")"; done
du -sm "$SNAP"/*/ | sed "s|$SNAP/||" | tee -a "$OUT/summary.txt"
if [ "${KEEP_SNAP:-0}" = 1 ]; then
	log "KEEP_SNAP: checkpoints left in $SNAP, final disk $FINALDISK"; rm -f -- "${RUNIMG:?}"
else
	rm -f -- "${RUNIMG:?}" "${FINALDISK:?}"; rm -rf -- "${SNAP:?}"
fi
log "DONE -> $OUT"

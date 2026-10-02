#!/bin/bash
# lifecycle.sh <session> [4k|2m] -- one real-agent microVM lifecycle, every
# checkpoint taken by every offload-daemon variant from the same paused image.
#
#   boot (task image rootfs, RW copy)  -> AGENT_BASE_POINT
#   base checkpoint                     (each variant, VM paused throughout)
#   turn k runs in the guest            -> AGENT_STEP_DONE k
#   turn checkpoint                     (each variant, rotating order)
#   ... last turn: checkpoint, source VMM shut down while paused
#   restore last -> guest must reach AGENT_DIFF_POINT    (resume a session)
#   restore base -> guest must reach AGENT_STEP_DONE 1   (fork from warm base)
#
# Variants (VARIANTS): base = unmodified iaa-integration daemon
# (BASE_OFFLOAD_BIN); cpu / dsa / cpucrc / dsacrc = this branch's daemon with
# --classify cpu|dsa [--crc]. All use the same codec, chunk size and --workers;
# the daemon is pinned to DAEMON_CPUS (one core per memory-slot thread) and the
# VM to VM_CPUS, so only the classifier differs.
#
# Output: $OUT/checkpoints.csv, $OUT/restores.csv, $OUT/steps.csv, logs.
set -u
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)
SESSION=${1:?session, e.g. path-tracing_30b}; PAGES=${2:-4k}
BIN_DIR=${BIN_DIR:-$REPO/target-iaa/release}
CH=$BIN_DIR/cloud-hypervisor; REM=$BIN_DIR/ch-remote; OD=${OFFLOAD_BIN:-$BIN_DIR/offload_daemon}
BASE_OD=${BASE_OFFLOAD_BIN:-$HOME/ch-iaa-base/target/release/offload_daemon}
KERNEL=${KERNEL:-$HOME/vmbench/guest-kernel/vmlinuz-6.8.0-142-generic}; INITRD=${INITRD:-$HOME/vmbench/guest-kernel/initrd.img-6.8.0-142-generic}
ROOTFS_TEMPLATE=${ROOTFS_DIR:-$HOME/vmbench}/rootfs_traj_$SESSION.ext4
TRAJ=$HERE/traj/$SESSION.traj.json
read -r -a VARIANTS <<<"${VARIANTS:-base cpu dsa cpucrc dsacrc}"
CODEC=${CODEC:-qpl-hardware-static-async}; CHUNK=${CHUNK:-1048576}
WORKERS=${WORKERS:-8}; RESTORE_WORKERS=${RESTORE_WORKERS:-32}; DSA_DEPTH=${DSA_DEPTH:-32}
VM_CPUS=${VM_CPUS:-0-3}; DAEMON_CPUS=${DAEMON_CPUS:-4,5}; GUEST_MEM=${GUEST_MEM:-4G}; GUEST_VCPUS=${GUEST_VCPUS:-2}
export DTO_WQ_LIST=${DTO_WQ_LIST:-"wq0.0;wq2.0;wq4.0;wq6.0"} DTO_IS_NUMA_AWARE=${DTO_IS_NUMA_AWARE:-1} DTO_LOG_LEVEL=${DTO_LOG_LEVEL:-1}
SNAP=${SNAP_ROOT:-/mnt/chsnap/lifecycle}/$SESSION-$PAGES
SCRATCH=${SCRATCH:-$HOME/chlogs/lifecycle-scratch}
OUT=${OUT_ROOT:-$HOME/chlogs/lifecycle}/$SESSION-$PAGES-$(date +%Y%m%d_%H%M%S)
STEP_TIMEOUT=${STEP_TIMEOUT:-900}

[ -f "$ROOTFS_TEMPLATE" ] || { echo "missing $ROOTFS_TEMPLATE (build it with mkrootfs_traj.sh)"; exit 1; }
[ -f "$TRAJ" ] || { echo "missing $TRAJ"; exit 1; }
for v in "${VARIANTS[@]}"; do
	case $v in base|cpu|dsa|cpucrc|dsacrc) ;; *) echo "unknown variant $v"; exit 2 ;; esac
done
mkdir -p "$OUT" "$SCRATCH" "$SNAP"
for v in "${VARIANTS[@]}"; do rm -rf -- "${SNAP:?}/${v:?}"; done
SOCK=$OUT/api.sock; OSOCK=$OUT/offload.sock; RSOCK=$OUT/restore-api.sock; RDATA=$OUT/restore-data.sock
RUNIMG=$SCRATCH/$SESSION-$PAGES.run.ext4
BASEDISK=$SCRATCH/$SESSION-$PAGES.base-disk.ext4; FINALDISK=$SCRATCH/$SESSION-$PAGES.final-disk.ext4
CONSOLE=$OUT/console.log
log() { echo "$(date +%H:%M:%S) $*" | tee -a "$OUT/summary.txt"; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

# One token per turn ("traj-<session>:<turn>,..."), which every agent_init
# understands. Only when that would push the kernel command line past x86's
# 2048 bytes use the compact range form, which needs the range-aware
# agent_init in guest/ (images built before it boot and run a single step).
STEPS=$(python3 - "$TRAJ" "$SESSION" <<'PY'
import json, sys
t = json.load(open(sys.argv[1])); s = sys.argv[2]; ts = [x["turn"] for x in t["turns"]]
full = ",".join(f"traj-{s}:{n}" for n in ts)
print(full if len(full) < 1700 else f"traj-{s}:{min(ts)}-{max(ts)}")
PY
)
NSTEPS=$(python3 -c "import json;print(len(json.load(open('$TRAJ'))['turns']))")
MEM="size=$GUEST_MEM,shared=on"; [ "$PAGES" = 2m ] && MEM="$MEM,hugepages=on,hugepage_size=2M"

echo "session,pages,checkpoint,turn,variant,order,wall_ms,user_s,sys_s,out_bytes,zero_chunks,chunks,prefault_ms,dsa_submitted,dsa_fallback,rc" > "$OUT/checkpoints.csv"
echo "session,pages,checkpoint,variant,restore_ms,user_s,sys_s,guest_ms,guest_ok,rc" > "$OUT/restores.csv"
echo "session,pages,turn,guest_secs,rc" > "$OUT/steps.csv"

variant_args() { # variant -> extra daemon snapshot args
	case $1 in
		base) ;; cpu) echo "--classify cpu" ;; dsa) echo "--classify dsa --dsa-depth $DSA_DEPTH" ;;
		cpucrc) echo "--classify cpu --crc" ;; dsacrc) echo "--classify dsa --dsa-depth $DSA_DEPTH --crc" ;;
	esac
}
variant_bin() { if [ "$1" = base ]; then echo "$BASE_OD"; else echo "$OD"; fi; }

manifest_stats() { # dir -> "out_bytes,zero,chunks"
	python3 - "$1" <<'PY'
import json, os, sys
d = sys.argv[1]; out = zero = n = 0
for f in os.listdir(d):
    p = os.path.join(d, f)
    if f.endswith(".compressed"): out += os.path.getsize(p)
    if f.endswith(".index.json"):
        for c in json.load(open(p))["chunks"]:
            n += 1; zero += bool(c.get("zero"))
print(f"{out},{zero},{n}")
PY
}

wait_marker() { # marker timeout_s
	local deadline=$(( $(date +%s) + $2 ))
	until grep -aq "$1" "$CONSOLE" 2>/dev/null; do
		kill -0 "$CHPID" 2>/dev/null || { log "CH exited while waiting for '$1'"; tail -5 "$OUT/ch.log"; return 1; }
		[ "$(date +%s)" -ge "$deadline" ] && { log "timeout waiting for '$1'"; return 1; }
		sleep 0.2
	done
}

checkpoint() { # name turn final(0|1)
	local name=$1 turn=$2 final=$3 n=${#VARIANTS[@]} j v dir dpid t0 t1 rc tm cls pf ds df dlog
	"$REM" --api-socket "$SOCK" pause >/dev/null || { log "pause failed"; return 1; }
	for ((j = 0; j < n; j++)); do
		v=${VARIANTS[$(( (j + turn) % n ))]}
		dir=$SNAP/$v/$name; [ "$name" != base ] && dir=$SNAP/$v/last
		rm -rf -- "${dir:?}"; mkdir -p "$(dirname "$dir")"; rm -f -- "${OSOCK:?}" "${OSOCK:?}.lock"
		dlog=$OUT/daemon_${name}_$v.log
		/usr/bin/time -f "%e %U %S" -o "$OUT/t.txt" taskset -c "$DAEMON_CPUS" "$(variant_bin "$v")" snapshot \
			--socket "$OSOCK" --output-dir "$dir" --compression "$CODEC" --chunk-size "$CHUNK" \
			--workers "$WORKERS" $(variant_args "$v") > "$dlog" 2>&1 &
		dpid=$!
		for _ in $(seq 1 200); do [ -S "$OSOCK" ] && break; sleep 0.05; done
		t0=$(date +%s%N)
		"$REM" --api-socket "$SOCK" send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on" > "$OUT/remote.log" 2>&1
		wait "$dpid"; rc=$?; t1=$(date +%s%N)
		tm=$(tail -1 "$OUT/t.txt")
		cls=$(grep -ahoE "prefault=[0-9.]+ms|dsa submitted=[0-9]+ fallback=[0-9]+" "$dlog" | tr '\n' ' ')
		pf=$(echo "$cls" | grep -oE "prefault=[0-9.]+" | cut -d= -f2 | awk '{s+=$1} END{if (NR) printf "%.1f", s}')
		ds=$(echo "$cls" | grep -oE "submitted=[0-9]+" | cut -d= -f2 | awk '{s+=$1} END{if (NR) print s}')
		df=$(echo "$cls" | grep -oE "fallback=[0-9]+" | cut -d= -f2 | awk '{s+=$1} END{if (NR) print s}')
		echo "$SESSION,$PAGES,$name,$turn,$v,$j,$(( (t1 - t0) / 1000000 )),$(echo "$tm" | cut -d' ' -f2),$(echo "$tm" | cut -d' ' -f3),$(manifest_stats "$dir"),$pf,$ds,$df,$rc" >> "$OUT/checkpoints.csv"
		if [ $rc -ne 0 ]; then log "checkpoint $name variant $v failed rc=$rc"; tail -5 "$dlog" | tee -a "$OUT/summary.txt"; fi
		# keep the per-variant daemon logs of the base and final checkpoints only
		if [ "$name" != base ] && [ "$final" = 0 ]; then rm -f -- "${dlog:?}"; fi
	done
	[ "$final" = 1 ] || "$REM" --api-socket "$SOCK" resume >/dev/null
}

restore() { # checkpoint(base|last) variant disk marker timeout
	local cp=$1 v=$2 disk=$3 marker=$4 to=$5 args="" t0 t1 rc tm gstart gok=0 gms="" recv
	cp --sparse=always "$disk" "$RUNIMG"
	[ -f "$CONSOLE" ] && mv -f "$CONSOLE" "$OUT/console.before-restore-$cp-$v.log"
	rm -f -- "${RSOCK:?}" "${RDATA:?}"
	numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$RSOCK" > "$OUT/ch_restore_${cp}_$v.log" 2>&1 &
	CHPID=$!
	for _ in $(seq 1 200); do [ -S "$RSOCK" ] && break; sleep 0.05; done
	"$REM" --api-socket "$RSOCK" receive-migration "receiver_url=unix:$RDATA" > "$OUT/recv.log" 2>&1 &
	recv=$!
	for _ in $(seq 1 200); do [ -S "$RDATA" ] && break; sleep 0.05; done
	if [ "$v" != base ]; then
		args="--dsa-depth $DSA_DEPTH"; [ "$PAGES" = 2m ] && args="$args --hugetlb"
		case $v in *crc) args="$args --verify-crc" ;; esac
	fi
	t0=$(date +%s%N)
	/usr/bin/time -f "%e %U %S" -o "$OUT/t.txt" taskset -c "$DAEMON_CPUS" "$(variant_bin "$v")" restore --socket "$RDATA" \
		--input-dir "$SNAP/$v/$cp" --resume --workers "$RESTORE_WORKERS" $args > "$OUT/rdaemon_${cp}_$v.log" 2>&1
	rc=$?; t1=$(date +%s%N); wait "$recv" 2>/dev/null; tm=$(tail -1 "$OUT/t.txt")
	if [ $rc -eq 0 ]; then
		gstart=$(now_ms)
		if wait_marker "$marker" "$to"; then gok=1; gms=$(( $(now_ms) - gstart )); fi
	fi
	echo "$SESSION,$PAGES,$cp,$v,$(( (t1 - t0) / 1000000 )),$(echo "$tm" | cut -d' ' -f2),$(echo "$tm" | cut -d' ' -f3),$gms,$gok,$rc" >> "$OUT/restores.csv"
	log "restore $cp $v: rc=$rc $(( (t1 - t0) / 1000000 )) ms, guest '$marker' ok=$gok ${gms:+after $gms ms}"
	"$REM" --api-socket "$RSOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
}

# ---- boot
cp --sparse=always "$ROOTFS_TEMPLATE" "$RUNIMG"
rm -f -- "${SOCK:?}"
t_boot=$(now_ms)
numactl -C "$VM_CPUS" -m 0 "$CH" --api-socket "$SOCK" --kernel "$KERNEL" --initramfs "$INITRD" \
	--disk "path=$RUNIMG,image_type=raw" \
	--cmdline "console=ttyS0 root=/dev/vda rw init=/agent_init agent_wl=idle agent_warm= agent_steps=$STEPS" \
	--cpus "boot=$GUEST_VCPUS" --memory "$MEM" --serial "file=$CONSOLE" --console off > "$OUT/ch.log" 2>&1 &
CHPID=$!
log "session $SESSION pages=$PAGES turns=$NSTEPS variants=${VARIANTS[*]} workers=$WORKERS daemon_cpus=$DAEMON_CPUS"
wait_marker AGENT_BASE_POINT 300 || exit 1
log "boot to AGENT_BASE_POINT: $(( $(now_ms) - t_boot )) ms"
checkpoint base 0 0
"$REM" --api-socket "$SOCK" pause >/dev/null; cp --sparse=always "$RUNIMG" "$BASEDISK"; "$REM" --api-socket "$SOCK" resume >/dev/null

for ((k = 1; k <= NSTEPS; k++)); do
	wait_marker "AGENT_STEP_DONE $k " "$STEP_TIMEOUT" || exit 1
	line=$(grep -aE "AGENT_STEP_DONE $k " "$CONSOLE" | tail -1)
	wl=$(grep -aE "AGENT_WL2 traj-$SESSION seed=$((k - 1)) " "$CONSOLE" | tail -1)
	echo "$SESSION,$PAGES,$k,$(echo "$line" | grep -oE "secs=[0-9.]+" | cut -d= -f2),$(echo "$wl" | grep -oE "rc=-?[0-9]+" | cut -d= -f2)" >> "$OUT/steps.csv"
	checkpoint "step$k" "$k" "$([ $k -eq "$NSTEPS" ] && echo 1 || echo 0)"
	[ $((k % 5)) -eq 0 ] && log "  checkpointed turn $k/$NSTEPS"
done
"$REM" --api-socket "$SOCK" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill "$CHPID" 2>/dev/null; wait "$CHPID" 2>/dev/null
cp --sparse=always "$RUNIMG" "$FINALDISK"
log "session done; restoring"

for v in "${VARIANTS[@]}"; do
	restore last "$v" "$FINALDISK" AGENT_DIFF_POINT 60
	restore base "$v" "$BASEDISK" "AGENT_STEP_DONE 1 " 300
done
rm -f -- "${RUNIMG:?}" "${BASEDISK:?}" "${FINALDISK:?}"
log "DONE -> $OUT"

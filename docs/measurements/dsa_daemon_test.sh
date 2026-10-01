#!/bin/bash
# dsa_daemon_test.sh -- functional + timing check of the DSA-enabled offload
# daemon on a 4 GiB warm guest: snapshot cells (cpu / dsa / dsa+crc /
# dsa no-prefault), manifest equivalence, then restore with --verify-crc and
# --populate dsa and confirm the restored guest keeps running; finally a
# corrupted-payload restore must be refused.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote; OD=$B/offload_daemon
KERN=/boot/vmlinuz-6.8.0-142-generic; INITRD=/boot/initrd.img-6.8.0-142-generic
ROOTFS=$HOME/vmbench/rootfs_full.ext4
WARM=${WARM-all}; HUGE=${HUGE-on}
OUT=$HOME/chlogs/dsadaemon_${WARM:-idle}_h${HUGE}_$(date +%Y%m%d_%H%M); mkdir -p $OUT
SOCK=/tmp/chdsa.sock; OSOCK=/tmp/chdsa_offload.sock; RSOCK=/tmp/chdsa_restore_api.sock; RDATA=/tmp/chdsa_restore_data.sock
SNAPROOT=${SNAPROOT:-/mnt/chsnap/dsasnap}; sudo mkdir -p $SNAPROOT
DTOENV="DTO_WQ_LIST=wq0.0;wq2.0;wq4.0;wq6.0 DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info"
log() { echo "$(date +%H:%M:%S) $*" | tee -a $OUT/summary.txt; }
MEM="size=4G,shared=on"; [ "$HUGE" = on ] && MEM="size=4G,hugepages=on,hugepage_size=2M,shared=on"

sudo rm -f $SOCK $OSOCK $RSOCK $RDATA
sudo numactl -N0 -m0 $CH --api-socket $SOCK -v --kernel $KERN --initramfs $INITRD \
  --disk path=$ROOTFS,readonly=on,image_type=raw \
  --cmdline "console=ttyS0 root=/dev/vda ro init=/agent_init agent_wl=idle agent_warm=$WARM agent_steps=" \
  --cpus boot=2 --memory $MEM --serial file=$OUT/console.log --console off > $OUT/ch.log 2>&1 &
CHPID=$!
for i in $(seq 1 1500); do sudo grep -q AGENT_BASE_POINT $OUT/console.log 2>/dev/null && break; kill -0 $CHPID 2>/dev/null || { log "CH exited"; tail -20 $OUT/ch.log; exit 1; }; sleep 0.2; done
sudo $REM --api-socket $SOCK pause && log "guest paused at AGENT_BASE_POINT (warm=${WARM:-none}, hugepages=$HUGE)"

snap() {  # snap <name> <daemon args...>
  local name=$1; shift
  sudo rm -rf $SNAPROOT/$name; sudo rm -f $OSOCK
  sudo env $DTOENV /usr/bin/time -f "wall=%e user=%U sys=%S" -o $OUT/time_$name.txt numactl -N0 -m0 \
    $OD snapshot --socket $OSOCK --output-dir $SNAPROOT/$name --compression qpl-hardware-static-async --chunk-size 1048576 --workers 8 "$@" > $OUT/daemon_$name.log 2>&1 &
  local DPID=$!
  for i in $(seq 1 150); do [ -S $OSOCK ] && break; sleep 0.1; done
  local t0=$(date +%s%N)
  sudo $REM --api-socket $SOCK send-migration "destination_url=unix:$OSOCK,memory_mode=memfds,preserve_source=on" > $OUT/remote_$name.log 2>&1 || log "  send-migration rc=$?"
  wait $DPID; local rc=$?; local t1=$(date +%s%N)
  log "$name: migration wall $(( (t1-t0)/1000000 )) ms, daemon rc=$rc, $(cat $OUT/time_$name.txt), out=$(sudo du -sm $SNAPROOT/$name | cut -f1) MiB"
  grep -hE "classify=|Compressed slot" $OUT/daemon_$name.log | sed -E "s/^.*INFO +offload_daemon(::compression)?\] /    /" | cut -c1-190 | tee -a $OUT/summary.txt
  [ $rc -ne 0 ] && tail -5 $OUT/daemon_$name.log | tee -a $OUT/summary.txt
}
snap cpu        --classify cpu
snap dsa        --classify dsa
snap dsa_crc    --classify dsa --crc
snap dsa_nopf   --classify dsa --no-prefault
snap cpu_crc    --classify cpu --crc

log "manifest equivalence (cpu vs dsa, ignoring crc):"
sudo python3 - $SNAPROOT <<'PY' | tee -a $OUT/summary.txt
import json, sys, os
root=sys.argv[1]
def load(n):
    out={}
    for f in sorted(os.listdir(os.path.join(root,n))):
        if f.endswith(".index.json"):
            m=json.load(open(os.path.join(root,n,f)))
            out[f]=[(c["uncompressed_offset"],c["uncompressed_length"],c.get("zero",False),c["compressed_length"]) for c in m["chunks"]]
    return out
ref=load("cpu")
for n in ["dsa","dsa_crc","dsa_nopf","cpu_crc"]:
    m=load(n); same=all(ref[f]==m[f] for f in ref) and set(ref)==set(m)
    nz=sum(1 for f in m for c in m[f] if c[2]); tot=sum(len(m[f]) for f in m)
    print("  %-8s zero %d/%d  records %s cpu" % (n, nz, tot, "IDENTICAL to" if same else "DIFFER from"))
crc=json.load(open(os.path.join(root,"dsa_crc","memory-0.index.json")))["chunks"]
have=[c for c in crc if "crc32c" in c]; z=[c for c in crc if c.get("zero")]
print("  dsa_crc slot0: %d records carry crc32c, %d zero records carry none: %s" % (len(have), len(z), "ok" if all("crc32c" not in c for c in z) and len(have)==len(crc)-len(z) else "WRONG"))
a=json.load(open(os.path.join(root,"dsa_crc","memory-0.index.json")))["chunks"]; b=json.load(open(os.path.join(root,"cpu_crc","memory-0.index.json")))["chunks"]
print("  dsa crc == software crc for every slot0 chunk: %s" % ("ok" if [c.get("crc32c") for c in a]==[c.get("crc32c") for c in b] else "MISMATCH"))
PY

# ---- restore: shut the source down, restore the crc snapshot with verify + populate dsa
sudo $REM --api-socket $SOCK shutdown-vmm > /dev/null 2>&1; sleep 1; sudo kill $CHPID 2>/dev/null; wait $CHPID 2>/dev/null
restore() {  # restore <name> <snapdir> <daemon args...>
  local name=$1 dir=$2; shift 2
  sudo rm -f $RSOCK $RDATA
  sudo numactl -N0 -m0 $CH --api-socket $RSOCK -v > $OUT/ch_restore_$name.log 2>&1 &
  local RPID=$!
  for i in $(seq 1 100); do sudo test -S $RSOCK && break; sleep 0.1; done
  sudo $REM --api-socket $RSOCK receive-migration receiver_url=unix:$RDATA > $OUT/recv_$name.log 2>&1 &
  local RECV=$!
  for i in $(seq 1 100); do sudo test -S $RDATA && break; sleep 0.1; done
  local t0=$(date +%s%N)
  sudo env $DTOENV /usr/bin/time -f "wall=%e user=%U sys=%S" -o $OUT/rtime_$name.txt numactl -N0 -m0 \
    $OD restore --socket $RDATA --input-dir $dir --resume --workers 8 "$@" > $OUT/rdaemon_$name.log 2>&1
  local rc=$?; local t1=$(date +%s%N)
  wait $RECV 2>/dev/null
  log "restore $name: rc=$rc wall $(( (t1-t0)/1000000 )) ms, $(cat $OUT/rtime_$name.txt)"
  grep -hE "Decompressed|Populated|crc verify|populate:|Error|error" $OUT/rdaemon_$name.log | sed -E "s/^.*(INFO|ERROR) +offload_daemon(::compression)?\] /    /" | cut -c1-190 | tee -a $OUT/summary.txt
  if [ $rc -eq 0 ]; then
    # the guest resumes at AGENT_BASE_POINT and must reach AGENT_DIFF_POINT on its own
    local ok=no; for i in $(seq 1 300); do sudo grep -q AGENT_DIFF_POINT $OUT/console.log 2>/dev/null && { ok=yes; break; }; sleep 0.1; done
    log "    restored guest progressed to AGENT_DIFF_POINT: $ok"
  fi
  sudo $REM --api-socket $RSOCK shutdown-vmm > /dev/null 2>&1; sleep 1; sudo kill $RPID 2>/dev/null; wait $RPID 2>/dev/null
}
restore verify_populate_dsa $SNAPROOT/dsa_crc --verify-crc --populate dsa
sudo truncate -s 0 $OUT/console.log 2>/dev/null
restore verify_populate_cpu $SNAPROOT/dsa_crc --verify-crc --populate cpu
# corrupt one byte of a compressed chunk; verify must refuse
sudo cp -r $SNAPROOT/dsa_crc $SNAPROOT/corrupt
off=$(sudo python3 -c "import json;m=json.load(open('$SNAPROOT/corrupt/memory-1.index.json'));c=[x for x in m['chunks'] if not x.get('zero')][5];print(c['compressed_offset']+7)")
sudo python3 -c "f=open('$SNAPROOT/corrupt/memory-1.compressed','r+b');f.seek($off);b=f.read(1);f.seek($off);f.write(bytes([b[0]^0xff]))"
sudo truncate -s 0 $OUT/console.log 2>/dev/null
restore corrupt_refused $SNAPROOT/corrupt --verify-crc
log "DONE -> $OUT"

#!/bin/bash
# cmp_chain.sh <chain-tip-dir> <4k|2m> <disk>: restore the same diff chain
# twice into PAUSED VMMs, with the ordered per-diff apply (target-iaa) and the
# one-pass chain apply (target-iaa2), raw-dump both restored memories (base
# daemon, raw codec) and compare them page by page.
set -u
SNAPDIR=${1:?}; PAGES=${2:-4k}; DISK=${3:?}
B=$HOME/cloud-hypervisor/target-iaa/release; CH=$B/cloud-hypervisor; REM=$B/ch-remote
OLD=$B/offload_daemon; NEW=$HOME/cloud-hypervisor/target-iaa2/release/offload_daemon
RAW=$HOME/ch-iaa-base/target/release/offload_daemon
OUT=$HOME/chlogs/cmp_chain_${PAGES}_$(date +%m%d_%H%M%S); mkdir -p "$OUT"; SNAP=/mnt/chsnap/cmp_chain; rm -rf $SNAP; mkdir -p $SNAP
export DTO_WQ_LIST="wq0.0;wq2.0;wq4.0;wq6.0" DTO_IS_NUMA_AWARE=1 DTO_LOG_LEVEL=1 RUST_LOG=info
HT=""; [ "$PAGES" = 2m ] && HT="--hugetlb"
# the checkpoint's VM config names the (deleted) per-run disk: put the kept final disk there
CFGDISK=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['vm_config']['disks'][0]['path'])" "$SNAPDIR/migration_config.json")
echo "disk in config: $CFGDISK <- $DISK"
one() { # name daemon
  local name=$1 od=$2
  cp --sparse=always "$DISK" "$CFGDISK"
  rm -f "$OUT/api.sock" "$OUT/data.sock" "$OUT/off.sock"
  numactl -C 0-3 -m 0 "$CH" --api-socket "$OUT/api.sock" > "$OUT/ch_$name.log" 2>&1 & local chpid=$!
  for _ in $(seq 1 200); do [ -S "$OUT/api.sock" ] && break; sleep 0.05; done
  "$REM" --api-socket "$OUT/api.sock" receive-migration "receiver_url=unix:$OUT/data.sock" >/dev/null 2>&1 & local recv=$!
  for _ in $(seq 1 200); do [ -S "$OUT/data.sock" ] && break; sleep 0.05; done
  local t0=$(date +%s%N)
  timeout 120 taskset -c 4,5 "$od" restore --socket "$OUT/data.sock" --input-dir "$SNAPDIR" --workers 32 $HT > "$OUT/d_$name.log" 2>&1; local rc=$?; local t1=$(date +%s%N); wait $recv
  echo "$name: restore rc=$rc $(( (t1 - t0) / 1000000 )) ms; $(grep -ao "Applied .*" "$OUT/d_$name.log" | head -2 | tr '\n' ';')"
  if [ $rc -eq 0 ]; then
    "$RAW" snapshot --socket "$OUT/off.sock" --output-dir "$SNAP/$name" > "$OUT/raw_$name.log" 2>&1 & local rd=$!
    for _ in $(seq 1 200); do [ -S "$OUT/off.sock" ] && break; sleep 0.05; done
    timeout 120 "$REM" --api-socket "$OUT/api.sock" send-migration "destination_url=unix:$OUT/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null 2>&1 || echo "$name: raw dump send-migration failed"
    for _ in $(seq 1 600); do kill -0 $rd 2>/dev/null || break; sleep 0.2; done
    kill $rd 2>/dev/null; wait $rd 2>/dev/null
  else
    tail -2 "$OUT/d_$name.log" | cut -c1-200
  fi
  "$REM" --api-socket "$OUT/api.sock" shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $chpid 2>/dev/null; wait $chpid 2>/dev/null
}
for i in 1 2; do one "ordered$i" "$OLD"; one "onepass$i" "$NEW"; done
python3 - $SNAP <<'PY'
import mmap, sys, numpy as np, os, glob
S = sys.argv[1]
def pages(p):
    f = open(p, "rb"); m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    return np.frombuffer(m, dtype=np.uint8).reshape(-1, 4096)
files = sorted(os.path.basename(p) for p in glob.glob(f"{S}/ordered1/memory-*") if "." not in os.path.basename(p))
print("dumps:", sorted(os.path.basename(d) for d in glob.glob(f"{S}/*")), "files:", files)
for fn in files:
    a = pages(f"{S}/ordered1/{fn}")
    for other in ("onepass1", "onepass2", "ordered2"):
        if not os.path.exists(f"{S}/{other}/{fn}"):
            print(f"{fn}: {other} missing"); continue
        b = pages(f"{S}/{other}/{fn}")
        d = np.flatnonzero((a != b).any(axis=1)) if a.shape == b.shape else None
        print(f"{fn}: ordered1 vs {other}: {a.shape[0]} pages, differ={len(d) if d is not None else 'size mismatch'} {[hex(int(x)*4096) for x in d[:4]] if d is not None else ''}")
PY
rm -rf $SNAP
echo CMP_CHAIN_DONE

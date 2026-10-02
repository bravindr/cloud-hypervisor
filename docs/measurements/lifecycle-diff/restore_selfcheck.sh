#!/bin/bash
# Restore the raw ground truth itself, dump it, and diff page by page.
set -u
B=$HOME/cloud-hypervisor/target-iaa/release; RAW=$HOME/ch-iaa-base/target/release/offload_daemon
S=/mnt/chsnap/verifydiff/write-compressor_30b-4k; O=$HOME/chlogs/verifydiff/selfcheck; rm -rf -- "${O:?}"; mkdir -p $O
rm -rf -- "${S:?}/truth-rt"
$B/cloud-hypervisor --api-socket $O/r.sock > $O/ch.log 2>&1 & R=$!
until [ -S $O/r.sock ]; do sleep 0.05; done
$B/ch-remote --api-socket $O/r.sock receive-migration receiver_url=unix:$O/rd.sock > /dev/null 2>&1 & RV=$!
until [ -S $O/rd.sock ]; do sleep 0.05; done
$RAW restore --socket $O/rd.sock --input-dir $S/truth > $O/restore.log 2>&1; wait $RV
$RAW snapshot --socket $O/off.sock --output-dir $S/truth-rt > $O/dump.log 2>&1 & D=$!
until [ -S $O/off.sock ]; do sleep 0.05; done
$B/ch-remote --api-socket $O/r.sock send-migration "destination_url=unix:$O/off.sock,memory_mode=memfds,preserve_source=on" >/dev/null; wait $D
$B/ch-remote --api-socket $O/r.sock shutdown-vmm >/dev/null 2>&1; sleep 0.5; kill $R 2>/dev/null
python3 - <<'PY'
import mmap, numpy as np
S = "/mnt/chsnap/verifydiff/write-compressor_30b-4k"
def pages(p):
    f = open(p, "rb"); m = mmap.mmap(f.fileno(), 0, prot=mmap.PROT_READ)
    return np.frombuffer(m, dtype=np.uint8).reshape(-1, 4096)
t = pages(S + "/truth/memory-0")
for name in ("truth-rt", "check-none", "check-cpu", "check-dsa"):
    c = pages(f"{S}/{name}/memory-2")
    diff = np.flatnonzero((t != c).any(axis=1))
    print(f"{name:11} slot0 differing pages: {len(diff):5d}  first: {[hex(int(x) * 4096) for x in diff[:6]]}")
PY

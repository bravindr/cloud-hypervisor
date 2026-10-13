#!/bin/bash
# Snapshots of the write-compressor_30b session at its last turn, kept on
# /mnt/chsnap: 4k and 2m with 1 MiB chunks, 2m with 2 MiB chunks (for
# UFFDIO_COPY, which needs whole huge pages).
cd ~/cloud-hypervisor/benchmark/agent || exit 1
export OFFLOAD_BIN=$HOME/cloud-hypervisor/target-iaa2/release/offload_daemon
for pg in 4k 2m; do
  echo "######## snap $pg start $(date +%T)"
  SKIP_RESTORE=1 VARIANTS="dsa" OUT_ROOT=$HOME/chlogs/snaps-restore bash lifecycle.sh write-compressor_30b $pg > ~/chlogs/snaps_${pg}.log 2>&1
  echo "######## snap $pg rc=$? end $(date +%T)"
done
echo "######## snap 2m/2MiB-chunk start $(date +%T)"
SNAP_ROOT=/mnt/chsnap/lifecycle-2mc CHUNK=2097152 SKIP_RESTORE=1 VARIANTS="dsa" OUT_ROOT=$HOME/chlogs/snaps-restore bash lifecycle.sh write-compressor_30b 2m > ~/chlogs/snaps_2m_2mc.log 2>&1
echo "######## snap 2m/2MiB-chunk rc=$? end $(date +%T)"
ls -la /mnt/chsnap/lifecycle/write-compressor_30b-4k/dsa/last /mnt/chsnap/lifecycle/write-compressor_30b-2m/dsa/last /mnt/chsnap/lifecycle-2mc/write-compressor_30b-2m/dsa/last
echo MK_SNAPS_DONE

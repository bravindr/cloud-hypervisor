#!/bin/bash
# End to end: CPU only (lz4, plain restore, providers off) vs DSA + IAA (IAA
# static, UFFDIO_COPY restore on 2 MiB guests with the DSA copy provider and
# the clear offload on). Both ladders run cpu and dsa classification.
cd ~/cloud-hypervisor/benchmark/agent || exit 1
K=/sys/kernel/uffd_dma; M=/sys/module/mm_offload_dma/parameters
export RESTORE_WORKERS=8
prep() { rm -rf /mnt/chsnap/lifecycle; sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null; numactl -H | grep "node 0 free"; cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor; }
echo 0 | sudo tee $K/offloading /sys/module/dcbm/parameters/offloading >/dev/null
prep; echo "######## CPU-only run (lz4) start $(date +%T)"
CODEC=lz4 VARIANTS="cpu dsa" OUT_ROOT=$HOME/chlogs/e2e-cpu bash run_all.sh 4k 2m > ~/chlogs/e2e_cpu_run.log 2>&1
echo "######## CPU-only run rc=$? end $(date +%T)"
echo 8192 | sudo tee $M/map_cache_cap_mb >/dev/null; echo 1 | sudo tee $K/dev_by_pfn $K/dst_map_cache >/dev/null
echo 1 | sudo tee $K/offloading /sys/module/dcbm/parameters/offloading >/dev/null; sudo chmod 666 /dev/uffd_dma
prep; echo "######## DSA+IAA run (qpl static) 4k start $(date +%T)"
CODEC=qpl-hardware-static-async VARIANTS="cpu dsa" OUT_ROOT=$HOME/chlogs/e2e-acc bash run_all.sh 4k > ~/chlogs/e2e_acc_4k_run.log 2>&1
echo "######## DSA+IAA 4k rc=$? end $(date +%T)"
prep; echo "######## DSA+IAA run 2m (2 MiB chunks, --uffd-copy) start $(date +%T)"
CHUNK=2097152 RESTORE_EXTRA_ARGS="--uffd-copy" CODEC=qpl-hardware-static-async VARIANTS="cpu dsa" OUT_ROOT=$HOME/chlogs/e2e-acc bash run_all.sh 2m > ~/chlogs/e2e_acc_2m_run.log 2>&1
echo "######## DSA+IAA 2m rc=$? end $(date +%T)"
for a in copies_done copies_refused copies_failed regbufs_invalidated src_registered; do echo "$a=$(cat $K/$a)"; done; cat $M/map_cache_stats | head -4
echo 0 | sudo tee $K/offloading /sys/module/dcbm/parameters/offloading >/dev/null; echo 4096 | sudo tee $M/map_cache_cap_mb >/dev/null
echo RUN_E2E_DONE

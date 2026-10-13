#!/bin/bash
# CPU-only (lz4, zstd-1) lifecycle baselines through the shared pipeline, performance governor.
cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor
cd ~/cloud-hypervisor/benchmark/agent
for codec in lz4 zstd; do
  rm -rf /mnt/chsnap/lifecycle; sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
  numactl -H | grep "node 0 free"
  export CODEC=$codec VARIANTS="cpu dsa cpucrc dsacrc" OUT_ROOT=$HOME/chlogs/lifecycle-sw-$codec
  echo "######## $codec run start $(date +%T)"
  bash run_all.sh 4k 2m > ~/chlogs/lifecycle_sw_${codec}_run.log 2>&1; echo "######## $codec run rc=$? end $(date +%T)"
done
echo RUN_SW_DONE

#!/bin/bash
# resident lifecycle: software codec (lz4) then IAA static, same 3 resident variants
cd ~/cloud-hypervisor/benchmark/agent || exit 1
export VARIANTS="full-res diff-cpu-res diff-dsa-res"
cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor
for codec in lz4 qpl-hardware-static-async; do
  tag=$codec; [ $codec = qpl-hardware-static-async ] && tag=iaa
  rm -rf /mnt/chsnap/lifecycle-res; sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
  numactl -H | grep "node 0 free"
  echo "######## $tag run start $(date +%T)"
  CODEC=$codec OUT_ROOT=$HOME/chlogs/lifecycle-res-$tag bash run_all_resident.sh 4k 2m > ~/chlogs/lifecycle_res_${tag}_run.log 2>&1
  echo "######## $tag run rc=$? end $(date +%T)"
done
echo RUN_RES_DONE

#!/bin/bash
# Lifecycle suite + NVMe pipeline sweep with --submitters picked from affinity, performance governor.
cat /sys/devices/system/cpu/cpu4/cpufreq/scaling_governor
cd ~/cloud-hypervisor/benchmark/agent
rm -rf /mnt/chsnap/lifecycle; sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
numactl -H | grep "node 0 free"
export VARIANTS="base cpu dsa cpucrc dsacrc" OUT_ROOT=$HOME/chlogs/lifecycle-perf7
echo "######## full run start $(date +%T)"
bash run_all.sh 4k 2m > ~/chlogs/lifecycle_perf7_run.log 2>&1; echo "######## full run rc=$? end $(date +%T)"
cd ~/cloud-hypervisor/benchmark
sed -e "s#results-pipeline-nvme-sub7#results-pipeline-nvme-sub8#" benchmark_pipeline_nvme_sub7.env > benchmark_pipeline_nvme_sub8.env
cd ~/cloud-hypervisor
sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
numactl -H | grep "node 0 free"
echo "######## nvme sweep start $(date +%T)"
BENCHMARK_CONFIG=$PWD/benchmark/benchmark_pipeline_nvme_sub8.env bash benchmark/benchmark.sh > ~/chlogs/bench_pipeline_nvme_sub8.log 2>&1; echo "######## nvme sweep rc=$? end $(date +%T)"
rm -rf /mnt/nvme/chharness/*
echo RERUN_PERF7_DONE

cd ~/cloud-hypervisor
( while true; do echo "$(date +%T) $(numactl -H | grep free | tr "\n" " ")"; sleep 30; done ) > ~/chlogs/pipeline_free.log 2>&1 &
MON=$!
for v in disk tmpfs; do
  sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null
  echo "######## $v start $(date +%T)"
  BENCHMARK_CONFIG=$PWD/benchmark/benchmark_pipeline_$v.env bash benchmark/benchmark.sh > ~/chlogs/bench_pipeline_$v.log 2>&1
  echo "######## $v rc=$? end $(date +%T)"
  rm -rf /mnt/chsnap/harness
done
kill $MON
echo PIPELINE_SWEEPS_DONE

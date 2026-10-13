#!/bin/bash
# All restore cells on the three write-compressor_30b last-turn snapshots.
S4=/mnt/chsnap/lifecycle/write-compressor_30b-4k/dsa/last
S2=/mnt/chsnap/lifecycle/write-compressor_30b-2m/dsa/last
S2C=/mnt/chsnap/lifecycle-2mc/write-compressor_30b-2m/dsa/last
R=~/chlogs/restore_cells2.sh
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null; numactl -H | grep "node 0 free"
echo "######## 4k cells $(date +%T)"
REPS=3 bash $R $S4 4k "c2w32:4,5:32:0:0:" "c8w32:4-11:32:0:0:" "c2w8:4,5:8:0:0:" "c2w32crc:4,5:32:0:0:--verify-crc"
PERF=1 REPS=1 bash $R $S4 4k-perf "c2w32:4,5:32:0:0:"
echo "######## 2m cells (1 MiB chunks) $(date +%T)"
REPS=3 bash $R $S2 2m "c2w32:4,5:32:0:0:--hugetlb" "c8w32:4-11:32:0:0:--hugetlb" "c2w32clr:4,5:32:0:1:--hugetlb"
echo "######## 2m cells (2 MiB chunks) $(date +%T)"
REPS=3 bash $R $S2C 2mc "cpu:4,5:32:0:0:--hugetlb" "cpuclr:4,5:32:0:1:--hugetlb" "uffdcpu:4,5:32:0:0:--hugetlb --uffd-copy" "uffddma:4,5:32:1:0:--hugetlb --uffd-copy" "cpu8:4-11:32:0:0:--hugetlb" "uffddma8:4-11:32:1:0:--hugetlb --uffd-copy"
PERF=1 REPS=1 bash $R $S2C 2mc-perf "cpu:4,5:32:0:0:--hugetlb" "uffddma:4,5:32:1:0:--hugetlb --uffd-copy"
echo RESTORE_CELLS_DONE

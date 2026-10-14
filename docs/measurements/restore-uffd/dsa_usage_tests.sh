#!/bin/bash
# Exercise each mm_offload function that goes to DSA and read the usage
# counters over a tight window around each run.
W=/sys/kernel/mm_offload_dma/window; T=/sys/kernel/mm_offload_dma/totals
B=~/chlogs/mmbench; K=/sys/kernel/uffd_dma; D=/sys/module/dcbm/parameters
show() { awk '/window_ms/ {print "    " $0; next} /^(clear|migrate|uffd_copy)/ {if ($3 != 0) print "    " $0; next} {z=1; for (i=2;i<=NF;i++) if ($i ~ /^[0-9]+$/ && $i != 0) z=0; if (!z) print "    " $0}' $W; }
run() { local name=$1; shift; echo "== $name"; echo "   cmd: $*"; echo 1 | sudo tee $W >/dev/null; local t0=$(date +%s%N); "$@" 2>&1 | grep -vE "^$" | sed 's/^/   | /' | tail -${TAIL:-4}; echo "   wall $(( ($(date +%s%N)-t0)/1000000 )) ms"; show; }
echo 1 | sudo tee $D/offloading $K/offloading >/dev/null; echo 8192 | sudo tee /sys/module/mm_offload_dma/parameters/map_cache_cap_mb >/dev/null
echo 1 | sudo tee $K/dev_by_pfn $K/dst_map_cache >/dev/null
echo "pool: $(dmesg | grep -o "mm_offload_dma: [0-9]* channel(s) in [0-9]* group(s), [0-9]* user(s)" | tail -1)"
echo "hugepages free node0/node1: $(cat /sys/devices/system/node/node*/hugepages/hugepages-2048kB/free_hugepages | tr '\n' ' ')"
run "clear: THP faults, 8 threads x 1 GiB on node 0" numactl -N0 -m0 $B/thp_zero_bench -t 8 -s 1024
run "clear: hugetlb faults, 4 threads x 1 GiB on node 0" numactl -N0 -m0 $B/thp_zero_bench -t 4 -s 1024 -H
run "migrate: move_pages THP 2 GiB node0->node1, 4 threads, 2 iters" numactl -C 16-31 $B/migbench -s 2048 -f 0 -t 1 -T 4 -i 2 -H -v
echo 1024 | sudo tee /sys/devices/system/node/node1/hugepages/hugepages-2048kB/nr_hugepages >/dev/null
echo "hugepages free node0/node1: $(cat /sys/devices/system/node/node*/hugepages/hugepages-2048kB/free_hugepages | tr '\n' ' ')"
run "migrate: move_pages hugetlb 1 GiB node0->node1, 2 threads, 2 iters" numactl -C 16-31 $B/migbench -s 1024 -f 0 -t 1 -T 2 -i 2 -L -v
echo 0 | sudo tee /sys/devices/system/node/node1/hugepages/hugepages-2048kB/nr_hugepages >/dev/null
echo "== uffd_copy: UFFDIO_COPY restore x3 (restore_cells2.sh prints the window over the cell)"
TAIL=40 REPS=3 bash ~/chlogs/restore_cells2.sh /mnt/chsnap/lifecycle-2mc/write-compressor_30b-2m/dsa/last usage-uffd "final:4,5:32:1:0:--hugetlb --uffd-copy" 2>&1 | grep -E "rc=|window_ms|^    (clear|migrate|uffd_copy)|0000:" | awk '/0000:/ {z=1; for (i=2;i<=NF;i++) if ($i ~ /^[0-9]+$/ && $i != 0) z=0; if (!z) print; next} {print}' | cut -c1-200
echo "== running totals"; cat $T
echo 0 | sudo tee $D/offloading >/dev/null

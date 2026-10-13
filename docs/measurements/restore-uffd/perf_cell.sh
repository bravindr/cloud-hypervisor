#!/bin/bash
# perf_cell.sh <snapdir> <tag> <cell>  -- one restore with a system-wide perf
# record on the daemon's cores (4,5), then the report.
SNAP=$1; TAG=$2; CELL=$3
OUTP=~/chlogs/perf_$TAG.data
sudo rm -f $OUTP
sudo perf record -C 4,5 -g -F 2999 -o $OUTP -- sleep 6 > /dev/null 2>&1 &
PP=$!
sleep 1
REPS=1 bash ~/chlogs/restore_cells2.sh "$SNAP" "perf-$TAG" "$CELL" | grep -E "rc=|counters"
wait $PP
echo "== report $TAG (daemon comm only, no children)"
sudo perf report -i $OUTP --stdio --no-children --comm offload_daemon --percent-limit 1.2 2>/dev/null | grep -vE "^#|^$" | grep -E "^\s+[0-9]+\.[0-9]+%" | head -40
echo "== by dso"
sudo perf report -i $OUTP --stdio --no-children --comm offload_daemon --sort dso 2>/dev/null | grep -E "^\s+[0-9]+\.[0-9]+%" | head -8

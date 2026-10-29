#!/bin/bash
# run_perf_cell.sh [4k|2m] -- one session (regex-chess_30b, 24 turns) with the
# DSA + IAA resident variants after every turn, under perf on the VMM cores
# (0-3) and the daemon cores (4,5): where the per-checkpoint time goes
# outside the daemon's own phases. cpu-clock sampling (hw cycles give no
# samples on this kernel); reports split by comm.
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
PAGES=${1:-4k}; SESSION=${SESSION:-regex-chess_30b}
PERF=${PERF:-/usr/lib/linux-tools-6.8.0-146/perf}
export VARIANTS="full-res diff-dsa-res" EVERY=1 OUT_ROOT=$HOME/chlogs/lifecycle-cad-perf-$PAGES
mkdir -p "$OUT_ROOT"
sudo "$PERF" record -e cpu-clock -F 4999 -g -C 0-5 -o "$OUT_ROOT/perf.data" -- sleep 100000 &
PPID_PERF=$!
sleep 1
bash "$HERE/lifecycle_cadence.sh" "$SESSION" "$PAGES"
sudo pkill -INT -x perf; wait $PPID_PERF 2>/dev/null
sudo chown "$USER" "$OUT_ROOT/perf.data"
for comm in cloud-hypervisor offload_daemon ch-remote; do
	echo "=== $comm (self, relative) ==="
	"$PERF" report -i "$OUT_ROOT/perf.data" --comm "$comm" --no-children --percentage relative --stdio -g none 2>/dev/null | grep -vE "^#|^$" | head -30
done
echo "=== all comms by share ==="
"$PERF" report -i "$OUT_ROOT/perf.data" --sort comm --no-children --stdio -g none 2>/dev/null | grep -vE "^#|^$" | head -15
echo PERF_CELL_DONE

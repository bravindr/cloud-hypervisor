#!/bin/bash
# run_chain_cell.sh -- the every-turn diff variants once more with the daemon
# whose restore applies a diff chain in one pass (target-iaa2: parallel
# decompress, newest-first scatter, each page written once). The ladder's
# every-turn cells (target-iaa, ordered apply) are the control. Runs after
# post_ladder.sh is done.
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
until grep -q POST_LADDER_DONE ~/chlogs/post_ladder.log 2>/dev/null; do sleep 30; done
SESSIONS=${SESSIONS:-"write-compressor_30b circuit-fibsqrt_7b circuit-fibsqrt_30b regex-chess_30b path-tracing_30b polyglot-rust-c_30b winning-avg-corewars_30b"}
export VARIANTS="diff-dsa-res diff-cpu-res-lz4" EVERY=1 OFFLOAD_BIN=$HOME/cloud-hypervisor/target-iaa2/release/offload_daemon
export OUT_ROOT=$HOME/chlogs/lifecycle-cad-chain; mkdir -p "$OUT_ROOT"
for pages in 4k 2m; do
	for s in $SESSIONS; do
		echo "######## $s $pages chain-apply start $(date +%T)"
		bash "$HERE/lifecycle_cadence.sh" "$s" "$pages"; echo "######## $s $pages rc=$? end $(date +%T)"
	done
done
python3 "$HERE/analyze_cadence.py" "$OUT_ROOT"/*/ | tee "$OUT_ROOT/summary-$(date +%Y%m%d_%H%M).txt"
grep -h "Applied" "$OUT_ROOT"/*/rdaemon_*.log | sed 's/.*Applied/Applied/' | sort | uniq -c | sort -rn | head -5
echo CHAIN_CELL_DONE

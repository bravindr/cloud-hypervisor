#!/bin/bash
# run_thp_cell.sh -- 4 KiB memfd guests with and without shmem transparent
# huge pages (host /sys/kernel/mm/transparent_hugepage/shmem_enabled), the
# DSA + IAA resident variants after every turn, restore --workers 8 (the e2e
# restore setting). Control first, then THP, then the knob is put back.
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SESSIONS=${SESSIONS:-"write-compressor_30b circuit-fibsqrt_7b circuit-fibsqrt_30b regex-chess_30b path-tracing_30b polyglot-rust-c_30b winning-avg-corewars_30b"}
KNOB=/sys/kernel/mm/transparent_hugepage/shmem_enabled
orig=$(sed -n 's/.*\[\(.*\)\].*/\1/p' $KNOB)
export VARIANTS="full-res diff-dsa-res" EVERY=1 RESTORE_WORKERS=8
for mode in never always; do
	echo "$mode" | sudo tee $KNOB >/dev/null
	export OUT_ROOT=$HOME/chlogs/lifecycle-cad-thp-$mode; mkdir -p "$OUT_ROOT"
	for s in $SESSIONS; do
		echo "######## $s 4k shmem_enabled=$mode start $(date +%T)"
		grep -E "^(ShmemHugePages|AnonHugePages)" /proc/meminfo | tr '\n' ' '; echo
		bash "$HERE/lifecycle_cadence.sh" "$s" 4k; echo "######## $s rc=$? end $(date +%T)"
	done
	python3 "$HERE/analyze_cadence.py" "$OUT_ROOT"/*/ | tee "$OUT_ROOT/summary-$(date +%Y%m%d_%H%M).txt"
done
echo "$orig" | sudo tee $KNOB >/dev/null
echo THP_CELL_DONE

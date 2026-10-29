#!/bin/bash
# run_all_cadence.sh [pages...] -- every recorded session through
# lifecycle_cadence.sh for each page size (default: 4k 2m) and each cadence
# in EVERIES (default: 1 2 5 10 0; 0 = only after the last turn), then the
# summary. Runs serialise on node 0 (one VM at a time).
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SESSIONS=${SESSIONS:-"write-compressor_30b circuit-fibsqrt_7b circuit-fibsqrt_30b regex-chess_30b path-tracing_30b polyglot-rust-c_30b winning-avg-corewars_30b"}
EVERIES=${EVERIES:-"1 2 5 10 0"}
OUT_ROOT=${OUT_ROOT:-$HOME/chlogs/lifecycle-cad}; export OUT_ROOT
mkdir -p "$OUT_ROOT"
for pages in ${@:-4k 2m}; do
	for every in $EVERIES; do
		for s in $SESSIONS; do
			echo "######## $s $pages every=$every start $(date +%T)"
			EVERY=$every bash "$HERE/lifecycle_cadence.sh" "$s" "$pages"; echo "######## $s $pages every=$every rc=$? end $(date +%T)"
		done
	done
done
python3 "$HERE/analyze_cadence.py" "$OUT_ROOT"/*/ | tee "$OUT_ROOT/summary-$(date +%Y%m%d_%H%M).txt"
echo LIFECYCLE_CAD_ALL_DONE

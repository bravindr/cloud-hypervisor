#!/bin/bash
# run_all.sh [pages...] -- every recorded session through lifecycle.sh, for
# each page size (default: 4k 2m), then the summary.
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SESSIONS=${SESSIONS:-"write-compressor_30b circuit-fibsqrt_7b circuit-fibsqrt_30b regex-chess_30b path-tracing_30b polyglot-rust-c_30b winning-avg-corewars_30b"}
OUT_ROOT=${OUT_ROOT:-$HOME/chlogs/lifecycle}; export OUT_ROOT
for pages in ${@:-4k 2m}; do
	for s in $SESSIONS; do
		echo "######## $s $pages start $(date +%T)"
		bash "$HERE/lifecycle.sh" "$s" "$pages"; echo "######## $s $pages rc=$? end $(date +%T)"
	done
done
python3 "$HERE/analyze_lifecycle.py" "$OUT_ROOT"/*/ | tee "$OUT_ROOT/summary-$(date +%Y%m%d_%H%M).txt"
echo LIFECYCLE_ALL_DONE

#!/usr/bin/env python3
# analyze.py <results-dir>... : fair CPU-vs-DSA snapshot tables from the harness
import csv, glob, os, re, statistics, sys, collections

def runs(d):
    rows = [r for r in csv.DictReader(open(os.path.join(d, "results.csv"))) if r["phase"] == "snapshot"]
    g = collections.defaultdict(list)
    for r in rows:
        g[(r["dataset"], r["codec"])].append(r)
    return g

def logstats(d, label):
    # aggregate classify lines over the measured iterations
    sub = fb = fail = scans = 0; pf = []; zero = None
    for f in glob.glob(os.path.join(d, "logs", f"snapshot-{label}-c*-i[0-9]*-daemon.log")):
        if "warmup" in f:
            continue
        for line in open(f):
            m = re.search(r"classify=\w+ prefault=([\d.]+)ms zero_chunks=(\d+)/(\d+) dsa submitted=(\d+) fallback=(\d+) failed=(\d+) cpu_scans=(\d+)", line)
            if m:
                pf.append(float(m.group(1))); sub += int(m.group(4)); fb += int(m.group(5)); fail += int(m.group(6)); scans += int(m.group(7))
    return sub, fb, fail, scans, (statistics.median(pf) if pf else None)

order = ["raw", "qpl-hardware-static-async", "qpl-hardware-static-async+cpu", "qpl-hardware-static-async+dsa",
         "qpl-hardware-static-async+cpucrc", "qpl-hardware-static-async+dsacrc",
         "qpl-hardware-dynamic-async", "qpl-hardware-dynamic-async+cpu", "qpl-hardware-dynamic-async+dsa",
         "qpl-hardware-dynamic-async+cpucrc", "qpl-hardware-dynamic-async+dsacrc"]
for d in sys.argv[1:]:
    g = runs(d)
    for (ds, codec) in sorted(g, key=lambda k: (k[0], order.index(k[1]) if k[1] in order else 99)):
        v = g[(ds, codec)]
        ms = [float(r["elapsed_ms"]) for r in v]; cpu = [float(r["cpu_util_pct"]) for r in v]
        cores = [m / 1000 * c / 100 for m, c in zip(ms, cpu)]
        mib = statistics.median(int(r["stored_bytes"]) for r in v) / 2**20
        sub, fb, fail, scans, pf = logstats(d, codec)
        note = "" if "+" not in codec else f"dsa sub={sub} fb={fb} fail={fail} cpu_scans={scans} prefault~{pf:.0f}ms" if pf is not None else "no classify lines"
        print(f"{ds:11} {codec:34} n={len(v)} wall med {statistics.median(ms):7.0f} ms (min {min(ms):5.0f} max {max(ms):5.0f})  "
              f"cpu {statistics.median(cores):5.2f} core-s  out {mib:6.0f} MiB  {note}")
    print()

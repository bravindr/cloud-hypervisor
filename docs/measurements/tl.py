import glob, re, statistics, sys, collections
d = sys.argv[1]
def ts(line):
    m = re.match(r"\[\d{4}-\d\d-\d\dT(\d\d):(\d\d):(\d\d)\.(\d{6})Z", line)
    return int(m[1])*3600 + int(m[2])*60 + int(m[3]) + int(m[4])/1e6 if m else None
rows = collections.defaultdict(list)
for f in glob.glob(d + "/logs/snapshot-*-w8-i*-daemon.log"):
    m = re.search(r"snapshot-(.*)-c\d+-w8-i(\d+)-daemon", f)
    if not m or m[1].endswith("raw"): continue
    lines = open(f).read().splitlines()
    t = {k: None for k in ("connected", "first_classify", "persisted")}
    prepare = compress = sync = None
    for l in lines:
        if "CH connected" in l: t["connected"] = ts(l)
        if "classify=" in l and t["first_classify"] is None: t["first_classify"] = ts(l)
        if "Snapshot persisted" in l: t["persisted"] = ts(l)
        mm = re.search(r"prepare ([\d.]+) ms", l)
        if mm: prepare = float(mm[1])
        mm = re.search(r"compress_ms=([\d.]+) sync_ms=([\d.]+)", l)
        if mm: compress, sync = float(mm[1]), float(mm[2])
    if t["connected"] and t["persisted"]:
        rows[m[1]].append(((t["persisted"] - t["connected"]) * 1e3, prepare, compress, sync, (t["first_classify"] - t["connected"]) * 1e3))
print(f"{'variant':40s} {'connected->persisted':>20s} {'prepare':>8s} {'compress':>9s} {'sync':>6s} {'connected->classify log':>24s}")
for k in sorted(rows):
    v = rows[k]
    med = lambda i: statistics.median(x[i] for x in v if x[i] is not None) if any(x[i] is not None for x in v) else float('nan')
    print(f"{k:40s} {med(0):20.1f} {med(1):8.1f} {med(2):9.1f} {med(3):6.1f} {med(4):24.1f}")

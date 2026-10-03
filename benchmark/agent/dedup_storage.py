import csv, glob, collections, sys
rows = []
for f in glob.glob(sys.argv[1] + "/*/checkpoints.csv"):
    rows += [r for r in csv.DictReader(open(f)) if r["rc"] == "0"]
by = collections.defaultdict(lambda: collections.defaultdict(float))
for r in rows:
    if r["checkpoint"] == "base":
        continue
    d = by[(r["pages"], r["session"])]
    d[r["variant"] + ":bytes"] += float(r["out_bytes"])
    d[r["variant"] + ":dedup"] += float(r["dedup_pages"] or 0)
print("%-30s %9s %9s %6s %13s" % ("pages session", "diffs MiB", "w/ dedup", "saved", "dedup MiB raw"))
for (pg, s), d in sorted(by.items()):
    a = d["diff-dsa-res:bytes"] / 2**20; b = d["diff-dsa-res-dedup:bytes"] / 2**20; n = d["diff-dsa-res-dedup:dedup"]
    if a and b:
        print("%-30s %9.1f %9.1f %5.0f%% %13.1f" % (pg + " " + s, a, b, 100 * (1 - b / a), n * 4096 / 2**20))

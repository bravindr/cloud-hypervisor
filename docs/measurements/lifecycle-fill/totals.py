import csv, glob, collections, sys, statistics
for root in sys.argv[1:]:
    ck = []; rs = []; st = []
    for f in glob.glob(root + "/*/checkpoints.csv"):
        ck += [r for r in csv.DictReader(open(f)) if r["rc"] == "0"]
    for f in glob.glob(root + "/*/restores.csv"):
        rs += [r for r in csv.DictReader(open(f)) if r["rc"] == "0"]
    for f in glob.glob(root + "/*/steps.csv"):
        st += [r for r in csv.DictReader(open(f)) if r["rc"] == "0"]
    print("#####", root)
    for pg in ("4k", "2m"):
        steps = sum(float(r["guest_secs"]) for r in st if r["pages"] == pg)
        nsess = len({r["session"] for r in st if r["pages"] == pg})
        print("== %s: sessions=%d guest steps total %.1f s (%d steps)" % (pg, nsess, steps, len([r for r in st if r["pages"] == pg])))
        vars_ = sorted({r["variant"] for r in ck if r["pages"] == pg}, key=lambda v: ["base","cpu","dsa","cpucrc","dsacrc"].index(v) if v in ["base","cpu","dsa","cpucrc","dsacrc"] else 9)
        print("%-8s %5s %10s %10s %10s | %12s %12s" % ("variant", "n", "ckpt wall s", "ckpt cpu s", "out GiB", "restore ms", "restore cpu"))
        for v in vars_:
            rows = [r for r in ck if r["pages"] == pg and r["variant"] == v]
            wall = sum(float(r["wall_ms"]) for r in rows) / 1000
            cpu = sum(float(r["user_s"]) + float(r["sys_s"]) for r in rows)
            out = sum(float(r["out_bytes"]) for r in rows) / 2**30
            rr = [r for r in rs if r["pages"] == pg and r["variant"] == v]
            rms = statistics.median(float(r["restore_ms"]) for r in rr) if rr else 0
            rcpu = statistics.median(float(r["user_s"]) + float(r["sys_s"]) for r in rr) if rr else 0
            print("%-8s %5d %10.1f %10.1f %10.2f | %12.0f %12.2f" % (v, len(rows), wall, cpu, out, rms, rcpu))

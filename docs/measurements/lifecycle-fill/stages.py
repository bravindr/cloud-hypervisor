#!/usr/bin/env python3
"""Per-stage split of the last-turn full checkpoint and of restores, per variant and page size.
usage: stages.py OUT_ROOT [OUT_ROOT...]   (lifecycle result roots; uses daemon_step*_<v>.log, checkpoints.csv, rdaemon_*_<v>.log, restores.csv)"""
import sys, glob, os, re, csv, statistics as st
from datetime import datetime
def ts(line):
    m = re.match(r"\[(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z", line); return datetime.fromisoformat(m.group(1)).timestamp() if m else None
def med(xs): return st.median(xs) if xs else float("nan")
for root in sys.argv[1:]:
    print(f"##### {root}")
    for pages in ("4k", "2m"):
        rows = {}
        for d in glob.glob(f"{root}/*-{pages}-*/"):
            # last-turn checkpoint: daemon_step<N>_<v>.log where N = max step
            steps = sorted({int(m.group(1)) for f in os.listdir(d) for m in [re.match(r"daemon_step(\d+)_", f)] if m})
            if not steps: continue
            last = steps[-1]
            walls = {}
            for r in csv.reader(open(d + "checkpoints.csv")):
                if len(r) > 6 and r[2] == f"step{last}" and r[6].isdigit():
                    walls[r[4]] = (int(r[6]), float(r[7]) + float(r[8]), int(r[9]))
            for f in glob.glob(f"{d}daemon_step{last}_*.log"):
                v = re.search(r"_([a-z0-9]+)\.log$", f).group(1)
                L = open(f).read().splitlines()
                t = {k: None for k in ("start", "conn", "done", "persist")}
                prep = comp = sync = prefault = None
                for line in L:
                    if "listening" in line and t["start"] is None: t["start"] = ts(line)
                    if "CH connected" in line: t["conn"] = ts(line)
                    if "shared pipeline:" in line:
                        t["done"] = ts(line)
                        m = re.search(r"prepare ([\d.]+) ms.*compress ([\d.]+) ms", line); prep, comp = float(m.group(1)), float(m.group(2))
                    if "sync_ms=" in line and sync is None:
                        sync = float(re.search(r"sync_ms=([\d.]+)", line).group(1))
                    if "Snapshot persisted" in line: t["persist"] = ts(line)
                pf = sum(float(x) for x in re.findall(r"prefault=([\d.]+)ms", "\n".join(L)))
                if t["done"] is None or v not in walls: continue
                wall, cpu, out = walls[v]
                r = rows.setdefault(v, {k: [] for k in ("wall", "cpu", "out", "startup", "prepare", "prefault", "compress", "sync", "persist", "vmm")})
                r["wall"].append(wall); r["cpu"].append(cpu); r["out"].append(out / 2**20)
                r["startup"].append((t["conn"] - t["start"]) * 1e3)
                r["prepare"].append(prep); r["prefault"].append(pf); r["compress"].append(comp); r["sync"].append(sync or 0)
                r["persist"].append((t["persist"] - t["done"]) * 1e3 if t["persist"] else 0)
                r["vmm"].append(wall - (t["persist"] or t["done"] - t["conn"]) * 1e3 if False else wall - ((t["persist"] or t["done"]) - t["conn"]) * 1e3)
            # restores
            rw = {}
            for r in csv.reader(open(d + "restores.csv")):
                if len(r) > 5 and r[4].isdigit(): rw[(r[2], r[3])] = (int(r[4]), float(r[5]) + float(r[6]))
            for f in glob.glob(f"{d}rdaemon_*_*.log"):
                m = re.search(r"rdaemon_([a-z0-9]+)_([a-z0-9]+)\.log$", f); cp, v = m.group(1), m.group(2)
                if (cp, v) not in rw: continue
                L = open(f).read().splitlines(); t0 = t1 = None
                for line in L:
                    if "connected" in line and t0 is None: t0 = ts(line)
                    if "replay finished" in line: t1 = ts(line)
                if t0 and t1:
                    r = rows.setdefault(v, {}); r.setdefault("r_wall", []).append(rw[(cp, v)][0]); r.setdefault("r_cpu", []).append(rw[(cp, v)][1]); r.setdefault("r_daemon", []).append((t1 - t0) * 1e3)
        print(f"=== {pages}: last-turn full checkpoint, medians over sessions (ms; cpu core-s; out MiB)")
        print(f"{'variant':8s} {'wall':>6s} {'cpu':>5s} {'out':>5s} | {'start':>6s} {'prepare':>7s} {'(pf)':>5s} {'compress':>8s} {'fsync':>5s} {'persist':>7s} {'vmm+proto':>9s}")
        for v, r in sorted(rows.items()):
            if "wall" not in r: continue
            print(f"{v:8s} {med(r['wall']):6.0f} {med(r['cpu']):5.2f} {med(r['out']):5.0f} | {med(r['startup']):6.1f} {med(r['prepare']):7.1f} {med(r['prefault']):5.1f} {med(r['compress']):8.1f} {med(r['sync']):5.1f} {med(r['persist']):7.1f} {med(r['vmm']):9.1f}")
        print(f"=== {pages}: restores (ms; harness wall, daemon span connect->replay, daemon cpu core-s)")
        for v, r in sorted(rows.items()):
            if "r_wall" in r: print(f"{v:8s} wall {med(r['r_wall']):6.0f}  daemon {med(r['r_daemon']):6.0f}  cpu {med(r['r_cpu']):5.2f}  n={len(r['r_wall'])}")

#!/usr/bin/env python3
"""analyze_cadence.py <run-dir>... -- checkpoint cadence ladder (lifecycle_cadence.sh).

Per page size, cadence (EVERY) and variant: checkpoints taken, per-checkpoint
pause and CPU (median, p95), bytes per checkpoint, dirty/changed pages per
diff, per-session totals (pause s, CPU s, MiB stored) and the final chain
restore. Sessions with a failed checkpoint are reported but kept.
"""
import csv, os, statistics, sys
from collections import defaultdict

ORDER = ["full-res", "diff-dsa-res", "full-res-lz4", "diff-cpu-res-lz4",
         "full-res-zstd", "diff-cpu-res-zstd", "diff-dsa-res-dedup"]

def load(dirs, name):
    rows = []
    for d in dirs:
        p = os.path.join(d, name)
        if os.path.exists(p):
            rows.extend(csv.DictReader(open(p)))
    return rows

def f(r, k):
    v = r.get(k)
    return float(v) if v not in (None, "") else None

def med(v):
    v = [x for x in v if x is not None]
    return statistics.median(v) if v else None

def pct(v, p):
    v = sorted(x for x in v if x is not None)
    return v[min(len(v) - 1, int(round(p * (len(v) - 1))))] if v else None

def fmt(x, spec):
    return format(x, spec) if x is not None else " " * len(format(0, spec))

def every_key(e):
    e = int(e)
    return 10**6 if e == 0 else e

def main(dirs):
    ck = load(dirs, "checkpoints.csv"); rs = load(dirs, "restores.csv")
    bad = [r for r in ck if r["rc"] != "0"]
    print(f"checkpoints {len(ck)} ({len(bad)} failed), restores {len(rs)} "
          f"({sum(1 for r in rs if r['guest_ok'] != '1')} guest not ok)")
    for r in bad[:10]:
        print("  FAILED", r["session"], r["pages"], "every", r["every"], r["checkpoint"], r["variant"], "rc", r["rc"])
    ok = [r for r in ck if r["rc"] == "0"]
    variants = [v for v in ORDER if any(r["variant"] == v for r in ok)]
    variants += sorted({r["variant"] for r in ok} - set(variants))
    for pages in sorted({r["pages"] for r in ok}):
        for every in sorted({r["every"] for r in ok if r["pages"] == pages}, key=every_key):
            rows = [r for r in ok if r["pages"] == pages and r["every"] == every]
            turn = [r for r in rows if r["checkpoint"] != "base"]
            sessions = sorted({r["session"] for r in rows})
            nturn = len({(r["session"], r["turn"]) for r in turn})
            label = "after every turn" if every == "1" else ("only after the last turn" if every == "0" else f"every {every} turns")
            print(f"\n=== {pages}, {label}: {len(sessions)} sessions, {nturn} checkpointed turns ===")
            print(f"{'variant':18} {'n':>4} {'pause ms':>8} {'p95':>6} {'cpu ms':>7} {'p95':>6} {'MiB/ckpt':>8} {'dirty pg':>8} {'chg pg':>7} "
                  f"{'cmp ms':>6} {'gath ms':>7} {'comp ms':>7} | {'per session: pause s':>20} {'cpu s':>6} {'MiB':>7} | {'restore ms':>10} {'chain':>5} {'ok':>3}")
            for v in variants:
                t = [r for r in turn if r["variant"] == v]
                if not t:
                    continue
                wall = [f(r, "wall_ms") for r in t]; cpu = [f(r, "cpu_s") * 1000 for r in t if f(r, "cpu_s") is not None]
                out = [f(r, "out_bytes") / 2**20 for r in t]
                per_sess_pause = [sum(f(r, "wall_ms") for r in t if r["session"] == s) / 1000 for s in sessions]
                per_sess_cpu = [sum(f(r, "cpu_s") or 0 for r in t if r["session"] == s) for s in sessions]
                per_sess_mib = [sum(f(r, "out_bytes") for r in rows if r["session"] == s and r["variant"] == v and r["checkpoint"] != "base") / 2**20 for s in sessions]
                if not v.startswith("diff"):
                    # a full checkpoint's storage is its last image, not the sum
                    per_sess_mib = [med([f(r, "out_bytes") for r in t if r["session"] == s]) / 2**20 for s in sessions if any(r["session"] == s for r in t)]
                rr = [r for r in rs if r["pages"] == pages and r["every"] == every and r["variant"] == v]
                print(f"{v:18} {len(t):4d} {fmt(med(wall), '8.1f')} {fmt(pct(wall, .95), '6.0f')} {fmt(med(cpu), '7.1f')} {fmt(pct(cpu, .95), '6.0f')} "
                      f"{fmt(med(out), '8.2f')} {fmt(med([f(r, 'dirty_pages') for r in t]), '8.0f')} {fmt(med([f(r, 'changed_pages') for r in t]), '7.0f')} "
                      f"{fmt(med([f(r, 'compare_ms') for r in t]), '6.1f')} {fmt(med([f(r, 'gather_ms') for r in t]), '7.1f')} {fmt(med([f(r, 'compress_ms') for r in t]), '7.1f')} | "
                      f"{fmt(sum(per_sess_pause), '20.2f')} {fmt(sum(per_sess_cpu), '6.2f')} {fmt(sum(per_sess_mib), '7.0f')} | "
                      f"{fmt(med([f(r, 'restore_ms') for r in rr]), '10.0f')} {fmt(med([f(r, 'chain') for r in rr]), '5.0f')} {sum(1 for r in rr if r['guest_ok'] == '1'):2d}/{len(rr)}")
    # per-session detail for the diff variants: pause and bytes vs cadence
    print("\n=== per session, diff-dsa-res: checkpoints / sum pause ms / sum MiB / final chain restore ms ===")
    sess = sorted({r["session"] for r in ok})
    for pages in sorted({r["pages"] for r in ok}):
        everies = sorted({r["every"] for r in ok if r["pages"] == pages}, key=every_key)
        print(f"-- {pages}: " + "  ".join(f"every={e}" for e in everies))
        for s in sess:
            cells = []
            for e in everies:
                t = [r for r in ok if r["pages"] == pages and r["every"] == e and r["session"] == s and r["variant"] == "diff-dsa-res" and r["checkpoint"] != "base"]
                rr = [r for r in rs if r["pages"] == pages and r["every"] == e and r["session"] == s and r["variant"] == "diff-dsa-res"]
                if t:
                    cells.append(f"{len(t):3d} / {sum(f(r, 'wall_ms') for r in t):6.0f} / {sum(f(r, 'out_bytes') for r in t) / 2**20:5.1f} / {fmt(med([f(r, 'restore_ms') for r in rr]), '4.0f')}")
                else:
                    cells.append(" " * 28)
            print(f"{s:28} " + "  ".join(cells))

if __name__ == "__main__":
    main(sys.argv[1:])

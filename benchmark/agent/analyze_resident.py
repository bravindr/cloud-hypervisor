#!/usr/bin/env python3
"""analyze_resident.py <run-dir>... -- per-process vs resident daemons."""
import csv, os, statistics, sys

ORDER = ["full-proc", "full-res", "diff-cpu-proc", "diff-dsa-proc", "diff-cpu-res", "diff-dsa-res", "diff-cpu-res-dedup", "diff-dsa-res-dedup"]

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

def med(rows, k):
    v = [f(r, k) for r in rows if f(r, k) is not None]
    return statistics.median(v) if v else None

def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(round(p * (len(v) - 1))))] if v else float("nan")

def fmt(x, spec):
    return format(x, spec) if x is not None else " " * len(format(0, spec))

def main(dirs):
    ck = load(dirs, "checkpoints.csv"); rs = load(dirs, "restores.csv")
    ok = [r for r in ck if r["rc"] == "0"]
    print(f"checkpoints {len(ck)} ({len(ck) - len(ok)} failed), restores {len(rs)}")
    for pages in sorted({r["pages"] for r in ok}):
        turn = [r for r in ok if r["pages"] == pages and r["checkpoint"] != "base"]
        nturns = len({(r["session"], r["turn"]) for r in turn})
        print(f"\n=== {pages}: per-turn checkpoint, median over {nturns} turns ===")
        print(f"{'variant':18} {'wall ms':>7} {'p95':>5} {'cpu ms':>7} {'p95':>6} {'KiB out':>8} {'prep ms':>7} {'cmp ms':>6} {'gath ms':>7} {'comp ms':>7} {'populate':>8} {'dedup pg':>8} {'dedup ms':>8}")
        for v in ORDER:
            t = [r for r in turn if r["variant"] == v]
            if not t:
                continue
            wall = [f(r, "wall_ms") for r in t]; cpu = [f(r, "cpu_s") * 1e3 for r in t]
            print(f"{v:18} {statistics.median(wall):7.0f} {pct(wall, .95):5.0f} {statistics.median(cpu):7.1f} {pct(cpu, .95):6.1f} "
                  f"{med(t, 'out_bytes') / 1024:8.0f} {fmt(med(t, 'prepare_ms'), '7.1f')} {fmt(med(t, 'compare_ms'), '6.1f')} "
                  f"{fmt(med(t, 'gather_ms'), '7.1f')} {fmt(med(t, 'compress_ms'), '7.1f')} {fmt(med(t, 'populate_calls'), '8.0f')} "
                  f"{fmt(med(t, 'dedup_pages'), '8.0f')} {fmt(med(t, 'dedup_ms'), '8.1f')}")
        print(f"\n=== {pages}: all checkpoints of all sessions ===")
        print(f"{'variant':18} {'wall s':>7} {'cpu s':>7} {'stored MiB':>10} {'dedup pages':>11}")
        for v in ORDER:
            t = [r for r in ok if r["pages"] == pages and r["variant"] == v]
            if t:
                print(f"{v:18} {sum(f(r,'wall_ms') for r in t) / 1e3:7.2f} {sum(f(r,'cpu_s') for r in t):7.2f} {sum(f(r,'out_bytes') for r in t) / 2**20:10.0f} {sum(f(r,'dedup_pages') or 0 for r in t):11.0f}")
        base = [r for r in ok if r["pages"] == pages and r["checkpoint"] == "base"]
        print(f"\n  base (first, full) checkpoint median wall: " + ", ".join(f"{v} {med([r for r in base if r['variant'] == v], 'wall_ms'):.0f} ms" for v in ORDER if any(r['variant'] == v for r in base)))
        print(f"\n=== {pages}: restores ===")
        for v in ORDER:
            t = [r for r in rs if r["pages"] == pages and r["variant"] == v]
            if t:
                print(f"  {v:18} median {statistics.median(f(r,'restore_ms') for r in t):5.0f} ms, chain up to {max(int(f(r,'chain')) for r in t)} diffs, guest ok {sum(r['guest_ok'] == '1' for r in t)}/{len(t)}")

if __name__ == "__main__":
    main(sys.argv[1:])

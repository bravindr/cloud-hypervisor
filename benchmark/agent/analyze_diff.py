#!/usr/bin/env python3
"""analyze_diff.py <run-dir>... -- summarize lifecycle_diff.sh runs."""
import csv, collections, os, statistics, sys

ORDER = ["full-dsa", "diff-none", "diff-cpu", "diff-dsa", "diff-dsa1"]

def load(dirs, name):
    rows = []
    for d in dirs:
        p = os.path.join(d, name)
        if os.path.exists(p):
            rows.extend(csv.DictReader(open(p)))
    return rows

def f(r, k):
    return float(r[k]) if r.get(k) not in (None, "") else 0.0

def pct(v, p):
    v = sorted(v); return v[min(len(v) - 1, int(round(p * (len(v) - 1))))] if v else float("nan")

def main(dirs):
    ck = load(dirs, "checkpoints.csv"); rs = load(dirs, "restores.csv")
    ok = [r for r in ck if r["rc"] == "0"]
    print(f"checkpoints {len(ck)} ({len(ck) - len(ok)} failed), restores {len(rs)}")
    for pages in sorted({r["pages"] for r in ok}):
        turn = [r for r in ok if r["pages"] == pages and r["checkpoint"] != "base"]
        print(f"\n=== {pages}: per-turn checkpoint (median over {len({(r['session'], r['turn']) for r in turn})} turns) ===")
        print(f"{'variant':10} {'wall ms':>8} {'p95':>6} {'cpu-s':>6} {'KiB out':>8} {'dirty pg':>8} {'stored pg':>9} {'compare ms':>10} {'gather ms':>9} {'compress ms':>11} {'prep ms':>7} {'redo':>5}")
        for v in ORDER:
            t = [r for r in turn if r["variant"] == v]
            if not t:
                continue
            med = lambda k: statistics.median(f(r, k) for r in t)
            wall = [f(r, "wall_ms") for r in t]
            print(f"{v:10} {statistics.median(wall):8.0f} {pct(wall, .95):6.0f} {statistics.median(f(r,'user_s') + f(r,'sys_s') for r in t):6.2f} "
                  f"{med('out_bytes') / 1024:8.0f} {med('dirty_pages'):8.0f} {med('changed_pages'):9.0f} {med('compare_ms'):10.1f} "
                  f"{med('gather_ms'):9.1f} {med('compress_ms'):11.1f} {med('prepare_ms'):7.1f} {sum(f(r,'dsa_redo') for r in t):5.0f}")
        print(f"\n=== {pages}: whole sessions (sum over all checkpoints incl. base) ===")
        print(f"{'variant':10} {'wall s':>8} {'cpu-s':>7} {'stored MiB':>10}")
        for v in ORDER:
            t = [r for r in ok if r["pages"] == pages and r["variant"] == v]
            if not t:
                continue
            print(f"{v:10} {sum(f(r,'wall_ms') for r in t) / 1e3:8.1f} {sum(f(r,'user_s') + f(r,'sys_s') for r in t):7.1f} "
                  f"{sum(f(r,'out_bytes') for r in t) / 2**20:10.0f}")
        dsa = [r for r in turn if r["variant"] in ("diff-cpu", "diff-dsa")]
        pairs = collections.defaultdict(dict)
        for r in dsa:
            pairs[(r["session"], r["turn"])][r["variant"]] = r
        same = sum(1 for p in pairs.values() if len(p) == 2 and p["diff-cpu"]["changed_pages"] == p["diff-dsa"]["changed_pages"])
        print(f"\n  diff-cpu and diff-dsa agree on changed pages in {same}/{len(pairs)} turns")
        print(f"\n=== {pages}: restores ===")
        for v in ORDER:
            t = [r for r in rs if r["pages"] == pages and r["variant"] == v]
            if t:
                print(f"  {v:10} restore median {statistics.median(f(r,'restore_ms') for r in t):6.0f} ms, chain up to {max(int(f(r,'chain')) for r in t)} diffs, guest ok {sum(r['guest_ok'] == '1' for r in t)}/{len(t)}")

if __name__ == "__main__":
    main(sys.argv[1:])

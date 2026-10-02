#!/usr/bin/env python3
"""analyze_lifecycle.py <run-dir>... -- summarize lifecycle.sh runs.

Per (pages, variant): base checkpoint wall, per-turn checkpoint wall and daemon
CPU (median / p95 over every turn of every session), total daemon CPU per
session set, compressed size, DSA fallbacks; then per-session medians and the
restore table."""
import csv, collections, os, statistics, sys

ORDER = ["base", "cpu", "dsa", "cpucrc", "dsacrc"]

def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(round(p * (len(v) - 1))))]

def load(dirs, name):
    rows = []
    for d in dirs:
        p = os.path.join(d, name)
        if os.path.exists(p):
            rows.extend(csv.DictReader(open(p)))
    return rows

def cpu(r):
    return float(r["user_s"] or 0) + float(r["sys_s"] or 0)

def main(dirs):
    ck = load(dirs, "checkpoints.csv"); rs = load(dirs, "restores.csv"); st = load(dirs, "steps.csv")
    ok = [r for r in ck if r["rc"] == "0"]
    print(f"checkpoints: {len(ck)} ({len(ck) - len(ok)} failed); restores: {len(rs)}; guest turns: {len(st)}")
    for pages in sorted({r["pages"] for r in ok}):
        print(f"\n=== {pages} pages: checkpoint cost per variant (all sessions) ===")
        print(f"{'variant':8} {'base ms':>8} {'turn ms med':>11} {'p95':>6} {'turn cpu-s med':>14} {'p95':>6} {'total cpu-s':>11} {'MiB/turn med':>12} {'dsa fb':>6}")
        for v in ORDER:
            rows = [r for r in ok if r["pages"] == pages and r["variant"] == v]
            if not rows:
                continue
            base = [int(r["wall_ms"]) for r in rows if r["checkpoint"] == "base"]
            turn = [r for r in rows if r["checkpoint"] != "base"]
            wall = [int(r["wall_ms"]) for r in turn]
            c = [cpu(r) for r in turn]
            mib = [int(r["out_bytes"]) / 2**20 for r in turn]
            fb = sum(int(r["dsa_fallback"] or 0) for r in rows)
            print(f"{v:8} {statistics.median(base):8.0f} {statistics.median(wall):11.0f} {pct(wall, .95):6.0f} "
                  f"{statistics.median(c):14.2f} {pct(c, .95):6.2f} {sum(cpu(r) for r in rows):11.1f} "
                  f"{statistics.median(mib):12.0f} {fb:6d}")
        print(f"\n=== {pages} pages: per session  (turn checkpoint median ms / session daemon cpu-s) ===")
        print(f"{'session':28} {'turns':>5} " + " ".join(f"{v:>15}" for v in ORDER))
        for s in sorted({r["session"] for r in ok if r["pages"] == pages}):
            cells = []
            for v in ORDER:
                t = [r for r in ok if r["pages"] == pages and r["session"] == s and r["variant"] == v]
                tt = [int(r["wall_ms"]) for r in t if r["checkpoint"] != "base"]
                cells.append(f"{statistics.median(tt):7.0f}/{sum(cpu(r) for r in t):6.1f}s" if tt else f"{'-':>15}")
            n = len({r["turn"] for r in ok if r["pages"] == pages and r["session"] == s and r["checkpoint"] != "base"})
            print(f"{s:28} {n:5d} " + " ".join(cells))
        print(f"\n=== {pages} pages: restores (median over sessions) ===")
        for cp in ("last", "base"):
            for v in ORDER:
                t = [r for r in rs if r["pages"] == pages and r["checkpoint"] == cp and r["variant"] == v]
                if not t:
                    continue
                good = [r for r in t if r["rc"] == "0"]
                ms = [int(r["restore_ms"]) for r in good]
                print(f"  {cp:4} {v:7} restore {statistics.median(ms) if ms else float('nan'):7.0f} ms  "
                      f"daemon cpu {statistics.median(cpu(r) for r in good) if good else float('nan'):5.2f} s  "
                      f"guest progressed {sum(r['guest_ok'] == '1' for r in t)}/{len(t)}")

if __name__ == "__main__":
    main(sys.argv[1:])

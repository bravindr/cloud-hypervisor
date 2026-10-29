#!/usr/bin/env python3
"""analyze_realism.py <cad-run-dir>... -- checkpoint cost against the real agent timeline.

The recorded fixtures carry each turn's LLM latency (local vLLM on this box)
and AgentSysPerf's spans carry the tool-call time the agent's Harbor
container needed; a Terminal-Bench 2 task by a frontier agent takes 11 min 1 s
(Claude Opus 4.7 with Terminus 2, vals.ai Terminal-Bench 2.0 leaderboard,
450k tokens per task). For every cadence and variant of the ladder this
prints the VM pause and the daemon CPU per session as a fraction of those
timelines.
"""
import csv, glob, json, os, sqlite3, statistics, sys
from collections import defaultdict

FIX = "/mnt/nvme/fixtures/tb2"
DB = "/mnt/nvme/agentsysperf_home/results.db"
FRONTIER_S_PER_SESSION = 11 * 60 + 1
TASK_OF = lambda s: s.rsplit("_", 1)[0]

def fixture_timeline():
    out = {}
    for f in glob.glob(os.path.join(FIX, "*_*.jsonl")):
        name = os.path.basename(f)[:-6]
        rows = sorted((json.loads(l) for l in open(f)), key=lambda r: r["turn"])
        out[name] = {"turns": len(rows), "llm_s": sum(r["latency_ms"] for r in rows) / 1e3,
                     "llm_med_s": statistics.median(r["latency_ms"] for r in rows) / 1e3,
                     "span_s": rows[-1]["recorded_at"] - rows[0]["recorded_at"]}
    return out

def tool_times():
    c = sqlite3.connect(DB)
    out = {}
    for task, n, tot in c.execute("select task_id, count(*), sum(duration_us)/1e6 from spans where span_kind='tool_call' group by task_id"):
        out[task.split("/")[-1]] = (n, tot)
    return out

def main(dirs):
    tl = fixture_timeline(); tt = tool_times()
    ck = []
    for d in dirs:
        p = os.path.join(d, "checkpoints.csv")
        if os.path.exists(p):
            ck.extend(csv.DictReader(open(p)))
    ck = [r for r in ck if r["rc"] == "0" and r["checkpoint"] != "base"]
    sessions = sorted({r["session"] for r in ck})
    print("session timelines (recorded on this box with the local vLLM; tool time from the agent's Harbor container):")
    print(f"{'session':28} {'turns':>5} {'LLM s':>7} {'med/turn':>8} {'tool s':>6} {'wall s':>6} {'s/turn':>6} | frontier reference {FRONTIER_S_PER_SESSION} s per task")
    tot_turns = tot_llm = tot_tool = tot_span = 0
    for s in sessions:
        t = tl.get(s); n, tool = tt.get(TASK_OF(s), (0, 0.0))
        if not t:
            continue
        print(f"{s:28} {t['turns']:5d} {t['llm_s']:7.1f} {t['llm_med_s']:8.2f} {tool:6.1f} {t['span_s']:6.0f} {t['span_s'] / t['turns']:6.2f}")
        tot_turns += t["turns"]; tot_llm += t["llm_s"]; tot_tool += tool; tot_span += t["span_s"]
    print(f"{'all':28} {tot_turns:5d} {tot_llm:7.1f} {'':8} {tot_tool:6.1f} {tot_span:6.0f} {tot_span / max(tot_turns, 1):6.2f}")
    rec_total = tot_span
    frontier_total = len(sessions) * FRONTIER_S_PER_SESSION
    print(f"\nrecorded timeline {rec_total:.0f} s for {tot_turns} turns ({rec_total / max(tot_turns, 1):.1f} s per turn); "
          f"frontier timeline {frontier_total:.0f} s = {len(sessions)} tasks x {FRONTIER_S_PER_SESSION} s")
    for pages in sorted({r["pages"] for r in ck}):
        print(f"\n=== {pages}: checkpoint cost per {len(sessions)} sessions vs the agent's own time ===")
        print(f"{'cadence':10} {'variant':18} {'ckpts':>5} {'pause s':>7} {'cpu s':>6} | {'pause % recorded':>16} {'% frontier':>10} | {'cpu % recorded':>14} {'% frontier':>10} | {'max pause ms':>12}")
        everies = sorted({r["every"] for r in ck if r["pages"] == pages}, key=lambda e: 10**6 if int(e) == 0 else int(e))
        for e in everies:
            label = "every turn" if e == "1" else ("last only" if e == "0" else f"every {e}")
            for v in ["full-res", "diff-dsa-res", "full-res-lz4", "diff-cpu-res-lz4"]:
                t = [r for r in ck if r["pages"] == pages and r["every"] == e and r["variant"] == v]
                if not t:
                    continue
                pause = sum(float(r["wall_ms"]) for r in t) / 1e3; cpu = sum(float(r["cpu_s"] or 0) for r in t)
                print(f"{label:10} {v:18} {len(t):5d} {pause:7.2f} {cpu:6.2f} | {100 * pause / rec_total:16.2f} {100 * pause / frontier_total:10.2f} | "
                      f"{100 * cpu / rec_total:14.2f} {100 * cpu / frontier_total:10.2f} | {max(float(r['wall_ms']) for r in t):12.0f}")

if __name__ == "__main__":
    main(sys.argv[1:])

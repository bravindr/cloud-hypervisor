#!/usr/bin/env python3
"""ckpt_send.py <ch-remote> <api-sock> <daemon-sock> <dirty_log|-> <daemon-log> <daemon-pid>

Checkpoint a paused VM into a resident offload daemon and wait until the
daemon has written it (its "Checkpoint N ..." line). ch-remote send-migration
returns once the migration is dispatched, not when it completes, so the
daemon's log is the completion signal. Retries while CH still reports the
previous migration in progress. Prints: wall_ms cpu_s rc <checkpoint line>.
cpu_s is the daemon's CPU over the checkpoint: the change in the summed run
time of all its threads (/proc/<pid>/task/*/schedstat, ns)."""
import glob, os, re, subprocess, sys, time

remote, api, dsock, dirty, logf, pid = sys.argv[1:7]

def thread_ns():
    total = 0
    for f in glob.glob(f"/proc/{pid}/task/*/schedstat"):
        try:
            total += int(open(f).read().split()[0])
        except (OSError, ValueError, IndexError):
            pass
    return total

offset = os.path.getsize(logf)
c0 = thread_ns()
url = f"destination_url=unix:{dsock},memory_mode=memfds,preserve_source=on" + ("" if dirty == "-" else f",dirty_log={dirty}")
deadline = time.monotonic() + 30
while True:
    t0 = time.monotonic()
    p = subprocess.run([remote, "--api-socket", api, "send-migration", url], capture_output=True, text=True)
    if p.returncode == 0:
        break
    if "already in progress" in p.stderr and time.monotonic() < deadline:
        time.sleep(0.002)
        continue
    print(f"0 0 1 send-migration failed: {p.stderr.strip().splitlines()[-1] if p.stderr.strip() else p.returncode}")
    sys.exit(0)
line = None
pat = re.compile(r"(Checkpoint \d+ .*)")
with open(logf) as f:
    f.seek(offset)
    buf = ""
    while line is None:
        chunk = f.read()
        if chunk:
            buf += chunk
            for l in buf.splitlines():
                m = pat.search(l)
                if m:
                    line = m.group(1)
                    break
        if line is None:
            if time.monotonic() > deadline + 60:
                print("0 0 1 timeout waiting for the daemon")
                sys.exit(0)
            time.sleep(0.0005)
t1 = time.monotonic()
m = re.search(r"cpu_ms=([0-9.]+)", line)
if m:
    # the daemon's own getrusage: includes threads that have exited
    cpu = float(m.group(1)) / 1e3
else:
    time.sleep(0.005)
    cpu = (thread_ns() - c0) / 1e9
rc = 1 if "failed" in line else 0
print(f"{(t1 - t0) * 1e3:.1f} {cpu:.4f} {rc} {line}")

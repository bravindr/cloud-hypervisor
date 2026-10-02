#!/bin/bash
# agent_traj.sh <name> <turn> -- run one recorded real-agent turn in the guest.
#
# bash-only replacement for agent_traj.py: Terminal-Bench task images built
# from ubuntu/debian (C, Rust tasks) have no python3, and the guest must run
# whatever the task image has. mkrootfs_traj.sh pre-renders the trajectory
# from <name>.traj.json into /traj/<name>/<turn>.cmd (the command text; absent
# or empty for a turn that ran nothing) and /traj/<name>/workdir. agent_init
# dispatches "traj-<name>:<turn>" here; the printed line is identical to the
# python runner's so session.py records and verifies it unchanged.
#
# Summary is rc and byte counts only: the command's output may carry pids and
# timestamps, but its exit status and output size are what a replay asserts.
name=${1:?name}; turn=${2:?turn}
T=/traj/$name; TIMEOUT_S=${TRAJ_TIMEOUT:-300}
now() { date +%s.%N; }
t0=$(now)
elapsed() { awk -v a="$t0" -v b="$(now)" 'BEGIN{printf "%.2f", b-a}'; }
if [ ! -d "$T" ]; then
	echo "AGENT_WL2 traj-$name seed=$turn secs=0.00 rc=-2 err=missing-traj $T"; exit 1
fi
f=$T/$turn.cmd
if [ ! -e "$f" ] || [ ! -s "$f" ]; then
	# The model answered without acting, or the loop rejected its reply
	# (unparseable Action Input) and executed nothing. Keep the turn so
	# numbering matches the recording.
	echo "AGENT_WL2 traj-$name seed=$turn secs=0.00 rc=0 out=0 err=0 noop=1"; exit 0
fi
wd=$(cat "$T/workdir" 2>/dev/null); [ -n "$wd" ] && [ -d "$wd" ] || wd=/
out=/tmp/.traj_out.$$; err=/tmp/.traj_err.$$
( cd "$wd" && timeout "$TIMEOUT_S" bash -lc "$(cat "$f")" ) > "$out" 2> "$err"
rc=$?
tmo=""; [ "$rc" = 124 ] && tmo=" timeout=1"
echo "AGENT_WL2 traj-$name seed=$turn secs=$(elapsed) rc=$rc out=$(stat -c %s "$out") err=$(stat -c %s "$err")$tmo"
rm -f "$out" "$err"
exit 0

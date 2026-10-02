#!/bin/bash
# mkrootfs_traj.sh <name> <docker-image> -- build rootfs_traj_<name>.ext4: the
# Terminal-Bench task's own Docker image as the guest filesystem, plus the
# agent-step init and the recorded trajectory.
#
# Same recipe as build_fullrootfs.sh (docker export -> mkfs.ext4 -d), but the
# image is the one Harbor built for the task, so /app and everything the agent
# touched exist exactly as they did when the trajectory was recorded. The
# recorded commands then replay faithfully instead of failing on missing files.
#
# Guest bits added: /agent_init (traj-aware), /agent_traj.py, /agent_wl2.py
# (so mixed sessions still work), /traj/<name>.traj.json.
set -eu
NAME=${1:?name}; IMAGE=${2:?docker image}
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd); D=${ROOTFS_DIR:-$HOME/vmbench}; W=${WORK_DIR:-$D/trajrootfs}/$NAME; TRAJ=$HERE/traj/$NAME.traj.json; G=$HERE/guest
[ -f "$TRAJ" ] || { echo "no trajectory: $TRAJ (run traj_extract.py first)"; exit 1; }
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "image not found: $IMAGE"; exit 1; }
mkdir -p "$W"; cd "$W"

# The image must be able to act as a guest rootfs: an init shell and the tools
# agent_init / agent_traj.sh use. No python3 requirement: the trajectory is
# pre-rendered below and the runner is bash, because C and Rust task images
# (ubuntu:24.04 + gcc/rustc) ship without Python.
for tool in bash awk sed grep date sync timeout stat; do
	docker run --rm --entrypoint sh "$IMAGE" -c "command -v $tool >/dev/null" \
		|| { echo "image lacks $tool -- agent_init needs it"; exit 1; }
done

cid=$(docker create "$IMAGE" /bin/true)
sudo rm -rf root; mkdir root
docker export "$cid" | sudo tar -C root -x
docker rm "$cid" >/dev/null

# guest bits: bash runner + the trajectory rendered to one file per turn
# (host python does the JSON; the guest never needs any)
sudo cp "$G/agent_init" "$G/agent_traj.sh" "$G/agent_wl2.py" root/
sudo chmod 755 root/agent_init root/agent_traj.sh root/agent_wl2.py
sudo mkdir -p "root/traj/$NAME" root/tmp root/proc root/sys root/dev
sudo cp "$TRAJ" root/traj/
rm -rf "$W/turns"; mkdir -p "$W/turns"
python3 - "$TRAJ" "$W/turns" <<'RENDER'
import json, os, sys
t = json.load(open(sys.argv[1])); d = sys.argv[2]
open(os.path.join(d, "workdir"), "w").write((t.get("workdir") or "/app") + "\n")
n = 0
for x in t["turns"]:
    if x.get("command"):
        open(os.path.join(d, "%d.cmd" % x["turn"]), "w").write(x["command"]); n += 1
print("rendered %d command files of %d turns" % (n, t["n_turns"]))
RENDER
sudo cp "$W"/turns/* "root/traj/$NAME/"
# the agent's shell ran as root in $WORKDIR; make sure it exists
WD=$(docker image inspect "$IMAGE" --format '{{.Config.WorkingDir}}'); WD=${WD:-/app}
sudo mkdir -p "root$WD"

sz=$(sudo du -sm root | cut -f1); img=$(( sz * 13 / 10 + 512 ))
OUT=$D/rootfs_traj_$NAME.ext4
echo "rootfs ${sz}MiB -> image ${img}MiB -> $OUT (workdir $WD)"
rm -f "$OUT"; truncate -s ${img}M "$OUT"
sudo mkfs.ext4 -q -F -d root "$OUT"
sudo chown "$USER" "$OUT"
e2fsck -fn "$OUT" | tail -1
echo "turns: $(python3 -c "import json;print(json.load(open('$TRAJ'))['n_turns'])")"
# Compact range form (agent_init expands it): one token per turn put a 60-turn
# session's kernel command line at ~2000 bytes, at x86's 2048 limit, and the
# guest booted with no console output at all.
echo "boot with: agent_steps=$(python3 -c "import json;t=json.load(open('$TRAJ'));ts=[x['turn'] for x in t['turns']];print('traj-$NAME:%d-%d'%(min(ts),max(ts)))")"

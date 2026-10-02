# Real-agent microVM lifecycle benchmark

Replays recorded Terminal-Bench 2 agent sessions inside cloud-hypervisor
guests and checkpoints the guest after every agent turn through the offload
daemon, comparing daemon variants on identical memory images.

## What is here

| path | content |
|---|---|
| `traj/<session>.traj.json` | seven recorded sessions: the shell command each agent turn executed (Qwen2.5-Coder-7B and Qwen3-Coder-30B-A3B driving AgentSysPerf's LiteLLM ReACT loop against a local vLLM), extracted with `traj_extract.py` |
| `guest/agent_init` | PID 1 in the guest: prints `AGENT_BASE_POINT`, runs `agent_steps=traj-<session>:<lo>-<hi>` one turn at a time printing `AGENT_STEP_DONE <k>`, then `AGENT_DIFF_POINT` |
| `guest/agent_traj.sh` | bash turn runner (task images for C/Rust have no python) |
| `mkrootfs_traj.sh` | exports the task's Docker image (`tb2/<task>`) to an ext4 rootfs with the guest bits and the pre-rendered turns |
| `traj_extract.py` | recorded LLM responses -> `traj.json` |
| `lifecycle.sh` | one session: boot, base checkpoint, a checkpoint after every turn by every daemon variant from the same paused image, then restores of the base and final checkpoints with a guest liveness check |
| `run_all.sh`, `analyze_lifecycle.py` | all sessions for 4 KiB and 2 MiB guests, and the summary |

## Lifecycle measured

1. **Create**: boot the task image (RW copy) to `AGENT_BASE_POINT`.
2. **Warm-base checkpoint**: every variant snapshots the paused guest.
3. **Per-turn checkpoints**: after each agent turn the VM is paused and every
   variant snapshots it in turn (order rotated per turn), then the VM resumes.
   Each variant's `send-migration` wall time is the pause it alone would cost.
4. **Resume a session**: the final checkpoint is restored into a fresh VMM;
   the guest must reach `AGENT_DIFF_POINT`.
5. **Fork from the warm base**: the base checkpoint is restored with the
   base-point disk; the guest must complete turn 1 again.

Variants: `base` is the unmodified `iaa-integration` daemon; `cpu`, `dsa`,
`cpucrc`, `dsacrc` are this branch's daemon with `--classify cpu|dsa` and
optionally `--crc` (restore then runs `--verify-crc`; 2 MiB guests restore
with `--hugetlb`). All use `qpl-hardware-static-async`, 1 MiB chunks,
`--workers 8`, the daemon pinned to two cores (one per memory slot) and the VM
to four.

## Running

```bash
# rootfs images (once per session; needs the task's Docker image)
bash benchmark/agent/mkrootfs_traj.sh path-tracing_30b tb2/path-tracing:latest
# accelerators: DSA user WQs in DTO_WQ_LIST, IAA user WQs for QPL;
# 2 MiB guests need >= 2048 free 2 MiB pages on node 0
bash benchmark/agent/run_all.sh 4k 2m
```

Environment: `BIN_DIR` (cloud-hypervisor, ch-remote, offload_daemon),
`BASE_OFFLOAD_BIN`, `ROOTFS_DIR`, `KERNEL`/`INITRD` (a bzImage + initrd with
virtio-blk; tested with Ubuntu 6.8.0-142, copied out of /boot so it is user-readable), `SNAP_ROOT` (tmpfs recommended),
`VARIANTS`, `WORKERS`, `VM_CPUS`, `DAEMON_CPUS`, `DTO_WQ_LIST`.

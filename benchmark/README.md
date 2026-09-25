# Snapshot compression benchmark

This benchmark measures end-to-end Cloud Hypervisor snapshot and eager-restore
performance with raw memory, LZ4, Zstd, and Intel In-Memory Analytics
Accelerator (IAA) compression through Intel QPL.

## Quick start

Run these commands from the Cloud Hypervisor repository root:

```bash
cp benchmark/benchmark.env.example benchmark/benchmark.env
./benchmark/benchmark.sh --dry-run
./benchmark/benchmark.sh
```

The default run uses a 4 GiB VM containing a reproducible 1536 MiB Silesia
working set. It compares raw, LZ4, Zstd, QPL static Huffman, and QPL dynamic
Huffman snapshots, then restores each retained snapshot and writes:

- Raw measurements to `benchmark/results/results.csv`.
- KPI summaries to `benchmark/results/kpi-report.csv`.
- VM, daemon, and serial logs under `benchmark/results/logs/`.

`benchmark.sh` checks dependencies, builds the binaries, downloads the kernel,
guest image, and Silesia corpus when needed, configures IAA user work queues,
creates guest networking, runs the matrices, and cleans up. Package, QPL, and
TAP setup may request `sudo`.

Run multiple VMs concurrently with:

```bash
./benchmark/benchmark.sh --vms 2
```

Set `WITH_QPL=0` and remove QPL codecs from `CODECS` for a CPU-only run. Set
`AUTO_SETUP=0` to report missing dependencies instead of installing them.

## Architecture

Cloud Hypervisor uses its existing migration protocol to transfer VM memory and
state to a separate `offload_daemon`. Compression is outside the VMM process,
so raw, software, and IAA paths share the same migration interface.

The benchmark integration is implemented entirely in user space. Cloud
Hypervisor, `ch-remote`, `offload_daemon`, Intel QPL, workload preparation,
snapshot persistence, restore replay, and result collection all run as user
processes. No custom guest kernel, in-kernel compression path, or guest agent is
required. The host kernel only provides the standard virtualization, memfd,
networking, and IAA/IDXD device interfaces used by those processes.

```text
Snapshot:

Guest VM memory
    -> Cloud Hypervisor migration
    -> offload_daemon
    -> raw copy | LZ4/Zstd on CPU | Intel QPL on IAA
    -> snapshot directory

Restore:

Snapshot directory
    -> offload_daemon
    -> decompression or raw copy
    -> Cloud Hypervisor migration receiver
    -> restored VM
```

### IAA integration

The `offload_daemon` links Intel QPL statically and accesses it through the C
shim in `offload_daemon/src/qpl_shim.c`. Each QPL job submits Deflate work to an
enabled IAA user work queue. The benchmark setup installs QPL v1.9.0 under
`/usr/local` when its headers or static library are absent, then configures four
shared IAA user queues when fewer than `IAX_USER_WQ_COUNT` device nodes exist.
Queue discovery, job construction, submission, completion handling, and
fallback codec selection remain in user space; the IAA device executes the
submitted compression or decompression operation in hardware.

The asynchronous QPL codecs maintain reusable job pools instead of creating a
job for every chunk. `QPL_ASYNC_SNAPSHOT_DEPTHS` and
`QPL_ASYNC_RESTORE_DEPTHS` control the maximum in-flight IAA operations. Static
and dynamic codec names select fixed or dynamic Deflate Huffman tables.
`QPL_DEVICE_NUMA_ID_ANY` permits job submission to any enabled IAA device found
by QPL rather than restricting execution to the calling thread's socket.

IAA engines perform compression and decompression. The pinned offload CPU runs
the daemon's submission, completion, data movement, and persistence work; IAA
engine time is not host CPU utilization.

### Multi-VM execution

For `--vms N`, the parent starts `N` isolated benchmark workers. Each receives
unique API and migration sockets, TAP networking, guest disk, snapshot tree,
logs, and results. Workers rendezvous before every codec and iteration, so the
same snapshot or restore case starts concurrently in every VM.

CPU placement is deterministic by default. The allocator discovers all online
CPUs on `CPU_AFFINITY_SOCKET`, uses one hardware thread from each physical core
before SMT siblings, assigns `VCPUS` CPUs to each VM, and reserves a separate
CPU for each offload daemon. Per-VM results are stored under
`benchmark/results/multi-vm/vm-*`. Aggregate latency is the slowest VM for each
synchronized run; CPU utilization and stored bytes are summed across VMs.
Snapshot and restore events are printed live with `[VM N]` labels and retained
in each `vm-N/benchmark.log`.

## Benchmark workflow and features

The complete workflow is:

1. Check or install host dependencies and QPL.
2. Build Cloud Hypervisor, `ch-remote`, and `offload_daemon`.
3. Download canonical kernel, disk, and workload assets when absent.
4. Create the TAP interface and start a shared-memory source VM.
5. Populate guest memory and pause the VM.
6. Run synchronized snapshot matrices.
7. Stop the source VM to release its disk lock.
8. Run synchronized eager-restore matrices.
9. Generate raw-relative KPI reports and clean up.

The default matrix uses 1 MiB chunks, one software worker, QPL async snapshot
depth 8, QPL async restore depth 32, one warm-up, and three measured
iterations. Configure additional codecs, chunk sizes, workers, queue depths,
and iterations in `benchmark/benchmark.env`.

Before setup and build, the benchmark stops existing processes whose executable
is named `cloud-hypervisor` and removes stale benchmark sockets. Set
`CLEANUP_EXISTING_VMS=0` when unrelated Cloud Hypervisor VMs must remain
running. Cleanup also runs when a benchmark stage fails.

The KPI report includes:

- Dataset, phase, codec, chunk size, worker count, and run count.
- Snapshot and restore mean, median, and p95 latency.
- Median offload-daemon CPU utilization.
- Mean stored snapshot size, compression ratio, and space savings.
- Snapshot and restore speedup relative to the raw median.

## Build

QPL is statically linked from `/usr/local` by default:

```bash
./benchmark/build.sh
```

Override the installation directories when necessary:

```bash
QPL_INCLUDE_DIR=/path/include QPL_LIB_DIR=/path/lib64 \
  ./benchmark/build.sh
```

Set `WITH_QPL=0` for an LZ4/Zstd-only build.

## Configure

```bash
cp benchmark/benchmark.env.example benchmark/benchmark.env
```

Every script also accepts the variables directly in its environment. Set
`BENCHMARK_CONFIG=/path/to/file` to use a configuration outside this directory.

## Start the source VM

Download the repository's canonical x86_64 test kernel and Jammy image:

```bash
./benchmark/download-assets.sh
```

Set the direct-boot kernel and disk image. The lifecycle script copies the disk
by default, generates and attaches a NoCloud configuration disk, starts the VM
with shared memory, and records its PID and logs under `benchmark/results/`:

```bash
KERNEL_PATH=$HOME/workloads/vmlinux \
DISK_PATH=$HOME/workloads/jammy-server-cloudimg-amd64.raw \
MEMORY_SIZE=4G VCPUS=4 \
  ./benchmark/source-vm.sh start
```

For a qcow2 image, the image type is inferred from its filename:

```bash
KERNEL_PATH=$HOME/workloads/vmlinux \
DISK_PATH=$HOME/workloads/jammy-server-cloudimg-amd64.qcow2 \
  ./benchmark/source-vm.sh start
```

Additional Cloud Hypervisor arguments go after `--`. For example, with an
existing TAP device:

```bash
KERNEL_PATH=$HOME/workloads/vmlinux \
DISK_PATH=$HOME/workloads/jammy-server-cloudimg-amd64.raw \
  ./benchmark/source-vm.sh start -- \
  --net tap=tap0,mac=12:34:56:78:90:ab
```

One simple host TAP setup is:

```bash
sudo ip tuntap add tap0 mode tap user "$USER"
sudo ip addr add 192.168.2.1/25 dev tap0
sudo ip link set tap0 up
```

The guest must have an address and route compatible with that TAP network.
Cloud images may require cloud-init or their existing static-network setup.

Inspect lifecycle state and logs with:

```bash
./benchmark/source-vm.sh status
tail -f benchmark/results/logs/source-serial.log
```

## Prepare and pause the VM

`prepare-memory.sh` waits for SSH, prepares a resident workload, and pauses the
VM. File patterns live under `/dev/shm`; the Redis pattern keeps a populated
Redis server resident. The canonical test image uses the development credentials
`cloud/cloud123`; password authentication requires `sshpass`:

```bash
GUEST_SSH_TARGET=cloud@192.168.2.2 \
SSH_PASSWORD=cloud123 \
WORKING_SET_MIB=1536 MEMORY_PATTERN=silesia \
  ./benchmark/prepare-memory.sh
```

For a guest configured with a public key, set `SSH_KEY` instead of
`SSH_PASSWORD`.

Other useful patterns are:

```bash
# Best-case compression
MEMORY_PATTERN=zero ./benchmark/prepare-memory.sh

# Worst-case compression
MEMORY_PATTERN=random ./benchmark/prepare-memory.sh

# Repeat the uncompressed Silesia corpus to WORKING_SET_MIB
MEMORY_PATTERN=silesia ./benchmark/prepare-memory.sh

# Populate a live Redis instance with deterministic 4 KiB values
MEMORY_PATTERN=redis ./benchmark/prepare-memory.sh
```

The complete benchmark also accepts the pattern as an argument:

```bash
./benchmark/benchmark.sh --pattern silesia
```

The Silesia mode downloads the canonical corpus on the host, verifies its
SHA-256 checksum, creates `benchmark/assets/silesia.tar`, copies it to the
guest, and repeats its bytes to the exact requested size. Override
`SILESIA_PATH` to use an existing uncompressed tar.

Redis persistence is disabled so the measured state is resident memory rather
than an RDB or append-only file. `REDIS_VALUE_SIZE` controls value size and
defaults to 4096 bytes. When host `redis-server` and `redis-cli` binaries are
available, they are copied into the guest; override their locations with
`REDIS_SERVER_PATH` and `REDIS_CLI_PATH`. Otherwise, existing guest binaries
are used. As a final fallback, `REDIS_AUTO_INSTALL=1` uses `apt-get`, which
requires guest package-network access.

For Redis, `WORKING_SET_MIB=1536` targets approximately 1536 MiB of Redis
`used_memory`, including keys and dataset allocation overhead, rather than
creating 1536 MiB of values and adding overhead afterward. Loading stops after
the first bounded batch that reaches the target and reports both `used_memory`
and resident-set size.

Run different memory patterns as separate benchmark result sets; do not average
their compression ratios or latency results together.

When the guest workload is already prepared manually, pause it directly:

```bash
./benchmark/source-vm.sh pause
```

Resume or stop it with:

```bash
./benchmark/source-vm.sh resume
./benchmark/source-vm.sh stop
```

The snapshot matrix uses `preserve_source=on`, so the source remains paused and
the same memory state is used for every measured configuration.

## Run snapshot benchmarks

```bash
RESET_RESULTS=1 ./benchmark/run-matrix.sh snapshot
```

The measured interval begins immediately before `send-migration` and ends after
the daemon has compressed, written, synchronized, and acknowledged the
snapshot.

## Run restore benchmarks

Stop the source VM before restore testing so it releases its disk-image locks.
If the source must remain alive, copy its disks while paused and update the
saved migration configuration to use those copies.

The restore matrix uses iteration 1 of every generated snapshot by default:

```bash
./benchmark/run-matrix.sh restore
```

Set `RESTORE_SNAPSHOT_ITERATION` to select another snapshot. `COLD_CACHE=1`
drops the host page cache before each restore and therefore requires running as
root. The measured interval includes decompression, memfd population, VM-state
restore, and the completion or resume handshake.

To limit disk usage, snapshot benchmarking retains only the iteration selected
by `RESTORE_SNAPSHOT_ITERATION`. Set `KEEP_ALL_SNAPSHOTS=1` to retain every
measured snapshot.

## Run one case

```bash
./benchmark/snapshot.sh qpl-hardware-static 1048576 8 1

./benchmark/restore.sh \
  benchmark/results/snapshots/qpl-hardware-static-c1048576-w8-i1 \
  qpl-hardware-static 1048576 8 1
```

Use `qpl-hardware-static` and `qpl-hardware-dynamic` to compare fixed and
dynamic Deflate Huffman coding on IAA. The legacy `qpl-hardware` name remains
an alias for dynamic coding so existing snapshots can still be restored.
Append `-async` to either explicit hardware codec to use a reusable rolling
pool of asynchronously submitted QPL jobs. `SOFTWARE_WORKER_COUNTS` controls
host workers. `QPL_ASYNC_SNAPSHOT_DEPTHS` and
`QPL_ASYNC_RESTORE_DEPTHS` independently control maximum in-flight IAA jobs.
The daemon refills each completed slot immediately, bounds submission and
completion waits, reuses buffers, and divides the configured budget across
independent VM memory slots. See [IAA integration](#iaa-integration) for the
hardware path and [Multi-VM execution](#multi-vm-execution) for CPU placement.

Each measured row includes `cpu_util_pct` from the offload daemon. The KPI
report shows its median as `median_cpu_util_pct`. This excludes Cloud Hypervisor
and `ch-remote`; it measures the host CPU cost of raw copying, compression, or
decompression. With `OFFLOAD_CPU` set, 100% represents one fully occupied host
CPU even when several daemon threads share that CPU. IAA engine execution is
not counted as host CPU utilization.

Raw snapshots ignore chunk size and worker count:

```bash
./benchmark/snapshot.sh raw 0 1 1
```

## Summarize

```bash
./benchmark/summarize.py benchmark/results/results.csv
```

The summary reports run count, mean, median, p95, minimum, and maximum latency.
Generate the raw-relative KPI report independently with:

```bash
./benchmark/report.py \
  benchmark/results/results.csv \
  benchmark/results/kpi-report.csv
```

Use at least five measured iterations. Compare codecs first with identical
chunk sizes and worker counts, then run a second matrix using each codec's
best-performing settings.

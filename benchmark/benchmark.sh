#!/usr/bin/env bash

set -euo pipefail

BENCHMARK_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$BENCHMARK_DIR/.." && pwd)
source "$BENCHMARK_DIR/common.sh"

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Runs the complete compression benchmark:
  build -> download assets -> create TAP -> start VM -> prepare memory
  -> snapshot matrix -> stop source VM -> restore matrix -> KPI report

Configuration is read from benchmark/benchmark.env and environment variables.

Options:
  -p, --pattern PATTERN  Guest memory pattern: zero, repeat, random, silesia, redis
    -n, --vms COUNT        Number of VMs to benchmark concurrently
      --dry-run          Print the resolved configuration without running
  -h, --help             Show this help
EOF
}

dry_run=0
vm_count=${VM_COUNT:-1}
while (($#)); do
    case $1 in
        -p | --pattern)
            [[ $# -ge 2 ]] || {
                echo "$1 requires a pattern" >&2
                usage >&2
                exit 2
            }
            MEMORY_PATTERN=$2
            shift 2
            ;;
        --pattern=*)
            MEMORY_PATTERN=${1#*=}
            shift
            ;;
        -n | --vms)
            [[ $# -ge 2 ]] || {
                echo "$1 requires a count" >&2
                usage >&2
                exit 2
            }
            vm_count=$2
            shift 2
            ;;
        --vms=*)
            vm_count=${1#*=}
            shift
            ;;
        --dry-run)
            dry_run=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[[ "$vm_count" =~ ^[1-9][0-9]*$ ]] || {
    echo "Invalid VM count '$vm_count': expected a positive integer" >&2
    exit 2
}
((vm_count <= 253)) || {
    echo "Invalid VM count '$vm_count': at most 253 isolated IPv4 subnets are available" >&2
    exit 2
}

BUILD_BINARIES=${BUILD_BINARIES:-1}
DOWNLOAD_ASSETS=${DOWNLOAD_ASSETS:-1}
ASSET_DIR=${ASSET_DIR:-$BENCHMARK_DIR/assets}
KERNEL_PATH=${KERNEL_PATH:-$ASSET_DIR/vmlinux-x86_64}
DISK_PATH=${DISK_PATH:-$ASSET_DIR/jammy-server-cloudimg-amd64-custom-20241017-0.qcow2}
TAP_NAME=${TAP_NAME:-tap0}
TAP_HOST_CIDR=${TAP_HOST_CIDR:-192.168.2.1/25}
GUEST_MAC=${GUEST_MAC:-12:34:56:78:90:ab}
GUEST_SSH_TARGET=${GUEST_SSH_TARGET:-cloud@192.168.2.2}
SSH_PASSWORD=${SSH_PASSWORD:-cloud123}
MEMORY_SIZE=${MEMORY_SIZE:-4G}
VCPUS=${VCPUS:-4}
WORKING_SET_MIB=${WORKING_SET_MIB:-1536}
MEMORY_PATTERN=${MEMORY_PATTERN:-silesia}
WITH_QPL=${WITH_QPL:-1}
CHUNK_SIZES=${CHUNK_SIZES:-1048576}
SOFTWARE_WORKER_COUNTS=${SOFTWARE_WORKER_COUNTS:-${WORKER_COUNTS:-1}}
QPL_ASYNC_SNAPSHOT_DEPTHS=${QPL_ASYNC_SNAPSHOT_DEPTHS:-8}
QPL_ASYNC_RESTORE_DEPTHS=${QPL_ASYNC_RESTORE_DEPTHS:-32}
ITERATIONS=${ITERATIONS:-3}
WARMUPS=${WARMUPS:-1}
CLEAN_RESULTS=${CLEAN_RESULTS:-1}
CLEANUP_TAP=${CLEANUP_TAP:-1}
CLEANUP_EXISTING_VMS=${CLEANUP_EXISTING_VMS:-1}
REPORT_CSV=${REPORT_CSV:-$RESULTS_DIR/kpi-report.csv}
AUTO_SETUP=${AUTO_SETUP:-1}

case $MEMORY_PATTERN in
    zero | repeat | random | silesia | redis) ;;
    *)
        echo "Invalid pattern '$MEMORY_PATTERN': expected zero, repeat, random, silesia, or redis" >&2
        exit 2
        ;;
esac

if [[ -z ${CODECS:-} ]]; then
    if [[ "$WITH_QPL" == 1 ]]; then
        CODECS="raw lz4 zstd qpl-hardware-static-async qpl-hardware-dynamic-async"
    else
        CODECS="raw lz4 zstd"
    fi
fi

cat <<EOF
Cloud Hypervisor compression benchmark
    VMs:          $vm_count
  kernel:       $KERNEL_PATH
  disk:         $DISK_PATH
  VM:           $VCPUS vCPUs, $MEMORY_SIZE RAM
  guest data:   $WORKING_SET_MIB MiB, $MEMORY_PATTERN
    guest tmpfs:  ${SHM_SIZE_MIB:-default} MiB
  codecs:       $CODECS
    chunk sizes:  $CHUNK_SIZES
    CPU workers:  $SOFTWARE_WORKER_COUNTS
    QPL async:    snapshot [$QPL_ASYNC_SNAPSHOT_DEPTHS], restore [$QPL_ASYNC_RESTORE_DEPTHS]
    VM CPUs:      ${VM_CPU_LIST:-unbound}
    offload CPU:  ${OFFLOAD_CPU:-unbound}
  iterations:   $ITERATIONS measured + $WARMUPS warm-up
  results:      $RESULTS_DIR
EOF

if ((vm_count > 1)); then
    [[ ${AUTO_CPU_AFFINITY:-1} == 1 ]] || {
        echo "Multiple VMs require AUTO_CPU_AFFINITY=1" >&2
        exit 1
    }
    cpus_per_vm=$((VCPUS + 1))
    required_cpus=$((vm_count * cpus_per_vm))
    ((${#affinity_cpus[@]} >= required_cpus)) || {
        echo "$vm_count VMs require $required_cpus logical CPUs on socket $CPU_AFFINITY_SOCKET; found ${#affinity_cpus[@]}" >&2
        exit 1
    }
    for ((instance = 0; instance < vm_count; instance += 1)); do
        instance_offset=$((instance * cpus_per_vm))
        printf -v instance_vm_cpus '%s,' "${affinity_cpus[@]:instance_offset:VCPUS}"
        instance_vm_cpus=${instance_vm_cpus%,}
        printf '  VM %-7d CPUs [%s], offload CPU %s\n' \
            "$instance" "$instance_vm_cpus" "${affinity_cpus[instance_offset + VCPUS]}"
    done
fi

if [[ "$dry_run" == 1 ]]; then
    exit 0
fi

source_vm_started=0
tap_created=0
multi_vm_pids=()
multi_vm_config_dir=

run_privileged() {
    if ((EUID == 0)); then
        "$@"
    else
        command -v sudo >/dev/null || {
            echo "sudo is required to create the TAP interface" >&2
            exit 1
        }
        sudo "$@"
    fi
}

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    local pid
    for pid in "${multi_vm_pids[@]}"; do
        terminate_process "$pid"
    done
    if [[ "$source_vm_started" == 1 ]]; then
        "$BENCHMARK_DIR/source-vm.sh" stop || true
    fi
    if [[ "$tap_created" == 1 && "$CLEANUP_TAP" == 1 ]]; then
        run_privileged ip link delete "$TAP_NAME" 2>/dev/null || true
    fi
    if ((exit_code != 0)) && [[ -f "$RESULTS_CSV" ]] &&
        tail -n +2 "$RESULTS_CSV" | grep -q .; then
        echo "==> Benchmark failed; generating partial KPI report" >&2
        "$BENCHMARK_DIR/report.py" "$RESULTS_CSV" "$REPORT_CSV" || true
    fi
    if [[ -n "$multi_vm_config_dir" ]]; then
        rm -rf -- "$multi_vm_config_dir"
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

write_config_value() {
    printf '%s=%q\n' "$1" "$2"
}

run_multi_vm_benchmark() {
    local multi_results_dir=${MULTI_VM_RESULTS_DIR:-$RESULTS_DIR/multi-vm}
    local barrier_dir=$multi_results_dir/barriers
    local aggregate_results=$multi_results_dir/results.csv
    local aggregate_report=$multi_results_dir/kpi-report.csv
    local instance instance_offset instance_vm_cpus instance_results config_file
    local failed=0 pid

    if [[ "$CLEAN_RESULTS" == 1 ]]; then
        rm -rf -- "$multi_results_dir"
    fi
    rm -rf -- "$barrier_dir"
    mkdir -p "$barrier_dir"
    multi_vm_config_dir=$(mktemp -d)

    for ((instance = 0; instance < vm_count; instance += 1)); do
        instance_offset=$((instance * cpus_per_vm))
        printf -v instance_vm_cpus '%s,' "${affinity_cpus[@]:instance_offset:VCPUS}"
        instance_vm_cpus=${instance_vm_cpus%,}
        instance_results=$multi_results_dir/vm-$instance
        config_file=$multi_vm_config_dir/vm-$instance.env
        mkdir -p "$instance_results"
        : >"$config_file"
        chmod 600 "$config_file"
        {
            if [[ -n ${BENCHMARK_CONFIG:-} ]]; then
                printf 'source %q\n' "$BENCHMARK_CONFIG"
            elif [[ -f "$BENCHMARK_DIR/benchmark.env" ]]; then
                printf 'source %q\n' "$BENCHMARK_DIR/benchmark.env"
            fi
            write_config_value BUILD_BINARIES 0
            write_config_value AUTO_SETUP 0
            write_config_value DOWNLOAD_ASSETS 0
            write_config_value WITH_QPL "$WITH_QPL"
            write_config_value CLEAN_RESULTS 1
            write_config_value CLEANUP_TAP "$CLEANUP_TAP"
            write_config_value CLEANUP_EXISTING_VMS 0
            write_config_value ASSET_DIR "$ASSET_DIR"
            write_config_value KERNEL_PATH "$KERNEL_PATH"
            write_config_value DISK_PATH "$DISK_PATH"
            write_config_value CH_BIN "$CH_BIN"
            write_config_value REMOTE_BIN "$REMOTE_BIN"
            write_config_value OFFLOAD_BIN "$OFFLOAD_BIN"
            write_config_value MEMORY_SIZE "$MEMORY_SIZE"
            write_config_value VCPUS "$VCPUS"
            write_config_value WORKING_SET_MIB "$WORKING_SET_MIB"
            write_config_value MEMORY_PATTERN "$MEMORY_PATTERN"
            write_config_value CODECS "$CODECS"
            write_config_value CHUNK_SIZES "$CHUNK_SIZES"
            write_config_value SOFTWARE_WORKER_COUNTS "$SOFTWARE_WORKER_COUNTS"
            write_config_value QPL_ASYNC_SNAPSHOT_DEPTHS "$QPL_ASYNC_SNAPSHOT_DEPTHS"
            write_config_value QPL_ASYNC_RESTORE_DEPTHS "$QPL_ASYNC_RESTORE_DEPTHS"
            write_config_value ITERATIONS "$ITERATIONS"
            write_config_value WARMUPS "$WARMUPS"
            write_config_value RESULTS_DIR "$instance_results"
            write_config_value SNAPSHOT_ROOT "$instance_results/snapshots"
            write_config_value RESULTS_CSV "$instance_results/results.csv"
            write_config_value REPORT_CSV "$instance_results/kpi-report.csv"
            write_config_value LOG_DIR "$instance_results/logs"
            write_config_value SOURCE_PID_FILE "$instance_results/source-vm.pid"
            write_config_value SOURCE_VMM_LOG "$instance_results/logs/source-vm.log"
            write_config_value SOURCE_SERIAL_LOG "$instance_results/logs/source-serial.log"
            write_config_value WORKING_DISK_PATH "$instance_results/source-working.${DISK_PATH##*.}"
            write_config_value CLOUD_INIT_PATH "$instance_results/cloud-init.img"
            write_config_value SOURCE_API_SOCKET "/tmp/ch-compression-source-$instance.sock"
            write_config_value RESTORE_API_SOCKET "/tmp/ch-compression-restore-$instance.sock"
            write_config_value OFFLOAD_SOCKET "/tmp/ch-compression-offload-$instance.sock"
            write_config_value RESTORE_SOCKET "/tmp/ch-compression-restore-data-$instance.sock"
            write_config_value TAP_NAME "tap$instance"
            write_config_value TAP_HOST_CIDR "192.168.$((instance + 2)).1/25"
            write_config_value GUEST_IP "192.168.$((instance + 2)).2"
            write_config_value GUEST_PREFIX 25
            write_config_value GUEST_GATEWAY "192.168.$((instance + 2)).1"
            write_config_value GUEST_MAC "02:00:00:00:00:$(printf '%02x' "$instance")"
            write_config_value GUEST_SSH_TARGET "cloud@192.168.$((instance + 2)).2"
            write_config_value SSH_PASSWORD "$SSH_PASSWORD"
            write_config_value SSH_KEY "${SSH_KEY:-}"
            write_config_value AUTO_CPU_AFFINITY 0
            write_config_value VM_CPU_LIST "$instance_vm_cpus"
            write_config_value OFFLOAD_CPU "${affinity_cpus[instance_offset + VCPUS]}"
            write_config_value INSTANCE_ID "$instance"
            write_config_value MULTI_VM_COUNT "$vm_count"
            write_config_value MULTI_VM_BARRIER_DIR "$barrier_dir"
        } >"$config_file"

        BENCHMARK_CONFIG=$config_file "$BENCHMARK_DIR/benchmark.sh" --vms 1 \
            >"$instance_results/benchmark.log" 2>&1 &
        multi_vm_pids+=("$!")
        echo "==> Started benchmark VM $instance (log: $instance_results/benchmark.log)"
    done

    for pid in "${multi_vm_pids[@]}"; do
        if ! wait "$pid"; then
            failed=1
        fi
    done
    multi_vm_pids=()
    ((failed == 0)) || {
        echo "One or more VM benchmarks failed; see $multi_results_dir/vm-*/benchmark.log" >&2
        return 1
    }

    python3 "$BENCHMARK_DIR/aggregate-multi-vm.py" "$aggregate_results" \
        "$multi_results_dir"/vm-*/results.csv
    "$BENCHMARK_DIR/report.py" "$aggregate_results" "$aggregate_report"
    echo "Multi-VM benchmark complete"
    echo "  Per-VM results: $multi_results_dir/vm-*"
    echo "  Aggregate report: $aggregate_report"
}

if [[ "$CLEANUP_EXISTING_VMS" == 1 ]]; then
    echo "==> Cleaning up existing Cloud Hypervisor VMs"
    "$BENCHMARK_DIR/cleanup-vms.sh"
fi

if ! WITH_QPL=$WITH_QPL "$BENCHMARK_DIR/setup.sh" --check; then
    if [[ "$AUTO_SETUP" != 1 ]]; then
        echo "Dependencies are missing and AUTO_SETUP=0." >&2
        echo "Run: $BENCHMARK_DIR/setup.sh" >&2
        exit 1
    fi
    echo "==> Installing missing benchmark dependencies"
    WITH_QPL=$WITH_QPL "$BENCHMARK_DIR/setup.sh"
fi
export PATH="${HOME:-}/.cargo/bin:$PATH"

if [[ "$BUILD_BINARIES" == 1 ]]; then
    echo "==> Building benchmark binaries"
    WITH_QPL=$WITH_QPL "$BENCHMARK_DIR/build.sh"
fi

if [[ ! -f "$KERNEL_PATH" || ! -f "$DISK_PATH" ]]; then
    if [[ "$DOWNLOAD_ASSETS" != 1 ]]; then
        echo "Kernel or disk image is missing and DOWNLOAD_ASSETS=0" >&2
        exit 1
    fi
    echo "==> Downloading canonical test assets"
    ASSET_DIR=$ASSET_DIR "$BENCHMARK_DIR/download-assets.sh"
fi

require_executable "$CH_BIN"
require_executable "$REMOTE_BIN"
require_executable "$OFFLOAD_BIN"
command -v ip >/dev/null || {
    echo "iproute2 is required to configure guest networking" >&2
    exit 1
}
if [[ -n "$SSH_PASSWORD" ]]; then
    command -v sshpass >/dev/null || {
        echo "sshpass is required for the canonical guest password login" >&2
        echo "Install sshpass or configure SSH_KEY and clear SSH_PASSWORD." >&2
        exit 1
    }
fi

if ((vm_count > 1)); then
    run_multi_vm_benchmark
    exit 0
fi

if ! ip link show "$TAP_NAME" >/dev/null 2>&1; then
    echo "==> Creating TAP interface $TAP_NAME"
    tap_owner=${SUDO_USER:-${USER:-$(id -un)}}
    run_privileged ip tuntap add "$TAP_NAME" mode tap user "$tap_owner"
    tap_created=1
fi
if ! ip -o address show dev "$TAP_NAME" | grep -Fq "${TAP_HOST_CIDR%/*}/"; then
    run_privileged ip address add "$TAP_HOST_CIDR" dev "$TAP_NAME"
fi
run_privileged ip link set "$TAP_NAME" up

if [[ "$CLEAN_RESULTS" == 1 ]]; then
    echo "==> Removing previous benchmark results"
    rm -rf -- "$SNAPSHOT_ROOT"
    rm -f -- "$RESULTS_CSV" "$REPORT_CSV"
    mkdir -p "$SNAPSHOT_ROOT"
fi

echo "==> Starting source VM"
start_args=(--net "tap=$TAP_NAME,mac=$GUEST_MAC")
if [[ -n ${EXTRA_CH_ARGS:-} ]]; then
    read -r -a configured_args <<<"$EXTRA_CH_ARGS"
    start_args+=("${configured_args[@]}")
fi
KERNEL_PATH=$KERNEL_PATH \
DISK_PATH=$DISK_PATH \
MEMORY_SIZE=$MEMORY_SIZE \
VCPUS=$VCPUS \
    "$BENCHMARK_DIR/source-vm.sh" start -- "${start_args[@]}"
source_vm_started=1

echo "==> Preparing guest memory and pausing source VM"
GUEST_SSH_TARGET=$GUEST_SSH_TARGET \
SSH_KEY=${SSH_KEY:-} \
SSH_PASSWORD=$SSH_PASSWORD \
WORKING_SET_MIB=$WORKING_SET_MIB \
SHM_SIZE_MIB=${SHM_SIZE_MIB:-} \
MEMORY_PATTERN=$MEMORY_PATTERN \
PAUSE_AFTER_PREPARE=1 \
    "$BENCHMARK_DIR/prepare-memory.sh"

BENCHMARK_DATASET=$MEMORY_PATTERN
export BENCHMARK_DATASET CODECS CHUNK_SIZES SOFTWARE_WORKER_COUNTS
export QPL_ASYNC_SNAPSHOT_DEPTHS QPL_ASYNC_RESTORE_DEPTHS ITERATIONS WARMUPS WITH_QPL

echo "==> Running snapshot matrix"
RESET_RESULTS=1 "$BENCHMARK_DIR/run-matrix.sh" snapshot

echo "==> Stopping source VM before restore"
"$BENCHMARK_DIR/source-vm.sh" stop
source_vm_started=0

echo "==> Running restore matrix"
RESET_RESULTS=0 "$BENCHMARK_DIR/run-matrix.sh" restore

echo "==> Key performance indicators"
"$BENCHMARK_DIR/report.py" "$RESULTS_CSV" "$REPORT_CSV"

echo "Benchmark complete"
echo "  Raw results: $RESULTS_CSV"
echo "  KPI report:  $REPORT_CSV"

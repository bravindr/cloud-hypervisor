#!/usr/bin/env bash

set -euo pipefail

BENCHMARK_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$BENCHMARK_DIR/.." && pwd)

if [[ -n ${BENCHMARK_CONFIG:-} ]]; then
    source "$BENCHMARK_CONFIG"
elif [[ -f "$BENCHMARK_DIR/benchmark.env" ]]; then
    source "$BENCHMARK_DIR/benchmark.env"
fi

BIN_DIR=${BIN_DIR:-$REPO_ROOT/target/release}
CH_BIN=${CH_BIN:-$BIN_DIR/cloud-hypervisor}
REMOTE_BIN=${REMOTE_BIN:-$BIN_DIR/ch-remote}
OFFLOAD_BIN=${OFFLOAD_BIN:-$BIN_DIR/offload_daemon}
SOURCE_API_SOCKET=${SOURCE_API_SOCKET:-/tmp/ch-compression-source.sock}
RESTORE_API_SOCKET=${RESTORE_API_SOCKET:-/tmp/ch-compression-restore.sock}
OFFLOAD_SOCKET=${OFFLOAD_SOCKET:-/tmp/ch-compression-offload.sock}
RESTORE_SOCKET=${RESTORE_SOCKET:-/tmp/ch-compression-restore-data.sock}
RESULTS_DIR=${RESULTS_DIR:-$REPO_ROOT/benchmark/results}
SNAPSHOT_ROOT=${SNAPSHOT_ROOT:-$RESULTS_DIR/snapshots}
RESULTS_CSV=${RESULTS_CSV:-$RESULTS_DIR/results.csv}
LOG_DIR=${LOG_DIR:-$RESULTS_DIR/logs}
TIME_BIN=${TIME_BIN:-/usr/bin/time}
SOCKET_TIMEOUT=${SOCKET_TIMEOUT:-30}
VCPUS=${VCPUS:-4}
SOURCE_PID_FILE=${SOURCE_PID_FILE:-$RESULTS_DIR/source-vm.pid}
SOURCE_VMM_LOG=${SOURCE_VMM_LOG:-$LOG_DIR/source-vm.log}
SOURCE_SERIAL_LOG=${SOURCE_SERIAL_LOG:-$LOG_DIR/source-serial.log}

mkdir -p "$SNAPSHOT_ROOT" "$RESULTS_DIR" "$LOG_DIR"

expand_cpu_list() {
    local entry first last cpu
    local entries=()
    IFS=, read -r -a entries <<<"$1"
    for entry in "${entries[@]}"; do
        if [[ "$entry" == *-* ]]; then
            first=${entry%-*}
            last=${entry#*-}
        else
            first=$entry
            last=$entry
        fi
        for ((cpu = first; cpu <= last; cpu += 1)); do
            printf '%s\n' "$cpu"
        done
    done
}

order_cpus_by_physical_core() {
    local cpu package_id core_id core_key
    local primary_cpus=()
    local sibling_cpus=()
    declare -A seen_cores=()

    for cpu in "$@"; do
        if [[ -r /sys/devices/system/cpu/cpu${cpu}/topology/physical_package_id &&
            -r /sys/devices/system/cpu/cpu${cpu}/topology/core_id ]]; then
            package_id=$(</sys/devices/system/cpu/cpu${cpu}/topology/physical_package_id)
            core_id=$(</sys/devices/system/cpu/cpu${cpu}/topology/core_id)
            core_key=$package_id:$core_id
        else
            core_key=cpu:$cpu
        fi
        if [[ -z ${seen_cores[$core_key]:-} ]]; then
            seen_cores[$core_key]=1
            primary_cpus+=("$cpu")
        else
            sibling_cpus+=("$cpu")
        fi
    done
    printf '%s\n' "${primary_cpus[@]}" "${sibling_cpus[@]}"
}

discover_socket_cpus() {
    local socket_id=$1
    local cpu package_path package_id
    local online_cpus=()
    [[ -r /sys/devices/system/cpu/online ]] || {
        echo "Cannot discover online CPUs" >&2
        return 1
    }
    mapfile -t online_cpus < <(expand_cpu_list "$(</sys/devices/system/cpu/online)")
    for cpu in "${online_cpus[@]}"; do
        package_path=/sys/devices/system/cpu/cpu${cpu}/topology/physical_package_id
        [[ -r "$package_path" ]] || continue
        package_id=$(<"$package_path")
        if [[ "$package_id" == "$socket_id" ]]; then
            printf '%s\n' "$cpu"
        fi
    done
}

if [[ ${AUTO_CPU_AFFINITY:-1} == 1 ]]; then
    CPU_AFFINITY_SOCKET=${CPU_AFFINITY_SOCKET:-0}
    mapfile -t affinity_cpus < <(discover_socket_cpus "$CPU_AFFINITY_SOCKET")
    ((${#affinity_cpus[@]} > 0)) || {
        echo "Cannot discover CPUs for socket $CPU_AFFINITY_SOCKET" >&2
        exit 1
    }
    if [[ ${PREFER_PHYSICAL_CORES:-1} == 1 ]]; then
        mapfile -t affinity_cpus < <(order_cpus_by_physical_core "${affinity_cpus[@]}")
    fi
    ((${#affinity_cpus[@]} > VCPUS)) || {
        echo "Socket $CPU_AFFINITY_SOCKET needs at least $((VCPUS + 1)) logical CPUs" >&2
        exit 1
    }
    if [[ -z ${VM_CPU_LIST:-} ]]; then
        printf -v VM_CPU_LIST '%s,' "${affinity_cpus[@]:0:VCPUS}"
        VM_CPU_LIST=${VM_CPU_LIST%,}
    fi
    if [[ -z ${OFFLOAD_CPU:-} ]]; then
        declare -A vm_cpu_set=()
        while IFS= read -r cpu; do
            vm_cpu_set[$cpu]=1
        done < <(expand_cpu_list "$VM_CPU_LIST")
        for cpu in "${affinity_cpus[@]}"; do
            if [[ -z ${vm_cpu_set[$cpu]:-} ]]; then
                OFFLOAD_CPU=$cpu
                break
            fi
        done
        [[ -n ${OFFLOAD_CPU:-} ]] || {
            echo "No socket $CPU_AFFINITY_SOCKET CPU remains for offload_daemon" >&2
            exit 1
        }
    fi
fi

VM_PREFIX=()
if [[ -n ${VM_CPU_LIST:-} ]]; then
    command -v taskset >/dev/null || {
        echo "VM_CPU_LIST requires taskset" >&2
        exit 1
    }
    VM_PREFIX=(taskset --cpu-list "$VM_CPU_LIST")
fi

OFFLOAD_PREFIX=()
if [[ -n ${OFFLOAD_CPU:-} ]]; then
    command -v taskset >/dev/null || {
        echo "OFFLOAD_CPU requires taskset" >&2
        exit 1
    }
    OFFLOAD_PREFIX=(taskset --cpu-list "$OFFLOAD_CPU")
fi

require_executable() {
    local executable=$1
    if [[ ! -x "$executable" ]]; then
        echo "Required executable not found: $executable" >&2
        exit 1
    fi
}

wait_for_socket() {
    local socket_path=$1
    local waited=0
    while [[ ! -S "$socket_path" ]]; do
        if ((waited >= SOCKET_TIMEOUT * 10)); then
            echo "Timed out waiting for socket: $socket_path" >&2
            return 1
        fi
        sleep 0.1
        ((waited += 1))
    done
}

elapsed_ms() {
    local start_ns=$1
    local end_ns=$2
    awk -v start="$start_ns" -v end="$end_ns" \
        'BEGIN { printf "%.3f", (end - start) / 1000000 }'
}

snapshot_size_bytes() {
    local snapshot_dir=$1
    du -B1 -s "$snapshot_dir" | awk '{print $1}'
}

initialize_results() {
    local expected_header='dataset,phase,codec,chunk_size,workers,iteration,elapsed_ms,cpu_util_pct,stored_bytes,snapshot_dir'
    local legacy_header='phase,codec,chunk_size,workers,iteration,elapsed_ms,cpu_util_pct,stored_bytes,snapshot_dir'
    if [[ ! -e "$RESULTS_CSV" ]]; then
        printf '%s\n' "$expected_header" >"$RESULTS_CSV"
        return
    fi

    local current_header
    IFS= read -r current_header <"$RESULTS_CSV"
    if [[ "$current_header" == "$legacy_header" ]]; then
        local upgraded_results
        upgraded_results=$(mktemp "${RESULTS_CSV}.XXXXXX")
        awk 'NR == 1 { print "dataset," $0; next } { print "unknown," $0 }' \
            "$RESULTS_CSV" >"$upgraded_results"
        mv -- "$upgraded_results" "$RESULTS_CSV"
    elif [[ "$current_header" != "$expected_header" ]]; then
        echo "Unsupported results header: $current_header" >&2
        exit 1
    fi
}

record_result() {
    local phase=$1
    local codec=$2
    local chunk_size=$3
    local workers=$4
    local iteration=$5
    local duration_ms=$6
    local cpu_util_pct=$7
    local stored_bytes=$8
    local snapshot_dir=$9
    local dataset=${BENCHMARK_DATASET:-${MEMORY_PATTERN:-unknown}}

    initialize_results
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$dataset" "$phase" "$codec" "$chunk_size" "$workers" "$iteration" \
        "$duration_ms" "$cpu_util_pct" "$stored_bytes" "$snapshot_dir" >>"$RESULTS_CSV"
}

read_cpu_utilization() {
    local time_file=$1
    tr -d '%[:space:]' <"$time_file"
}

drop_page_cache() {
    if [[ ${COLD_CACHE:-0} != 1 ]]; then
        return
    fi
    if ((EUID != 0)); then
        echo "COLD_CACHE=1 requires root; run the benchmark as root." >&2
        exit 1
    fi
    sync
    echo 3 >/proc/sys/vm/drop_caches
}

terminate_process() {
    local pid=${1:-}
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
}

wait_for_multi_vm_peers() {
    local barrier_name=$1
    [[ -n ${MULTI_VM_BARRIER_DIR:-} ]] || return 0

    local barrier_path=$MULTI_VM_BARRIER_DIR/$barrier_name
    local timeout=${MULTI_VM_BARRIER_TIMEOUT:-300}
    local waited=0
    local participants=()
    mkdir -p "$barrier_path"
    : >"$barrier_path/${INSTANCE_ID:?INSTANCE_ID is required for multi-VM runs}"
    while true; do
        participants=("$barrier_path"/*)
        if ((${#participants[@]} >= ${MULTI_VM_COUNT:?MULTI_VM_COUNT is required for multi-VM runs})); then
            return 0
        fi
        if ((waited >= timeout * 10)); then
            echo "Timed out waiting at multi-VM barrier: $barrier_name" >&2
            return 1
        fi
        sleep 0.1
        ((waited += 1))
    done
}

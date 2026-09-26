#!/usr/bin/env bash

set -euo pipefail

BENCHMARK_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Runs the end-to-end benchmark at increasing VM counts and reports the maximum
VM density whose snapshot and restore p95 latency meet the raw-path SLA.

Options:
    --vm-counts "LIST|auto"  VM counts or automatic socket capacity (default: auto)
    --sla-multiplier VALUE  Raw p95 multiplier (default: SLA_MULTIPLIER or 1.0)
-p, --pattern PATTERN       Guest memory pattern passed to benchmark.sh
    --dry-run               Validate and print every scale configuration
-h, --help                  Show this help
EOF
}

base_config=${BENCHMARK_CONFIG:-$BENCHMARK_DIR/benchmark.env}
[[ -f "$base_config" ]] || {
    echo "Benchmark configuration not found: $base_config" >&2
    exit 1
}
source "$base_config"

vm_counts=${SCALE_VM_COUNTS:-auto}
sla_multiplier=${SLA_MULTIPLIER:-1.0}
pattern=
dry_run=0
while (($#)); do
    case $1 in
        --vm-counts)
            [[ $# -ge 2 ]] || {
                echo "$1 requires a list" >&2
                exit 2
            }
            vm_counts=$2
            shift 2
            ;;
        --vm-counts=*)
            vm_counts=${1#*=}
            shift
            ;;
        --sla-multiplier)
            [[ $# -ge 2 ]] || {
                echo "$1 requires a value" >&2
                exit 2
            }
            sla_multiplier=$2
            shift 2
            ;;
        --sla-multiplier=*)
            sla_multiplier=${1#*=}
            shift
            ;;
        -p | --pattern)
            [[ $# -ge 2 ]] || {
                echo "$1 requires a pattern" >&2
                exit 2
            }
            pattern=$2
            shift 2
            ;;
        --pattern=*)
            pattern=${1#*=}
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

[[ "$sla_multiplier" =~ ^[0-9]+([.][0-9]+)?$ ]] &&
    awk -v value="$sla_multiplier" 'BEGIN { exit !(value > 0) }' || {
    echo "Invalid SLA multiplier '$sla_multiplier': expected a positive number" >&2
    exit 2
}

count_socket_physical_cores() {
    local socket_id=$1
    local cpu_path online package_id core_id core_key
    declare -A physical_cores=()

    for cpu_path in /sys/devices/system/cpu/cpu[0-9]*; do
        if [[ -r "$cpu_path/online" ]]; then
            online=$(<"$cpu_path/online")
            [[ "$online" == 1 ]] || continue
        fi
        [[ -r "$cpu_path/topology/physical_package_id" &&
            -r "$cpu_path/topology/core_id" ]] || continue
        package_id=$(<"$cpu_path/topology/physical_package_id")
        [[ "$package_id" == "$socket_id" ]] || continue
        core_id=$(<"$cpu_path/topology/core_id")
        core_key=$package_id:$core_id
        physical_cores[$core_key]=1
    done
    printf '%s\n' "${#physical_cores[@]}"
}

if [[ "$vm_counts" == auto ]]; then
    [[ ${AUTO_CPU_AFFINITY:-1} == 1 ]] || {
        echo "Automatic VM counts require AUTO_CPU_AFFINITY=1" >&2
        exit 2
    }
    [[ ${VCPUS:-4} =~ ^[1-9][0-9]*$ ]] || {
        echo "Invalid VCPUS '${VCPUS:-}': expected a positive integer" >&2
        exit 2
    }
    offload_cpu_count=1
    for worker_count in ${SOFTWARE_WORKER_COUNTS:-${WORKER_COUNTS:-1}}; do
        [[ "$worker_count" =~ ^[1-9][0-9]*$ ]] || {
            echo "Invalid software worker count '$worker_count': expected a positive integer" >&2
            exit 2
        }
        if ((worker_count > offload_cpu_count)); then
            offload_cpu_count=$worker_count
        fi
    done
    socket_id=${CPU_AFFINITY_SOCKET:-0}
    physical_core_count=$(count_socket_physical_cores "$socket_id")
    cores_per_vm=$((${VCPUS:-4} + offload_cpu_count))
    max_vm_count=$((physical_core_count / cores_per_vm))
    ((max_vm_count > 0)) || {
        echo "Socket $socket_id has $physical_core_count physical cores; $cores_per_vm are required per VM" >&2
        exit 1
    }

    scale_counts=()
    count=1
    while ((count < max_vm_count)); do
        scale_counts+=("$count")
        count=$((count * 2))
    done
    scale_counts+=("$max_vm_count")
    vm_counts=${scale_counts[*]}
    printf 'Automatic scale: socket %s has %s physical cores; %s cores per VM; testing [%s]\n' \
        "$socket_id" "$physical_core_count" "$cores_per_vm" "$vm_counts"
fi

read -r -a scale_counts <<<"$vm_counts"
((${#scale_counts[@]} > 0)) || {
    echo "At least one VM count is required" >&2
    exit 2
}
declare -A seen_counts=()
previous_count=0
for count in "${scale_counts[@]}"; do
    [[ "$count" =~ ^[1-9][0-9]*$ ]] || {
        echo "Invalid VM count '$count': expected a positive integer" >&2
        exit 2
    }
    [[ -z ${seen_counts[$count]:-} ]] || {
        echo "Duplicate VM count: $count" >&2
        exit 2
    }
    ((count > previous_count)) || {
        echo "VM counts must be strictly increasing" >&2
        exit 2
    }
    seen_counts[$count]=1
    previous_count=$count
done

results_root=${SCALE_RESULTS_DIR:-${RESULTS_DIR:-$BENCHMARK_DIR/results}/scale}
detail_report=$results_root/sla-density.csv
summary_report=$results_root/sla-density-summary.csv
config_dir=$(mktemp -d)
cleanup() {
    rm -rf -- "$config_dir"
}
trap cleanup EXIT INT TERM

write_config_value() {
    printf '%s=%q\n' "$1" "$2"
}

report_specs=()
first_run=1
for count in "${scale_counts[@]}"; do
    scale_dir=$results_root/vms-$count
    config_file=$config_dir/vms-$count.env
    {
        printf 'source %q\n' "$base_config"
        write_config_value RESULTS_DIR "$scale_dir"
        write_config_value MULTI_VM_RESULTS_DIR "$scale_dir"
        write_config_value REPORT_CSV "$scale_dir/kpi-report.csv"
        write_config_value CLEAN_RESULTS 1
        if ((first_run == 0)); then
            write_config_value BUILD_BINARIES 0
            write_config_value AUTO_SETUP 0
            write_config_value DOWNLOAD_ASSETS 0
            write_config_value CLEANUP_EXISTING_VMS 0
        fi
    } >"$config_file"
    chmod 600 "$config_file"

    echo "==> Scale point: $count VM(s)"
    benchmark_args=(--vms "$count")
    if [[ -n "$pattern" ]]; then
        benchmark_args+=(--pattern "$pattern")
    fi
    if ((dry_run == 1)); then
        benchmark_args+=(--dry-run)
    fi
    BENCHMARK_CONFIG=$config_file "$BENCHMARK_DIR/benchmark.sh" "${benchmark_args[@]}"

    if ((dry_run == 0)); then
        report_specs+=("$count=$scale_dir/kpi-report.csv")
    fi
    first_run=0
done

if ((dry_run == 1)); then
    echo "Scale benchmark dry-run complete"
    exit 0
fi

"$BENCHMARK_DIR/sla-density-report.py" \
    --output "$detail_report" \
    --summary "$summary_report" \
    --sla-multiplier "$sla_multiplier" \
    "${report_specs[@]}"

echo "Scale benchmark complete"
echo "  Scale results: $results_root/vms-*"
echo "  SLA detail:    $detail_report"
echo "  SLA summary:   $summary_report"

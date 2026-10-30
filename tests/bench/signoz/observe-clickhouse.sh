#!/bin/bash
set -euo pipefail

samples=${1:?Specify the sample count.}
observer_pid=$$
for ((sample = 0; sample < samples; sample++)); do
    common=0
    background=0
    uptime=$(</proc/uptime)
    cpu=$(</sys/fs/cgroup/cpu.stat)
    printf 'sample_begin\t%s\t%s\t%s\n' "$sample" "$EPOCHREALTIME" "$uptime"
    printf 'cgroup_cpu\t%s\n' "$cpu"
    for task in /proc/1/task/*; do
        comm=$(<"$task/comm")
        case "$comm" in
            Common) common=$((common + 1)) ;;
            BgSchPool) background=$((background + 1)) ;;
            *) continue ;;
        esac
        printf 'thread\t%s\t%s\n' "${task##*/}" "$comm"
        # Bash read seeks on regular files; a procfs seek can regenerate changing text.
        status=$(<"$task/status")
        while IFS= read -r line; do
            case "$line" in
                State:*|Uid:*|Cpus_allowed_list:*|SigPnd:*|SigBlk:*|voluntary_ctxt_switches:*|nonvoluntary_ctxt_switches:*)
                    printf '%s\n' "$line"
                    ;;
            esac
        done <<< "$status"
        schedstat=$(<"$task/schedstat")
        syscall=$(<"$task/syscall")
        wchan=$(<"$task/wchan")
        printf 'schedstat\t%s\n' "$schedstat"
        printf 'syscall\t%s\n' "$syscall"
        printf 'wchan\t%s\n' "$wchan"
    done
    if ((common == 0 || background == 0)); then
        printf 'Sample %s lacks required Common or BgSchPool threads under PID 1.\n' "$sample" >&2
        exit 1
    fi
    printf 'thread_counts\tCommon\t%s\tBgSchPool\t%s\n' "$common" "$background"
    observer_schedstat=$(<"/proc/$observer_pid/schedstat")
    printf 'observer\t%s\t%s\n' "$observer_pid" "$observer_schedstat"
    uptime=$(</proc/uptime)
    printf 'sample_end\t%s\t%s\t%s\n' "$sample" "$EPOCHREALTIME" "$uptime"
    if ((sample + 1 < samples)); then
        sleep 1
    fi
done

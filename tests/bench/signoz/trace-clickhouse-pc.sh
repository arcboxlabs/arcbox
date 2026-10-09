#!/bin/bash
set -euo pipefail

fail() {
    printf 'trace-clickhouse-pc: %s\n' "$*" >&2
    exit 1
}
[[ $# -ge 2 ]] || fail 'Usage: prepare INSTANCE GUEST_PID_NAMESPACE; freeze INSTANCE REASON; export INSTANCE'
action=$1
instance=$2
[[ $instance =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ ]] || fail 'Use alphanumerics, underscores, and hyphens for INSTANCE.'
[[ $EUID == 0 ]] || fail 'Run the helper as root in the privileged guest sidecar.'
for tool in awk cat date getconf mkdir readlink rmdir sha256sum uname; do
    command -v "$tool" >/dev/null || fail "Required tool is missing: $tool. Install the tool before capture."
done
capture=/capture/pc
group=arcbox_pc_${instance//-/_}
event_name=$group
trace=/trace/instances/$instance-pc
event=$trace/events/$group/$event_name
instance_owned=0
probe_owned=0

definition() { awk -v key="p:$group/$event_name" '$1 == key' /trace/kprobe_events; }

namespace() {
    [[ $1 =~ ^pid:\[[0-9]+\]$ ]] || fail 'Pass the guest PID namespace token from the recorded guest preflight.'
    [[ $(readlink /proc/self/ns/pid) == "$1" && $(readlink /proc/1/ns/pid) == "$1" ]] || fail 'The sidecar PID namespace differs from the recorded guest namespace.'
}

mark() {
    local state
    state=$(<"$trace/tracing_on")
    printf '%s\t%s\t%s\t%s\n' "$(date -u +%s.%N)" "$state" "$(</proc/uptime)" "$*" >>"$capture/control.tsv"
    if [[ $state == 1 ]]; then printf '%s\n' "$*" >"$trace/trace_marker"; fi
}

configuration() {
    local destination=$1 file cpu
    mkdir "$destination" || return
    for file in tracing_on current_tracer trace_clock tracing_cpumask buffer_size_kb buffer_total_size_kb buffer_subbuf_size_kb trace_options set_event_pid set_event_notrace_pid; do
        cat "$trace/$file" >"$destination/$file" || return
    done
    for cpu in "$trace"/per_cpu/cpu[0-9]*; do
        cat "$cpu/stats" >"$destination/${cpu##*/}.stats" || return
    done
    for file in enable filter format trigger; do
        cat "$event/$file" >"$destination/event-$file" || return
    done
    definition >"$destination/definition" || return
    cat /trace/kprobe_profile >"$destination/kprobe_profile" || return
}

stop_owned() {
    local bad=0
    if ((instance_owned)) && [[ -d $trace ]]; then
        if [[ -e $event/enable ]]; then printf '0\n' >"$event/enable" || bad=1; fi
        printf '0\n' >"$trace/tracing_on" || bad=1
        printf '0\n' >"$trace/events/enable" || bad=1
    fi
    return "$bad"
}

dump_trace() {
    local destination=$1 cpu bad=0
    mkdir -p "$destination" || return
    for cpu in "$trace"/per_cpu/cpu[0-9]*; do
        cat "$cpu/stats" >"$destination/${cpu##*/}.stats-before" || bad=1
        cat "$cpu/trace" >"$destination/${cpu##*/}.trace" || bad=1
        cat "$cpu/stats" >"$destination/${cpu##*/}.stats-after" || bad=1
    done
    return "$bad"
}

remove_owned() {
    local evidence_saved=$1 current bad=0
    stop_owned || bad=1
    if ((probe_owned)); then
        cat /trace/kprobe_profile >"$capture/cleanup-profile" || bad=1
        if ((evidence_saved)); then
            current=$(definition) || bad=1
            if [[ -n $current ]]; then
                if [[ -s $capture/registered-definition && $current == "$(<"$capture/registered-definition")" && $current == "$(<"$capture/requested-definition")" ]]; then
                    printf -- '-:%s/%s\n' "$group" "$event_name" >>/trace/kprobe_events || bad=1
                    [[ -z $(definition) ]] || bad=1
                else
                    printf 'Owned probe definition changed; refusing to delete it.\n' >&2
                    bad=1
                fi
            fi
        fi
    fi
    if ((instance_owned)) && [[ -d $trace ]]; then
        if ((evidence_saved)); then rmdir "$trace" || bad=1; else
            printf 'Trace export is incomplete; preserve the disabled probe and frozen instance %s.\n' "$trace" >&2
            bad=1
        fi
    fi
    cat /trace/kprobe_events >"$capture/kprobe-events-after" || bad=1
    printf '%s\n' "$bad" >"$capture/cleanup-exit" || bad=1
    return "$bad"
}

failure_cleanup() {
    local code=$? bad=0 evidence_saved=1 destination
    trap - EXIT
    if ((code)); then
        printf '%s\t%s\t%d\n' "$(date -u +%s.%N)" "$action" "$code" >>"$capture/failures.tsv" || bad=1
        stop_owned || bad=1
        cat /trace/kprobe_profile >"$capture/failure-profile-$action-$$" || bad=1
        if [[ $action == prepare || $action == export ]]; then
            if ((instance_owned)) && [[ -d $trace ]]; then
                destination=$capture/failure-$action-$$
                configuration "$destination" || {
                    evidence_saved=0
                    bad=1
                }
                dump_trace "$destination" || {
                    evidence_saved=0
                    bad=1
                }
            fi
            remove_owned "$evidence_saved" || bad=1
        fi
        if ((bad)); then printf 'Owned-resource cleanup failed; preserve the sidecar and failure metadata.\n' >&2; fi
    fi
    exit "$code"
}

if [[ $action == prepare ]]; then
    [[ $# == 3 ]] || fail 'prepare requires INSTANCE GUEST_PID_NAMESPACE.'
    namespace "$3"
    [[ $(uname -m) == aarch64 ]] || fail 'The saved-register offsets require an AArch64 guest.'
    [[ -f /capture/instance && $(</capture/instance) == "$instance" && -d /trace/instances/$instance ]] || fail 'Prepare the matching base trace first.'
    [[ $(awk '$2=="/trace" {print $3}' /proc/mounts) == tracefs ]] || fail 'The base sidecar must already mount tracefs at /trace.'
    [[ ! -e $capture && ! -e $trace && ! -e /trace/events/$group ]] || fail 'The PC capture, instance, or probe group already exists.'
    mkdir "$capture"
    printf '%s\n' "$instance" >"$capture/instance"
    printf '%s-pc\n' "$instance" >"$capture/probe-instance"
    printf '%s\n' "$group" >"$capture/group"
    printf '%s\n' "$event_name" >"$capture/event"
    printf '%s\n' "$3" >"$capture/expected-pid-namespace"
else
    [[ -f $capture/instance && $(<"$capture/instance") == "$instance" && $(<"$capture/group") == "$group" && $(<"$capture/event") == "$event_name" ]] || fail 'The PC capture does not own this instance.'
    namespace "$(<"$capture/expected-pid-namespace")"
    [[ $(</proc/sys/kernel/random/boot_id) == "$(<"$capture/boot_id")" ]] || fail 'The guest boot identity changed.'
    if [[ ($action == freeze || $action == export) && -f $capture/export-complete ]]; then exit 0; fi
    [[ -f $capture/instance-owned && -d $trace ]] || fail 'The owned PC trace instance is missing.'
    instance_owned=1
    [[ -f $capture/probe-owned ]] || fail 'The owned PC probe is missing.'
    probe_owned=1
fi
trap failure_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case $action in
prepare)
    cat /proc/sys/kernel/random/boot_id >"$capture/boot_id"
    [[ $(<"$capture/boot_id") == "$(</capture/boot_id)" ]] || fail 'The base trace belongs to another guest boot.'
    uname -a >"$capture/uname"
    cat /proc/config.gz >"$capture/kernel.config.gz"
    sha256sum "$capture/kernel.config.gz" >"$capture/kernel-config-sha256"
    cat /sys/devices/system/cpu/online >"$capture/online"
    cat /proc/self/status >"$capture/tracer-status"
    readlink /proc/self/ns/pid >"$capture/tracer-pid-namespace"
    getconf CLK_TCK >"$capture/user_hz"
    cat /trace/kprobe_events >"$capture/kprobe-events-before"
    # The profile exposes event names without groups; reject an ambiguous name.
    awk -v name="$event_name" '$1 ~ ("/" name "$") {bad=1} END {exit bad}' "$capture/kprobe-events-before" || fail 'Another probe uses the PC event name.'
    mkdir "$trace"
    instance_owned=1
    printf 'owned\n' >"$capture/instance-owned"
    printf '0\n' >"$trace/tracing_on"
    printf 'nop\n' >"$trace/current_tracer"
    printf 'mono\n' >"$trace/trace_clock"
    cat "/trace/instances/$instance/tracing_cpumask" >"$trace/tracing_cpumask"
    printf '2048\n' >"$trace/buffer_size_kb"
    for option in overwrite markers context-info; do printf '1\n' >"$trace/options/$option"; done
    for option in stacktrace userstacktrace event-fork verbose raw hex bin latency-format; do
        if [[ -e $trace/options/$option ]]; then printf '0\n' >"$trace/options/$option"; fi
    done
    printf '0\n' >"$trace/events/enable"
    printf '\n' >"$trace/set_event_pid"
    printf '\n' >"$trace/set_event_notrace_pid"
    # kprobe_events must receive literal $comm and $arg1 expressions.
    # shellcheck disable=SC2016
    printf 'p:%s/%s __arm64_sys_futex comm=$comm pc=+256($arg1):x64 uaddr=+0($arg1):x64 op=+8($arg1):x32 val=+16($arg1):u32 x19=+152($arg1):x64 x20=+160($arg1):u64\n' "$group" "$event_name" >"$capture/requested-definition"
    cat "$capture/requested-definition" >>/trace/kprobe_events
    probe_owned=1
    printf 'owned\n' >"$capture/probe-owned"
    definition >"$capture/registered-definition"
    [[ $(<"$capture/registered-definition") == "$(<"$capture/requested-definition")" ]] || fail 'The registered probe definition differs from the request.'
    printf 'comm == "clickhouse-serv"\n' >"$event/filter"
    [[ $(<"$event/filter") == 'comm == "clickhouse-serv"' ]] || fail 'The probe filter was not applied.'
    awk '!/^#/ && NF {bad=1} END {exit bad}' "$event/trigger" || fail 'The probe event has an unexpected trigger.'
    printf '1\n' >"$event/enable"
    configuration "$capture/initial"
    [[ $(<"$trace/trace_clock") == *'[mono]'* ]] || fail 'The PC clock is not mono.'
    printf '1\n' >"$trace/tracing_on"
    mark "ARM $group"
    ;;
freeze)
    [[ $# == 3 && -n $3 && $3 != *$'\n'* && $3 != *$'\r'* ]] || fail 'freeze requires INSTANCE and a single-line REASON.'
    if [[ -f $capture/freeze-complete ]]; then
        [[ $(<"$trace/tracing_on") == 0 && $(<"$event/enable") == 0 ]] || fail 'The frozen PC probe is active again.'
        exit 0
    fi
    mark "END $3"
    stop_owned
    configuration "$capture/frozen"
    printf '%s\n' "$3" >"$capture/freeze-reason"
    mark "FROZEN $3"
    date -u +%s.%N >"$capture/freeze-complete"
    ;;
export)
    [[ $# == 2 && -f $capture/freeze-complete && $(<"$trace/tracing_on") == 0 && $(<"$event/enable") == 0 ]] || fail 'Freeze the PC trace before export.'
    configuration "$capture/export"
    dump_trace "$capture/export"
    remove_owned 1
    date -u +%s.%N >"$capture/export-complete"
    ;;
*) fail "Unknown operation: $action." ;;
esac

#!/bin/bash
set -euo pipefail

fail() { printf 'trace-clickhouse: %s\n' "$*" >&2; exit 1; }
[[ $# -ge 2 ]] || fail 'Usage: prepare INSTANCE on|off; identity INSTANCE LABEL GUEST_PID; freeze INSTANCE REASON; export INSTANCE'
action=$1
instance=$2
[[ $instance =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]] || fail 'Invalid instance name.'
[[ $EUID == 0 ]] || fail 'Run the helper as root in the privileged guest sidecar.'
for tool in awk cat date getconf mkdir mount readlink rmdir uname; do
    command -v "$tool" >/dev/null || fail "Required tool is missing: $tool. Install the tool before capture."
done
trace=/trace/instances/$instance
stack_trace=/trace/instances/$instance-stacks
stack_event=$stack_trace/events/lock/contention_begin
events=(sched/sched_switch sched/sched_waking sched/sched_wakeup sched/sched_wakeup_new
    sched/sched_migrate_task sched/sched_process_fork sched/sched_process_exec sched/sched_process_exit
    signal/signal_generate signal/signal_deliver syscalls/sys_enter_futex syscalls/sys_exit_futex
    syscalls/sys_enter_rt_sigreturn syscalls/sys_enter_restart_syscall syscalls/sys_exit_restart_syscall
    sched/sched_stat_runtime lock/contention_begin lock/contention_end)

mark() {
    local now state
    now=$(date -u +%s.%N)
    state=$(<"$trace/tracing_on")
    printf '%s\t%s\t%s\t%s\n' "$now" "$state" "$(</proc/uptime)" "$*" >> /capture/control.tsv
    # Frozen identities have a control-log bracket; recording must remain stopped.
    if [[ $state == 1 ]]; then printf '%s\n' "$*" > "$trace/trace_marker"; fi
}

configuration() {
    local destination=$1 file event
    mkdir "$destination"
    for file in tracing_on current_tracer trace_clock tracing_cpumask buffer_size_kb buffer_total_size_kb buffer_subbuf_size_kb \
        trace_options set_event_pid set_event_notrace_pid; do
        cat "$trace/$file" > "$destination/$file"
    done
    cat "$trace/events/enable" > "$destination/events-enable"
    for event in "${events[@]}"; do
        mkdir "$destination/${event//\//-}"
        for file in enable filter trigger format; do
            cat "$trace/events/$event/$file" > "$destination/${event//\//-}/$file"
        done
    done
}


stack_configuration() {
    local destination=$1 file
    mkdir "$destination"
    for file in tracing_on current_tracer trace_clock tracing_cpumask buffer_size_kb buffer_total_size_kb buffer_subbuf_size_kb \
        trace_options set_event_pid set_event_notrace_pid; do
        cat "$stack_trace/$file" > "$destination/$file"
    done
    cat "$stack_trace/events/enable" > "$destination/events-enable"
    mkdir "$destination/lock-contention_begin"
    for file in enable filter trigger format; do
        cat "$stack_event/$file" > "$destination/lock-contention_begin/$file"
    done
}

stack_remove_trigger() {
    local current
    current=$(cat "$stack_event/trigger") || return 1
    if [[ $current == *stacktrace:* ]]; then printf '!stacktrace\n' > "$stack_event/trigger" || return 1; fi
    if ! awk '!/^#/ && NF {bad=1} END {exit bad}' "$stack_event/trigger"; then
        printf 'A stack trigger remains installed.\n' >&2
        return 1
    fi
}

stack_prepare() {
    local option cpu actual_kb total_kb=0 requested_kb=1024 blocks expected_kb
    mkdir /capture/stacks
    printf '%s-stacks\n' "$instance" > /capture/stacks/instance
    mkdir "$stack_trace"
    printf '0\n' > "$stack_trace/tracing_on"
    printf 'nop\n' > "$stack_trace/current_tracer"
    printf 'mono\n' > "$stack_trace/trace_clock"
    printf '3ffff\n' > "$stack_trace/tracing_cpumask"
    printf '%s\n' "$requested_kb" > "$stack_trace/buffer_size_kb"
    for option in overwrite markers context-info; do printf '1\n' > "$stack_trace/options/$option"; done
    for option in stacktrace userstacktrace event-fork verbose raw hex bin latency-format; do
        if [[ -e $stack_trace/options/$option ]]; then printf '0\n' > "$stack_trace/options/$option"; fi
    done
    printf '0\n' > "$stack_trace/events/enable"
    printf '\n' > "$stack_trace/set_event_pid"
    printf '\n' > "$stack_trace/set_event_notrace_pid"
    [[ -d $stack_event ]] || fail 'The stack instance requires lock/contention_begin.'
    awk '!/^#/ && NF {bad=1} END {exit bad}' "$stack_event/trigger" || fail 'The stack event has a trigger.'
    printf 'flags == 1\n' > "$stack_event/filter"
    [[ $(<"$stack_event/filter") == 'flags == 1' ]] || fail 'The stack event filter was not applied.'
    [[ $(<"$stack_trace/current_tracer") == nop && $(<"$stack_trace/trace_clock") == *'[mono]'* ]] || fail 'The stack tracer or clock was not selected.'
    [[ $(<"$stack_trace/tracing_cpumask") =~ ^0*3ffff$ ]] || fail 'The stack CPU mask was not applied.'
    blocks=$(((requested_kb * 1024 + data_bytes - 1) / data_bytes))
    if ((blocks < 2)); then blocks=2; fi
    expected_kb=$((blocks * data_bytes / 1024))
    printf 'requested_kb\t%s\npayload_bytes\t%s\nblocks\t%s\nexpected_per_cpu_kb\t%s\n' \
        "$requested_kb" "$data_bytes" "$blocks" "$expected_kb" > /capture/stacks/buffer-sizing.tsv
    for cpu in {0..17}; do
        actual_kb=$(<"$stack_trace/per_cpu/cpu$cpu/buffer_size_kb")
        [[ $actual_kb == "$expected_kb" ]] || fail "Stack CPU $cpu buffer is $actual_kb KiB; expected $expected_kb."
        total_kb=$((total_kb + actual_kb))
        printf 'cpu%s_kb\t%s\n' "$cpu" "$actual_kb" >> /capture/stacks/buffer-sizing.tsv
    done
    [[ $(<"$stack_trace/buffer_size_kb") == "$expected_kb" && $(<"$stack_trace/buffer_total_size_kb") == "$total_kb" ]] || fail 'The stack buffer sizes disagree.'
    : > "$stack_trace/trace"
    stack_configuration /capture/stacks/initial
    for cpu in {0..17}; do cat "$stack_trace/per_cpu/cpu$cpu/stats" > "/capture/stacks/initial/cpu$cpu.stats"; done
}

stack_arm() {
    if [[ $1 == on ]]; then
        printf '1\n' > "$stack_trace/tracing_on"
        [[ $(<"$stack_trace/tracing_on") == 1 ]] || fail 'The stack instance did not start.'
        printf '1\n' > "$stack_event/enable"
        printf '%s\t%s\n' "$(date -u +%s.%N)" "$(</proc/uptime)" > /capture/stacks/arm-before.tsv
        # Conditional triggers can spend their count while tracing is off.
        printf 'stacktrace:128 if flags == 1\n' > "$stack_event/trigger"
        printf '%s\t%s\n' "$(date -u +%s.%N)" "$(</proc/uptime)" > /capture/stacks/arm-after.tsv
    fi
    cat "$stack_event/trigger" > /capture/stacks/armed-trigger
    if [[ $1 == on ]]; then
        awk '$1 ~ /^stacktrace:count=[0-9]+$/ && $2=="if" && $3=="flags" && $4=="==" && $5=="1" {found++} END {exit found!=1}' \
            /capture/stacks/armed-trigger || fail 'The counted SPIN stack trigger was not installed.'
    fi
}

stack_freeze() {
    if [[ -f /capture/stacks/freeze-complete ]]; then
        [[ $(<"$stack_trace/tracing_on") == 0 ]] || fail 'A frozen stack instance is recording again.'
        awk '!/^#/ && NF {bad=1} END {exit bad}' "$stack_event/trigger" || fail 'A frozen stack trigger is installed again.'
        return
    fi
    printf '0\n' > "$stack_trace/tracing_on"
    printf '0\n' > "$stack_trace/events/enable"
    if [[ ! -f /capture/stacks/frozen-trigger-before-remove ]]; then
        cat "$stack_event/trigger" > /capture/stacks/frozen-trigger-before-remove
    fi
    # Disabling recording or the event does not unregister a conditional trigger.
    stack_remove_trigger
    cat "$stack_event/trigger" > /capture/stacks/frozen-trigger-after-remove
    [[ $(<"$stack_trace/tracing_on") == 0 ]] || fail 'The stack instance did not stop.'
    if [[ ! -d /capture/stacks/frozen ]]; then stack_configuration /capture/stacks/frozen; fi
    date -u +%s.%N > /capture/stacks/freeze-complete
}

failure_cleanup() {
    local code=$? active
    trap - EXIT
    if ((code)); then
        printf '%s\t%d\n' "$action" "$code" >> /capture/failures.tsv
        for active in "$trace" "$stack_trace"; do
            if [[ -e $active/tracing_on ]]; then
                printf '0\n' > "$active/tracing_on" || printf 'Failed to stop %s\n' "$active" >&2
                printf '0\n' > "$active/events/enable" || printf 'Failed to disable events for %s\n' "$active" >&2
            fi
        done
        if [[ -e $stack_event/trigger ]]; then
            cat "$stack_event/trigger" >> /capture/stacks/failure-trigger-before-remove || printf 'Failed to save the stack trigger.\n' >&2
            stack_remove_trigger || printf 'Failed to remove the stack trigger.\n' >&2
        fi
    fi
    exit "$code"
}

if [[ $action == prepare ]]; then
    [[ $# == 3 && ($3 == on || $3 == off) ]] || fail 'prepare requires INSTANCE on|off.'
    [[ $(</sys/devices/system/cpu/online) == 0-17 ]] || fail 'The capture requires online CPUs 0-17.'
    awk '/^MemAvailable:/ {ok=($2>2097152)} END {exit !ok}' /proc/meminfo || fail 'More than 2 GiB of guest MemAvailable is required.'
    [[ -r /proc/config.gz ]] || fail 'The guest must expose /proc/config.gz.'
    [[ ! -e /capture ]] || fail '/capture already exists; use a fresh sidecar.'
    [[ -z $(awk '$2=="/trace" {print $3}' /proc/mounts) ]] || fail '/trace is already mounted.'
    mkdir /capture
    printf '%s\n' "$instance" > /capture/instance
    printf '%s\n' "$3" > /capture/mode
    mkdir -p /trace
    mount -t tracefs tracefs /trace
    for existing in /trace/instances/*; do
        [[ ! -e $existing ]] || fail "Existing trace instance requires export and cleanup before capture: $existing."
    done
    [[ ! -e $trace ]] || fail 'The trace instance already exists.'
    mkdir "$trace"
else
    [[ -f /capture/instance && $(</capture/instance) == "$instance" ]] || fail 'The capture does not own this instance.'
    if [[ $action == freeze && $# == 3 && -f /capture/export-complete ]]; then exit 0; fi
    [[ -d $trace ]] || fail 'The trace instance is missing.'
fi
# A failed operation must preserve both buffers and stop both instances.
trap failure_cleanup EXIT

case $action in
    prepare)
        printf '0\n' > "$trace/tracing_on"
        printf 'nop\n' > "$trace/current_tracer"
        printf 'mono\n' > "$trace/trace_clock"
        printf '3ffff\n' > "$trace/tracing_cpumask"
        requested_kb=32768
        printf '%s\n' "$requested_kb" > "$trace/buffer_size_kb"
        for option in overwrite markers context-info; do
            printf '1\n' > "$trace/options/$option"
            [[ $(<"$trace/options/$option") == 1 ]] || fail "The instance did not enable $option."
        done
        # New instances inherit some global options, including stack capture.
        for option in stacktrace userstacktrace event-fork verbose raw hex bin latency-format; do
            if [[ -e $trace/options/$option ]]; then printf '0\n' > "$trace/options/$option"; fi
        done
        printf '0\n' > "$trace/events/enable"
        printf '\n' > "$trace/set_event_pid"
        printf '\n' > "$trace/set_event_notrace_pid"
        for event in "${events[@]}"; do
            [[ -d $trace/events/$event ]] || fail "Required event is missing: $event."
            [[ $(<"$trace/events/$event/filter") == none ]] || fail "Event has a filter: $event."
            awk '!/^#/ && NF {bad=1} END {exit bad}' "$trace/events/$event/trigger" || fail "Event has a trigger: $event."
            if [[ $3 == on ]]; then printf '1\n' > "$trace/events/$event/enable"; fi
        done
        [[ $(<"$trace/current_tracer") == nop && $(<"$trace/trace_clock") == *'[mono]'* ]] || fail 'The requested tracer or clock was not selected.'
        cat "$trace/events/header_page" > /capture/header_page
        data_bytes=$(awk '/field: char data;/ {sub(/.*size:/, ""); sub(/;.*/, ""); print; found++} END {if (found!=1) exit 1}' /capture/header_page)
        [[ $data_bytes =~ ^[1-9][0-9]*$ ]] || fail 'The ring buffer payload size is invalid.'
        # The kernel rounds up to payload blocks, then truncates each CPU capacity to KiB.
        blocks=$(((requested_kb * 1024 + data_bytes - 1) / data_bytes))
        if ((blocks < 2)); then blocks=2; fi
        expected_kb=$((blocks * data_bytes / 1024))
        total_kb=0
        printf 'requested_kb\t%s\npayload_bytes\t%s\nblocks\t%s\nexpected_per_cpu_kb\t%s\n' \
            "$requested_kb" "$data_bytes" "$blocks" "$expected_kb" > /capture/buffer-sizing.tsv
        for cpu in {0..17}; do
            actual_kb=$(<"$trace/per_cpu/cpu$cpu/buffer_size_kb")
            [[ $actual_kb == "$expected_kb" ]] || fail "CPU $cpu buffer is $actual_kb KiB; expected $expected_kb after payload-block rounding."
            total_kb=$((total_kb + actual_kb))
            printf 'cpu%s_kb\t%s\n' "$cpu" "$actual_kb" >> /capture/buffer-sizing.tsv
        done
        [[ $(<"$trace/buffer_size_kb") == "$expected_kb" && $(<"$trace/buffer_total_size_kb") == "$total_kb" ]] || fail 'Aggregate trace buffer sizes disagree with the per-CPU sizes.'
        mask=$(<"$trace/tracing_cpumask")
        [[ $mask =~ ^0*3ffff$ ]] || fail "Unexpected trace CPU mask: $mask."
        [[ -z $(<"$trace/set_event_pid") && -z $(<"$trace/set_event_notrace_pid") ]] || fail 'An event PID filter remains.'
        stack_prepare
        cat /proc/meminfo > /capture/meminfo
        awk '/^MemAvailable:/ {ok=($2>2097152)} END {exit !ok}' /capture/meminfo || fail 'More than 2 GiB must remain after trace allocation.'
        cat /proc/config.gz > /capture/kernel.config.gz
        cat /proc/sys/kernel/random/boot_id > /capture/boot_id
        cat /sys/devices/system/cpu/online > /capture/online
        cat /sys/devices/system/cpu/possible > /capture/possible
        cat /proc/self/status > /capture/tracer-status
        cat /proc/1/status > /capture/guest-pid1-status
        readlink /proc/self/ns/pid > /capture/tracer-pid-namespace
        readlink /proc/1/ns/pid > /capture/guest-pid1-namespace
        [[ $(</capture/tracer-pid-namespace) == "$(</capture/guest-pid1-namespace)" ]] || fail 'The sidecar must share the guest PID 1 namespace.'
        getconf CLK_TCK > /capture/user_hz
        uname -a > /capture/uname
        cat /trace/available_events > /capture/available_events
        for option in record-cmd record-tgid; do cat "/trace/options/$option" > "/capture/global-$option"; done
        : > "$trace/trace"
        configuration /capture/initial
        for cpu in {0..17}; do cat "$trace/per_cpu/cpu$cpu/stats" > "/capture/initial/cpu$cpu.stats"; done
        printf '1\n' > "$trace/tracing_on"
        [[ $(<"$trace/tracing_on") == 1 ]] || fail 'The trace instance did not start.'
        stack_arm "$3"
        mark "ARM $instance $3"
        ;;
    identity)
        [[ $# == 4 && $3 =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ && $4 =~ ^[1-9][0-9]*$ ]] || fail 'identity requires INSTANCE LABEL GUEST_PID.'
        destination=/capture/identity-$3
        mkdir "$destination"
        printf 'incomplete\n' > "$destination/completeness"
        mark "IDENTITY_BEGIN $3 $4"
        (
            cat "/proc/$4/maps" > "$destination/maps"
            cat "/proc/$4/cgroup" > "$destination/cgroup"
            for task in /proc/"$4"/task/[0-9]*; do
                tid=${task##*/}
                mkdir "$destination/$tid"
                for file in status stat; do cat "$task/$file" > "$destination/$tid/$file"; done
                readlink -v "$task/ns/pid" > "$destination/$tid/ns-pid"
            done
        ) 2> "$destination/errors"
        mark "IDENTITY_END $3 $4"
        printf 'complete\n' > "$destination/completeness"
        if [[ $3 == before ]]; then mark WINDOW_BEGIN; fi
        ;;
    freeze)
        [[ $# == 3 && -n $3 && $3 != *$'\n'* && $3 != *$'\r'* ]] || fail 'freeze requires a single-line reason.'
        if [[ -f /capture/freeze-reason ]]; then
            [[ $(<"$trace/tracing_on") == 0 ]] || fail 'A frozen instance is recording again.'
            stack_freeze
            exit 0
        fi
        mark "END $3"
        printf '0\n' > "$trace/tracing_on"
        printf '0\n' > "$trace/events/enable"
        [[ $(<"$trace/tracing_on") == 0 ]] || fail 'The trace instance did not stop.'
        stack_freeze
        printf '%s\n' "$3" > /capture/freeze-reason
        mark "FROZEN $3"
        ;;
    export)
        [[ $# == 2 ]] || fail 'export requires INSTANCE.'
        [[ $(<"$trace/tracing_on") == 0 ]] || fail 'Freeze the trace instance before export.'
        if [[ -d $stack_trace ]]; then stack_freeze; fi
        configuration /capture/export
        for cpu in {0..17}; do cat "$trace/per_cpu/cpu$cpu/stats" > "/capture/export/cpu$cpu.stats-before"; done
        for cpu in {0..17}; do cat "$trace/per_cpu/cpu$cpu/trace" > "/capture/export/cpu$cpu.trace"; done
        for cpu in {0..17}; do cat "$trace/per_cpu/cpu$cpu/stats" > "/capture/export/cpu$cpu.stats-after"; done
        for option in record-cmd record-tgid; do cat "/trace/options/$option" > "/capture/export/global-$option"; done
        cat /trace/saved_cmdlines > /capture/export/saved_cmdlines
        cat /trace/saved_tgids > /capture/export/saved_tgids
        cat /sys/devices/system/cpu/online > /capture/export/online
        [[ $(<"$trace/tracing_on") == 0 ]] || fail 'The trace instance restarted during export.'
        if [[ -d $stack_trace ]]; then
            stack_configuration /capture/stacks/export
            for cpu in {0..17}; do cat "$stack_trace/per_cpu/cpu$cpu/stats" > "/capture/stacks/export/cpu$cpu.stats-before"; done
            for cpu in {0..17}; do cat "$stack_trace/per_cpu/cpu$cpu/trace" > "/capture/stacks/export/cpu$cpu.trace"; done
            for cpu in {0..17}; do cat "$stack_trace/per_cpu/cpu$cpu/stats" > "/capture/stacks/export/cpu$cpu.stats-after"; done
            [[ $(<"$stack_trace/tracing_on") == 0 ]] || fail 'The stack instance restarted during export.'
        fi
        mark EXPORTED
        if [[ -d $stack_trace ]]; then rmdir "$stack_trace"; fi
        rmdir "$trace"
        date -u +%s.%N > /capture/export-complete
        ;;
    *) fail "Unknown operation: $action." ;;
esac

#!/bin/sh
# Usage: sh probe-overlay-lifecycle.sh prepare|stop|export|cleanup TOKEN
# Evidence: /run/arcbox/overlay-probe-TOKEN/export/trace.txt (read with abctl cat).
# Validation: sh -n; reject TOKEN=../bad before writes; check all postconditions below.
# After one bounded copy, require paired probes, zero misses and zero CPU overruns/drops.
# sched_process_exit and sys_exit_umount do not prove final mount-reference release.
set -eu
umask 077

fail() { printf 'overlay-probe: %s\n' "$*" >&2; exit 1; }
[ "$#" = 2 ] || fail 'Expected ACTION TOKEN.'
action=$1
token=$2
case $action in prepare|stop|export|cleanup) ;; *) fail 'Unknown action.' ;; esac
case $token in ''|[!A-Za-z]*|*[!A-Za-z0-9_]*) fail 'TOKEN must start with a letter and contain only letters, digits and underscores.' ;; esac
[ "${#token}" -le 32 ] || fail 'TOKEN must contain at most 32 characters.'
for tool in awk cat date id mkdir rmdir uname; do
    command -v "$tool" >/dev/null || fail "Required guest tool is missing: $tool. Restore the prerequisite before capture."
done
[ "$(id -u)" = 0 ] || fail 'Run as root inside the diagnostic guest.'
[ "$(uname -m)" = aarch64 ] || fail 'These probes require the verified arm64 guest.'
base=/sys/kernel/tracing
group=ovldiag_$token
trace=$base/instances/$token
out=/run/arcbox/overlay-probe-$token
events='syscalls/sys_enter_mount syscalls/sys_exit_mount syscalls/sys_enter_umount syscalls/sys_exit_umount syscalls/sys_enter_unshare syscalls/sys_exit_unshare sched/sched_process_exit nfsd/nfsd_export_update'
boot=$(cat /proc/sys/kernel/random/boot_id)

group_definitions() {
    awk -v wanted="$group" '{split($1,a,":"); split(a[2],b,"/"); if (b[1]==wanted) print}' "$base/kprobe_events"
}

stop_instance() {
    [ -f "$out/instance-owned" ] && [ -d "$trace" ] || return 0
    if [ "$(cat "$trace/tracing_on")" = 1 ]; then
        printf 'END %s\n' "$token" > "$trace/trace_marker" || return 1
    fi
    printf '0\n' > "$trace/tracing_on" || return 1
    printf '0\n' > "$trace/events/enable" || return 1
    trigger=$trace/events/$group/unlock/trigger
    if [ -f "$trigger" ] && awk '/^stacktrace:/ {found=1} END {exit !found}' "$trigger"; then
        cat "$trigger" > "$out/stack-trigger-before-remove" || return 1
        printf '!stacktrace\n' > "$trigger" || return 1
    fi
    [ "$(cat "$trace/tracing_on")" = 0 ] || return 1
    [ "$(cat "$trace/events/enable")" = 0 ] || return 1
}

failure() {
    code=$?
    trap - 0
    if [ "$code" != 0 ]; then
        printf '%s\t%s\n' "$action" "$code" >> "$out/failures.tsv"
        stop_instance || printf 'overlay-probe: Failed to stop the owned instance; inspect %s.\n' "$trace" >&2
    fi
    exit "$code"
}

if [ "$action" = prepare ]; then
    [ ! -e "$out" ] || fail 'The evidence directory already exists; choose a fresh token.'
    [ ! -e "$trace" ] || fail 'The trace instance already exists.'
    [ -w "$base/kprobe_events" ] && [ -r "$base/kprobe_profile" ] || fail 'The diagnostic kernel and mounted tracefs must expose kprobe_events and kprobe_profile.'
    [ ! -d "$base/events/$group" ] && [ -z "$(group_definitions)" ] || fail 'The probe group already exists.'
    mkdir "$out"
    printf '%s\n%s\n' "$group" "$boot" > "$out/owner"
else
    [ -f "$out/owner" ] || fail 'No ownership record exists for this token.'
    [ "$(cat "$out/owner")" = "$(printf '%s\n%s' "$group" "$boot")" ] || fail 'The token or guest boot identity changed.'
fi
trap failure 0
trap 'exit 130' INT
trap 'exit 143' TERM

case $action in
    prepare)
        # ovl_get_upper is inlined in this kernel; ovl_fill_super is the explicit creation boundary.
        # Return probes retain original arguments. Void-return probes never interpret a return value.
        cat > "$out/probes.table" <<'PROBES'
p fill_super ovl_fill_super sb=$arg1:x64 fc=$arg2:x64
r128 fill_super_ret ovl_fill_super sb=$arg1:x64 fc=$arg2:x64 ret=$retval:s32
p trylock ovl_inuse_trylock dentry=$arg1:x64
r128 trylock_ret ovl_inuse_trylock dentry=$arg1:x64 ret=$retval:u8
p unlock ovl_inuse_unlock dentry=$arg1:x64
r128 unlock_ret ovl_inuse_unlock dentry=$arg1:x64
p free_fs ovl_free_fs ofs=$arg1:x64
r128 free_fs_ret ovl_free_fs ofs=$arg1:x64
p put_super ovl_put_super sb=$arg1:x64
r128 put_super_ret ovl_put_super sb=$arg1:x64
p cleanup_mnt cleanup_mnt mount=$arg1:x64
r128 cleanup_mnt_ret cleanup_mnt mount=$arg1:x64
p deactivate_super deactivate_super sb=$arg1:x64
r128 deactivate_super_ret deactivate_super sb=$arg1:x64
p put_mnt_ns put_mnt_ns ns=$arg1:x64
r128 put_mnt_ns_ret put_mnt_ns ns=$arg1:x64
p mntput mntput_no_expire mount=$arg1:x64
r128 mntput_ret mntput_no_expire mount=$arg1:x64
p namespace_unlock namespace_unlock
r128 namespace_unlock_ret namespace_unlock
p exit_namespaces exit_task_namespaces task=$arg1:x64
r128 exit_namespaces_ret exit_task_namespaces task=$arg1:x64
PROBES
        while read -r kind name symbol fetch; do
            awk -v wanted="$symbol" '$3==wanted && $2~/^[tT]$/ {n++} END {exit n!=1}' /proc/kallsyms || fail "Required unique function symbol is missing: $symbol."
        done < "$out/probes.table"
        for event in $events; do
            [ -d "$base/events/$event" ] || fail "Required tracepoint is missing: $event."
        done
        cat /proc/version > "$out/kernel-version"
        cat /proc/kallsyms > "$out/kallsyms"
        cat "$base/kprobe_events" > "$out/kprobes-before"
        cat "$base/kprobe_profile" > "$out/profile-before"
        mkdir "$trace"
        printf '%s\n' "$trace" > "$out/instance-owned"
        printf '0\n' > "$trace/tracing_on"
        printf '0\n' > "$trace/events/enable"
        printf 'nop\n' > "$trace/current_tracer"
        printf 'mono\n' > "$trace/trace_clock"
        printf '4096\n' > "$trace/buffer_size_kb"
        for option in overwrite stacktrace userstacktrace event-fork; do printf '0\n' > "$trace/options/$option"; done
        for option in context-info markers; do printf '1\n' > "$trace/options/$option"; done
        : > "$out/registered-kprobes"
        while read -r kind name symbol fetch; do
            printf '%s:%s/%s %s %s\n' "$kind" "$group" "$name" "$symbol" "$fetch" >> "$base/kprobe_events"
            group_definitions > "$out/registered-kprobes"
            cat "$trace/events/$group/$name/format" > "$out/format-$name"
            printf '1\n' > "$trace/events/$group/$name/enable"
        done < "$out/probes.table"
        for event in $events; do printf '1\n' > "$trace/events/$event/enable"; done
        [ "$(cat "$trace/current_tracer")" = nop ] || fail 'The instance tracer is not nop.'
        case $(cat "$trace/trace_clock") in *'[mono]'*) ;; *) fail 'The mono trace clock was not selected.' ;; esac
        cat "$trace/buffer_size_kb" > "$out/buffer-size-kb"
        cat "$trace/buffer_total_size_kb" > "$out/buffer-total-size-kb"
        cat "$trace/trace_options" > "$out/trace-options"
        cat "$base/kprobe_profile" > "$out/profile-armed"
        printf '1\n' > "$trace/tracing_on"
        printf 'stacktrace:128\n' > "$trace/events/$group/unlock/trigger"
        cat "$trace/events/$group/unlock/trigger" > "$out/stack-trigger-armed"
        awk '/^stacktrace:/ {n++} END {exit n!=1}' "$out/stack-trigger-armed" || fail 'The bounded unlock stack trigger was not installed.'
        printf 'ARM %s\n' "$token" > "$trace/trace_marker"
        printf '%s\t%s\n' "$(date -u +%s)" "$(cat /proc/uptime)" > "$out/prepared"
        ;;
    stop)
        [ -f "$out/instance-owned" ] && [ -d "$trace" ] || fail 'The owned trace instance is missing.'
        stop_instance
        cat "$base/kprobe_profile" > "$out/profile-stopped"
        printf '%s\t%s\n' "$(date -u +%s)" "$(cat /proc/uptime)" > "$out/stopped"
        ;;
    export)
        [ -f "$out/instance-owned" ] && [ -d "$trace" ] || fail 'The owned trace instance is missing.'
        [ "$(cat "$trace/tracing_on")" = 0 ] && [ "$(cat "$trace/events/enable")" = 0 ] || fail 'Stop the capture before export.'
        mkdir "$out/export"
        for cpu in "$trace"/per_cpu/cpu*; do
            name=${cpu##*/}
            cat "$cpu/stats" > "$out/export/$name.stats-before"
        done
        cat "$trace/trace" > "$out/export/trace.txt"
        cat "$base/kprobe_profile" > "$out/export/kprobe_profile"
        group_definitions > "$out/export/kprobe_events"
        for cpu in "$trace"/per_cpu/cpu*; do
            name=${cpu##*/}
            cat "$cpu/stats" > "$out/export/$name.stats-after"
        done
        for file in "$out/export"/*; do printf '%s\n' "${file##*/}"; done > "$out/export-files"
        printf '%s\n' complete > "$out/export-complete"
        ;;
    cleanup)
        if [ -f "$out/prepared" ]; then [ -f "$out/export-complete" ] || fail 'Export the completed capture before cleanup.'; fi
        stop_instance
        if [ -f "$out/instance-owned" ] && [ -d "$trace" ]; then rmdir "$trace"; fi
        group_definitions > "$out/cleanup-current-kprobes"
        if [ -s "$out/cleanup-current-kprobes" ]; then
            # Reject added or changed definitions in this group before deleting any probe.
            awk 'FILENAME==ARGV[1] {owned[$0]=1; next} !($0 in owned) {exit 1}' "$out/registered-kprobes" "$out/cleanup-current-kprobes" || fail 'The probe group contains unowned definitions.'
            while read -r first rest; do printf '%s\n' "-:${first#*:}" >> "$base/kprobe_events"; done < "$out/cleanup-current-kprobes"
        fi
        [ ! -e "$trace" ] && [ -z "$(group_definitions)" ] || fail 'Owned tracing resources remain.'
        printf '%s\n' complete > "$out/cleanup-complete"
        ;;
esac
printf '%s complete; evidence: %s\n' "$action" "$out"

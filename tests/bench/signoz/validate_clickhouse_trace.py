#!/usr/bin/env python3
"""Usage: validate_clickhouse_trace.py CASE_DIR on|off EXPECTED_HELPER_SHA256. Read files only."""
import gzip
import hashlib
import json
import re
import sys
from collections import Counter
from pathlib import Path

EVENTS = "sched/sched_switch sched/sched_waking sched/sched_wakeup sched/sched_wakeup_new sched/sched_migrate_task sched/sched_process_fork sched/sched_process_exec sched/sched_process_exit signal/signal_generate signal/signal_deliver syscalls/sys_enter_futex syscalls/sys_exit_futex syscalls/sys_enter_rt_sigreturn syscalls/sys_enter_restart_syscall syscalls/sys_exit_restart_syscall".split()
EVENTS += ["sched/sched_stat_runtime", "lock/contention_begin", "lock/contention_end"]
EVENT_NAMES = {event.split("/")[1] for event in EVENTS} | {"tracing_mark_write"}
SETTINGS = {"global_profiler_real_time_period_ns": 10000000000, "global_profiler_cpu_time_period_ns": 10000000000, "background_schedule_pool_size": 512}
CONFIG_SHA = "38112631033bd2037a1c8c6207e8b8f0a018d814074e57a621c98cf023dee10d"
COUNTERS = {"entries", "overrun", "commit overrun", "bytes", "dropped events", "read events"}
REPORT = {"status": "FAIL", "limits": ["Trace timestamps use guest mono. Driver timestamps use host monotonic; the clocks are not subtracted.", "Guest scheduling events do not establish host physical CPU execution.", "Identity snapshots bracket the window and can miss threads born and exited between snapshots.", "The config hash verifies kernel configuration, not the complete running kernel binary."]}

def check(condition, message):
    if not condition:
        raise ValueError(message)

def text(path):
    return path.read_text().strip()

def document(path):
    return json.loads(text(path))

def stats(path):
    fields = dict(line.split(":", 1) for line in text(path).splitlines())
    check(set(fields) == COUNTERS | {"oldest event ts", "now ts"}, f"Invalid stats fields: {path}")
    return {key: int(value) if key in COUNTERS else float(value) for key, value in fields.items()}

def validate(root, mode, script_sha):
    capture = root / "kernel-trace"
    REPORT.update(case=str(root), mode=mode)
    observer, resources = document(root / "observer.json"), document(root / "resources.json")
    check(mode in {"on", "off"} and observer["kind"] == "kernel-events" and observer["mode"] == text(capture / "mode") == mode, "Observer mode mismatch")
    check(observer["instance"] == resources["prefix"] == text(capture / "instance") and observer["container"] == resources["tracer"], "Trace ownership mismatch")
    check(hashlib.sha256((root / "trace-clickhouse.sh").read_bytes()).hexdigest() == observer["script_sha256"] == script_sha, "Helper SHA256 mismatch")
    check(not (capture / "failures.tsv").exists(), "Helper recorded a failed operation")
    check(float(text(capture / "export-complete")) > 0, "Export is incomplete")
    records = [json.loads(line) for line in text(root / "events.jsonl").splitlines()]
    check([r["monotonic"] for r in records] == sorted(r["monotonic"] for r in records), "Host event order regressed")
    def one(kind):
        matches = [r for r in records if r["event"] == kind]
        check(len(matches) == 1, f"Expected one {kind} event; found {len(matches)}")
        return matches[0]
    check(one("observation_complete")["elapsed_seconds"] >= 300 and not one("cleanup")["errors"], "Application observation or cleanup is incomplete")
    requests = [r for r in records if r["event"] == "trace_control_requested"]
    for request in requests:
        returns = [r for r in records if r["event"] == "trace_control_returned" and r["label"] == request["label"]]
        check(len(returns) == 1 and returns[0]["monotonic"] >= request["monotonic"] and document(root / (request["label"] + ".json")).get("returncode") == 0, f"Failed trace command: {request['label']}")
        request["returned"] = returns[0]["monotonic"]
    check(Counter(r["action"] for r in requests) == {"prepare": 1, "identity": 2, "freeze": 1, "export": 1}, "Incomplete trace lifecycle")
    prepare, freeze, export = [next(r for r in requests if r["action"] == action) for action in ("prepare", "freeze", "export")]
    identities = {r["arguments"][0]: r for r in requests if r["action"] == "identity"}
    check(set(identities) == {"before", "after"} and identities["before"]["arguments"][1] == identities["after"]["arguments"][1], "Identity request mismatch")
    check(prepare["returned"] <= one("clickhouse_start_requested")["monotonic"] and identities["before"]["returned"] <= freeze["monotonic"] <= freeze["returned"] <= identities["after"]["monotonic"] <= identities["after"]["returned"] <= export["monotonic"], "Capture lifecycle order mismatch")
    stops = [r for r in records if r["event"] == "container_stop_requested"]
    check(len(stops) == 3 and {r["container"] for r in stops} == set(resources["containers"].values()) | {resources["tracer"]} and export["returned"] <= min(r["monotonic"] for r in stops), "Export did not precede all container stop requests")
    final = {name: document(root / (name + "-final.json")) for name in set(resources["containers"].values()) | {resources["tracer"]}}
    check(all(state["State"]["Running"] is False for state in final.values()), "A retained container is still running")
    for config in (document(root / "ch-create.json"), final[resources["containers"]["ch"]]):
        capabilities = {cap.upper().removeprefix("CAP_") for cap in config["HostConfig"].get("CapAdd") or []}
        check(not capabilities & {"SYS_PTRACE", "SYS_NICE"}, "Trace-mode ClickHouse gained observer capabilities")
    REPORT["final_containers_stopped"] = sorted(final)
    scheduled_events = [r for r in records if r["event"] == "contention_scheduled"]
    check(resources["case"] in {"control", "contention", "startup-contention"}, "Unknown application case")
    if resources["case"] == "control":
        check(not scheduled_events and not (root / "contention.json").exists(), "Control case contains contention")
        REPORT["contention"] = None
    else:
        contention = document(root / "contention.json")
        pids, codes = contention["pids"], contention["returncodes"]
        check(len(pids) == len(set(pids)) == 36 and all(type(pid) is int and pid > 0 for pid in pids), "Expected 36 distinct contention worker PIDs")
        check(len(codes) == 36 and all(code in {-15, -9} for code in codes), "Contention workers did not terminate as expected")
        scheduled, end = contention["scheduled_start_monotonic"], contention["scheduled_end_monotonic"]
        started, ready, finished = [contention[key] for key in ("started_monotonic", "all_started_monotonic", "finished_monotonic")]
        check(end == scheduled + 60 and 0 <= started - scheduled <= 1 and started <= ready < end <= finished, "Contention timing does not match the experiment")
        scheduled_event = one("contention_scheduled")
        check(scheduled_event["workers"] == 36 and scheduled_event["pid"] not in pids, "Contention supervisor metadata mismatch")
        if resources["case"] == "contention":
            check(scheduled == one("clickhouse_started")["completed_monotonic"] + 30 and scheduled_event["start_seconds"] == 30 and scheduled_event["end_seconds"] == 90, "Contention schedule does not match ClickHouse start")
        else:
            target = one("clickhouse_start_requested")["target_monotonic"]
            check(scheduled_event["start_monotonic"] == scheduled and scheduled_event["end_monotonic"] == end and target == ready + 5 and 0 <= one("clickhouse_started")["requested_monotonic"] - target <= 1, "Startup-contention timing mismatch")
        REPORT["contention"] = {**contention, "supervisor_start_lateness_seconds": started - scheduled, "worker_start_span_seconds": ready - started, "finish_lateness_seconds": finished - end, "sigkill_workers": codes.count(-9)}
    check(document(root / "trace-copy-capture.json").get("returncode") == 0, "Capture copy failed")
    queries = [r for r in records if r["event"] == "query"]
    check([q["number"] for q in queries] == list(range(len(queries))), "Query sequence is incomplete")
    for query in queries:
        command = document(root / f"query-{query['number']:03}.json")
        check(all(command.get(k) == query.get(k) for k in ("returncode", "timeout_seconds", "seconds")) and (command.get("returncode") != 0 or command["stdout"].strip() == "1"), f"Query record mismatch: {query['number']}")
    failures = [r for r in queries if r.get("returncode") != 0]
    reason = f"query-{failures[0]['number']:03}" if failures else "finalize"
    check(queries and freeze["arguments"] == [reason] and text(capture / "freeze-reason") == reason, "Freeze reason does not match the first query failure")
    trigger = failures[0]["monotonic"] if failures else one("observation_complete")["monotonic"]
    check(trigger <= freeze["monotonic"], "Freeze preceded its host trigger")
    REPORT["host_timing"] = {"freeze_requested": freeze["monotonic"], "freeze_returned": freeze["returned"], "trigger_to_request_seconds": freeze["monotonic"] - trigger, "query_failures": failures}
    runtime = document(root / "runtime-settings.json")
    values = [json.loads(line) for line in runtime["stdout"].splitlines()]
    check(runtime.get("returncode") == 0 and len(values) == 3 and {v["name"]: int(v["value"]) for v in values} == SETTINGS, "Runtime settings mismatch")
    REPORT["runtime_settings"] = SETTINGS
    check(hashlib.sha256(gzip.decompress((capture / "kernel.config.gz").read_bytes())).hexdigest() == CONFIG_SHA, "Runtime kernel config SHA256 mismatch")
    check(re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", text(capture / "boot_id")) and int(text(capture / "user_hz")) == 100, "Invalid guest boot identity or USER_HZ")
    check(all(text(capture / name) == "0-17" for name in ("online", "possible", "export/online")), "Expected guest CPUs 0-17")
    check(text(capture / "tracer-pid-namespace") == text(capture / "guest-pid1-namespace"), "Sidecar does not share the guest PID namespace")
    sizing = {k: int(v) for k, v in (line.split("\t") for line in text(capture / "buffer-sizing.tsv").splitlines())}
    payload = int(re.search(r"field: char data;.*?size:\s*(\d+);", text(capture / "header_page")).group(1))
    blocks = max(2, (32768 * 1024 + payload - 1) // payload)
    capacity = blocks * payload // 1024
    check(sizing == {"requested_kb": 32768, "payload_bytes": payload, "blocks": blocks, "expected_per_cpu_kb": capacity, **{f"cpu{cpu}_kb": capacity for cpu in range(18)}}, "Buffer rounding mismatch")
    for phase in ("initial", "export"):
        folder = capture / phase
        expected = {"tracing_on": "0", "current_tracer": "nop", "buffer_size_kb": str(capacity), "buffer_total_size_kb": str(18 * capacity), "set_event_pid": "", "set_event_notrace_pid": "", "events-enable": "X" if phase == "initial" and mode == "on" else "0"}
        check(all(text(folder / k) == v for k, v in expected.items()) and "[mono]" in text(folder / "trace_clock") and int(text(folder / "tracing_cpumask").replace(",", ""), 16) == 0x3FFFF, f"Invalid {phase} configuration")
        check(payload < int(text(folder / "buffer_subbuf_size_kb")) * 1024, "Invalid buffer payload size")
        check(set("overwrite markers context-info nostacktrace nouserstacktrace noevent-fork noverbose noraw nohex nobin nolatency-format".split()) <= set(text(folder / "trace_options").split()), f"Unsafe {phase} trace options")
        for event in EVENTS:
            path = folder / event.replace("/", "-")
            check(text(path / "enable") == ("1" if phase == "initial" and mode == "on" else "0") and text(path / "filter") == "none", f"Invalid event settings: {path}")
            check(all(line.startswith("#") for line in text(path / "trigger").splitlines()) and text(path / "format") == text(capture / "initial" / event.replace("/", "-") / "format") and text(path / "format"), f"Invalid event format or trigger: {path}")
    identity = {}
    for label in ("before", "after"):
        folder = capture / f"identity-{label}"
        check(text(folder / "completeness") == "complete" and not text(folder / "errors") and text(folder / "maps") and text(folder / "cgroup"), f"Incomplete {label} identity")
        pid = int(identities[label]["arguments"][1])
        tasks = {}
        for task in folder.iterdir():
            if not task.name.isdecimal():
                continue
            status = dict(line.split(":", 1) for line in text(task / "status").splitlines())
            stat, tid = text(task / "stat"), int(task.name)
            nspid = list(map(int, status["NSpid"].split()))
            check(int(status["Pid"]) == int(stat.split(" ", 1)[0]) == nspid[0] == tid and int(status["Tgid"]) == pid and len(nspid) >= 2 and text(task / "ns-pid") == text(folder / str(pid) / "ns-pid"), f"Invalid identity for {label} TID {tid}")
            tasks[tid] = {"name": status["Name"].strip(), "nspid": nspid, "starttime": int(stat[stat.rfind(")") + 2:].split()[19])}
        check(pid in tasks and tasks[pid]["nspid"][-1] == 1 and tasks[pid]["starttime"] > 0, f"Missing container PID 1 in {label}")
        identity[label] = tasks
    check(identity["before"][pid] == identity["after"][pid], "ClickHouse process identity changed")
    REPORT["identity"] = {k: {"threads": len(v), "common_tids": [tid for tid, task in v.items() if task["name"] == "Common"]} for k, v in identity.items()}
    control = [line.split("\t", 3) for line in text(capture / "control.tsv").splitlines()]
    check(all(len(row) == 4 for row in control), "Malformed control log")
    expected_markers = [f"ARM {resources['prefix']} {mode}", f"IDENTITY_BEGIN before {pid}", f"IDENTITY_END before {pid}", "WINDOW_BEGIN", "END " + reason, "FROZEN " + reason, f"IDENTITY_BEGIN after {pid}", f"IDENTITY_END after {pid}", "EXPORTED"]
    check([row[3] for row in control] == expected_markers and [row[1] for row in control] == ["1"] * 5 + ["0"] * 4, "Control markers are incomplete, out of order, or record after freeze")
    counters, headers, markers, event_counts = [], [], [], Counter()
    REPORT["cpus"] = counters
    for cpu in range(18):
        before, after = [stats(capture / "export" / f"cpu{cpu}.stats-{phase}") for phase in ("before", "after")]
        check(all(before[k] == after[k] for k in before if k != "now ts") and all(before[k] >= 0 for k in COUNTERS) and after["now ts"] >= before["now ts"] and not any(before[k] for k in ("commit overrun", "dropped events", "read events")), f"CPU {cpu}: loss, consumer, or changing frozen buffer")
        initial = stats(capture / "initial" / f"cpu{cpu}.stats")
        check(not any(initial[k] for k in COUNTERS), f"CPU {cpu}: initial buffer is not empty")
        content = text(capture / "export" / f"cpu{cpu}.trace")
        header = re.search(r"entries-in-buffer/entries-written: (\d+)/(\d+)\s+#P:(\d+)", content)
        check(header is not None and "# tracer: nop" in content, f"CPU {cpu}: missing trace header")
        headers.append(tuple(map(int, header.groups())))
        times = []
        for line in content.splitlines():
            if not line or line.startswith("#"):
                continue
            match = re.search(r"\[(\d+)\]\s+\S+\s+(\d+\.\d+):\s+(.*)$", line)
            check(match is not None and int(match[1]) == cpu, f"CPU {cpu}: unparsed trace line: {line[:160]}")
            stamp, payload = float(match[2]), match[3]
            # Linux trace_syscalls.c prints syscall entry and exit without the event name.
            syscall = re.fullmatch(r"sys_(\w+)(?:\((.*)\)| -> (0x[0-9a-f]+))", payload)
            if syscall is not None:
                direction = "enter" if syscall[2] is not None else "exit"
                event = f"sys_{direction}_{syscall[1]}"
                detail = syscall[2] if direction == "enter" else syscall[3]
            else:
                parsed = re.fullmatch(r"(\w+):\s*(.*)", payload)
                check(parsed is not None, f"CPU {cpu}: unparsed event payload: {payload[:160]}")
                event, detail = parsed.groups()
            check(event in EVENT_NAMES and (mode == "on" or event == "tracing_mark_write"), f"Unexpected event: {event}")
            times.append(stamp)
            event_counts[event] += 1
            if event == "tracing_mark_write":
                markers.append((stamp, detail))
        check(len(times) == before["entries"] and times == sorted(times), f"CPU {cpu}: event count or timestamp order mismatch")
        counters.append({"cpu": cpu, **before, "first_guest_mono": min(times) if times else None, "last_guest_mono": max(times) if times else None})
    totals = (sum(c["entries"] for c in counters), sum(c["entries"] + c["overrun"] for c in counters), 18)
    check(all(header == totals for header in headers), "Trace headers disagree with all-CPU stats totals")
    bounds = [[t for t, label in markers if label == expected] for expected in ("WINDOW_BEGIN", "END " + reason)]
    check(all(len(t) == 1 for t in bounds) and bounds[0][0] < bounds[1][0], "Window markers were lost or malformed")
    REPORT.update(guest_window_mono=[bounds[0][0], bounds[1][0]], event_counts=dict(event_counts), merged_marker_order=sorted(markers))
    check(all(not c["overrun"] or c["first_guest_mono"] <= bounds[0][0] for c in counters), "Overwritten events leave an incomplete analysis window")
    check(mode == "off" or event_counts["sched_switch"] > 0, "Events-on capture contains no scheduling events")
    REPORT["status"] = "PASS"

if __name__ == "__main__":
    try:
        check(len(sys.argv) == 4, __doc__)
        validate(Path(sys.argv[1]).resolve(), sys.argv[2], sys.argv[3])
    except Exception as error:
        REPORT["error"] = f"{type(error).__name__}: {error}"
    print(json.dumps(REPORT, indent=2))
    sys.exit(0 if REPORT["status"] == "PASS" else 1)

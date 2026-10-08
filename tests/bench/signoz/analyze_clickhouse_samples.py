"""Validate and summarize recorded ClickHouse samples without accessing live processes.

Usage: python3 analyze_clickhouse_samples.py CASE_DIRECTORY OUTPUT_JSON
"""

import collections
import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import sys


SAMPLE = re.compile(r"^sample_begin\t(\d+)\t([^\t]+)\t([^\n]+)\n(.*?)^sample_end\t\1\t([^\t]+)\t([^\n]+)\n", re.M | re.S)
THREAD = re.compile(r"^thread\t(\d+)\t(\w+)\n(.*?)(?=^thread\t|^thread_counts\t|\Z)", re.M | re.S)
COND_SIGNAL = {0x7EED4: "G1_reference_wait", 0x7EED8: "G1_reference_wait", 0x7F070: "internal_cond_lock", 0x7F074: "internal_cond_lock", 0x7EDB4: "signal_wake", 0x7EDB8: "signal_wake", 0x7EF28: "signal_wake", 0x7EF2C: "signal_wake"}
LIBC_HASH = "b8b0c12b8010d68f657287963645038257832504d3567d95326b6f98846965df"
THREAD_FIELDS = {
    "State:": r"[A-Za-z] \([^\n]+\)",
    "Uid:": r"\d+[ \t]+\d+[ \t]+\d+[ \t]+\d+",
    "SigPnd:": r"[0-9a-fA-F]+",
    "SigBlk:": r"[0-9a-fA-F]+",
    "Cpus_allowed_list:": r"[0-9,-]+",
    "voluntary_ctxt_switches:": r"\d+",
    "nonvoluntary_ctxt_switches:": r"\d+",
    "schedstat": r"\d+ \d+ \d+",
    "syscall": r"running|-1(?: 0x[0-9a-fA-F]+){2}|\d+(?: 0x[0-9a-fA-F]+){8}",
    "wchan": r"\S+",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def single_match(pattern, text, context):
    matches = list(re.finditer(pattern, text, re.M))
    require(len(matches) == 1, f"{context}: expected exactly one matching record.")
    return matches[0]


def distribution(values):
    if not values:
        return None
    ordered = sorted(values)
    return {"min": ordered[0], "median": statistics.median(ordered), "p95": ordered[int(.95 * (len(ordered) - 1))], "max": ordered[-1]}


def threads(body, *, complete=False):
    result = []
    for tid, name, fields in THREAD.findall(body):
        values = {}
        for key, pattern in THREAD_FIELDS.items():
            found = re.findall(rf"^{re.escape(key)}\t({pattern})$", fields, re.M)
            if len(found) != 1:
                require(not complete, f"Thread {tid}: missing, duplicate, or invalid {key} field.")
                break
            values[key] = found[0]
        else:
            require(name in ("Common", "BgSchPool"), f"Unexpected thread name: {name}.")
            if complete:
                require(len(fields.strip().splitlines()) == len(THREAD_FIELDS), f"Thread {tid}: unexpected field records.")
            result.append({"tid": int(tid), "name": name, "state": values["State:"][0],
                           "schedstat": [int(value) for value in values["schedstat"].split()],
                           "syscall": values["syscall"], "wchan": values["wchan"]})
    if complete:
        require(len(result) == len(re.findall(r"^thread\t", body, re.M)), "Invalid thread header in a complete sample.")
        require(len({row["tid"] for row in result}) == len(result), "Duplicate TID in a complete sample.")
    return result


def read_samples(raw):
    matches = list(SAMPLE.finditer(raw))
    require(matches, "The evidence has no complete samples.")
    samples = []
    cursor = 0
    seen_threads = {}
    for expected_number, match in enumerate(matches):
        number, begin, uptime_begin, body, end, uptime_end = match.groups()
        require(match.start() == cursor, f"Sample {expected_number}: skipped or malformed complete sample.")
        require(int(number) == expected_number, f"Sample sequence expected {expected_number}, got {number}.")
        require("sample_begin\t" not in body and "sample_end\t" not in body, f"Sample {number}: nested sample markers.")
        observer = single_match(r"^observer\t(\d+)\t(\d+) (\d+) (\d+)$", body, f"Sample {number} observer")
        counts = single_match(r"^thread_counts\tCommon\t(\d+)\tBgSchPool\t(\d+)$", body, f"Sample {number} counts")
        footer = body[body.index("thread_counts\t"):]
        require(re.fullmatch(r"thread_counts\tCommon\t\d+\tBgSchPool\t\d+\nobserver\t\d+\t\d+ \d+ \d+\n", footer), f"Sample {number}: malformed or duplicate footer records.")
        rows = threads(body, complete=True)
        actual_counts = collections.Counter(row["name"] for row in rows)
        require([actual_counts["Common"], actual_counts["BgSchPool"]] == [int(counts[1]), int(counts[2])], f"Sample {number}: declared thread counts do not match records.")
        require(int(counts[1]) > 0 and int(counts[2]) > 0, f"Sample {number}: required thread group is empty.")
        require(body.startswith("cgroup_cpu\t"), f"Sample {number}: missing cgroup CPU record.")
        cpu_pairs = [line.split() for line in body[:body.index("thread\t")].removeprefix("cgroup_cpu\t").splitlines()]
        require(all(len(pair) == 2 and pair[1].isdigit() for pair in cpu_pairs), f"Sample {number}: malformed cgroup CPU counter.")
        cpu = {key: int(value) for key, value in cpu_pairs}
        require(len(cpu) == len(cpu_pairs) and "usage_usec" in cpu, f"Sample {number}: duplicate or missing cgroup CPU counters.")
        begin, end = float(begin), float(end)
        uptime_begin, uptime_end = [float(value) for value in uptime_begin.split()], [float(value) for value in uptime_end.split()]
        require(len(uptime_begin) == len(uptime_end) == 2, f"Sample {number}: uptime must contain two numbers.")
        require(all(math.isfinite(value) and value >= 0 for value in (begin, end, *uptime_begin, *uptime_end)), f"Sample {number}: invalid time value.")
        sample = {"number": int(number), "begin": begin, "end": end, "uptime_begin": uptime_begin[0], "uptime_end": uptime_end[0], "observer_pid": int(observer[1]), "observer": [int(observer[i]) for i in range(2, 5)], "counts": [int(counts[1]), int(counts[2])], "cpu": cpu, "threads": rows}
        require(sample["begin"] <= sample["end"] and sample["uptime_begin"] <= sample["uptime_end"], f"Sample {number}: time moved backwards within the sweep.")
        if samples:
            previous = samples[-1]
            require(sample["begin"] >= previous["end"] and sample["uptime_begin"] >= previous["uptime_end"], f"Sample {number}: sample times overlap or move backwards.")
            require(sample["observer_pid"] == previous["observer_pid"], f"Sample {number}: observer PID changed.")
            require(all(new >= old for new, old in zip(sample["observer"], previous["observer"])), f"Sample {number}: observer counter decreased.")
            require(cpu.keys() == previous["cpu"].keys(), f"Sample {number}: cgroup counter fields changed.")
            require(all(cpu[key] >= value for key, value in previous["cpu"].items()), f"Sample {number}: cgroup counter decreased.")
        for row in rows:
            previous = seen_threads.get(row["tid"])
            if previous is not None:
                require(previous["name"] == row["name"], f"Sample {number}: thread {row['tid']} changed name.")
                require(all(new >= old for new, old in zip(row["schedstat"], previous["schedstat"])), f"Sample {number}: thread {row['tid']} schedstat counter decreased.")
            seen_threads[row["tid"]] = row
        samples.append(sample)
        cursor = match.end()
    tail = raw[cursor:]
    if tail:
        require(tail.startswith(f"sample_begin\t{len(samples)}\t"), "The incomplete tail does not follow the sample sequence.")
        require(len(re.findall(r"^sample_begin\t", tail, re.M)) == 1 and not re.search(r"^sample_end\t", tail, re.M), "Only the final incomplete sweep may omit fields.")
    return matches, samples


def summarize(case):
    source = case / "proc-samples.txt"
    raw = source.read_text()
    matches, samples = read_samples(raw)
    events = [json.loads(line) for line in (case / "events.jsonl").read_text().splitlines()]
    queries = [event for event in events if event["event"] == "query"]
    complete = next((event for event in events if event["event"] == "observation_complete"), None)
    complete_epoch = datetime.datetime.fromisoformat(complete["utc"]).timestamp() if complete else None
    maps = json.loads((case / "identity.json").read_text())["stdout"]
    mapping = next(line.split() for line in maps.splitlines() if "libc.so.6" in line and "r-xp" in line)
    map_begin, map_end = (int(value, 16) for value in mapping[0].split("-"))
    map_offset = int(mapping[2], 16)
    actual_hash = json.loads((case / "libc-hash.json").read_text())["stdout"].split()[0]
    if actual_hash != LIBC_HASH:
        raise ValueError("The libc hash does not match the retained instruction offsets.")
    require(map_offset == 0, "The retained libc PC classification requires executable mapping file offset zero.")

    def syscall_summary(rows):
        counts = collections.Counter()
        for row in rows:
            fields = row["syscall"].split()
            number = fields[0]
            if number == "98":
                operation = int(fields[2], 16)
                pc = int(fields[-1], 16)
                offset = map_offset + pc - map_begin if map_begin <= pc < map_end else None
                category = COND_SIGNAL.get(offset, "other_libc_futex" if offset is not None else "outside_libc_exec_mapping")
                command = operation & ~0x180
                expected = int(fields[3], 16)
                if category in ("G1_reference_wait", "internal_cond_lock"):
                    valid_value = expected >= 3 and expected % 2 == 1 if category == "G1_reference_wait" else expected & 3 == 2
                    if command != 0 or not valid_value:
                        category = "other_libc_futex"
                elif category == "signal_wake" and command != 1:
                    category = "other_libc_futex"
                counts[(row["name"], number, hex(operation), command, hex(offset) if offset is not None else None, category)] += 1
            else:
                counts[(row["name"], number, None, None, None, "not_futex_snapshot")] += 1
        return [{"thread_name": key[0], "syscall": key[1], "futex_op": key[2], "futex_command": key[3], "libc_pc_file_offset": key[4], "category": key[5], "count": count} for key, count in counts.most_common()]

    by_tid = collections.defaultdict(list)
    for sample in samples:
        for row in sample["threads"]:
            by_tid[row["tid"]].append({**row, "sample": sample["number"], "sweep_begin": sample["begin"], "sweep_end": sample["end"]})
    runnable = [row for rows in by_tid.values() for row in rows if row["state"] == "R"]
    intervals = []
    for rows in by_tid.values():
        chain = []
        for row in rows + [None]:
            same = chain and row is not None and row["state"] == "R" and row["sample"] == chain[-1]["sample"] + 1 and row["schedstat"][0::2] == chain[-1]["schedstat"][0::2]
            if not same:
                if len(chain) >= 2:
                    intervals.append({"tid": chain[0]["tid"], "name": chain[0]["name"], "samples": [item["sample"] for item in chain], "runtime_ns": chain[0]["schedstat"][0], "dispatch_count": chain[0]["schedstat"][2], "endpoint_time_distance_bounds_seconds": [max(0, chain[-1]["sweep_begin"] - chain[0]["sweep_end"]), chain[-1]["sweep_end"] - chain[0]["sweep_begin"]]})
                chain = []
            if row is not None and row["state"] == "R":
                chain.append(row)
    observer_delta = [samples[-1]["observer"][i] - samples[0]["observer"][i] for i in range(3)]
    cpu_delta = {key: samples[-1]["cpu"][key] - value for key, value in samples[0]["cpu"].items()}
    gaps = [{"after_sample": before["number"], "seconds": after["begin"] - before["end"], "uptime_seconds": after["uptime_begin"] - before["uptime_end"]} for before, after in zip(samples, samples[1:])]
    tail = raw[matches[-1].end():]
    partial = re.search(r"^sample_begin\t(\d+)\t([^\t]+)\t", tail, re.M)
    all_rows = [row for sample in samples for row in sample["threads"]]
    slow_windows = []
    for index, sample in enumerate(samples):
        if sample["end"] - sample["begin"] <= 2:
            continue
        window = []
        for current in range(max(0, index - 1), min(len(samples), index + 2)):
            item = samples[current]
            previous = samples[current - 1] if current else item
            previous_threads = {row["tid"]: row for row in previous["threads"]}
            deltas = [{"tid": row["tid"], "name": row["name"], "state": row["state"], "schedstat": row["schedstat"], "delta_from_previous_sweep": [row["schedstat"][i] - previous_threads[row["tid"]]["schedstat"][i] for i in range(3)], "syscall": row["syscall"]} for row in item["threads"] if row["name"] == "Common"]
            window.append({"sample": item["number"], "begin_utc": datetime.datetime.fromtimestamp(item["begin"], datetime.timezone.utc).isoformat(), "end_utc": datetime.datetime.fromtimestamp(item["end"], datetime.timezone.utc).isoformat(), "observer_delta_from_previous_sweep": [item["observer"][i] - previous["observer"][i] for i in range(3)], "cgroup_cpu_delta_from_previous_sweep": {key: item["cpu"][key] - value for key, value in previous["cpu"].items()}, "common_threads": deltas})
        slow_windows.append(window)
    timeout_details = []
    for query in queries:
        if query["returncode"] == 0:
            continue
        path = case / f"query-{query['number']:03}.json"
        command_result = json.loads(path.read_text())
        timeout_details.append({"file": str(path), **{key: value for key, value in command_result.items() if key != "command"}})
    return {
        "case": str(case), "source_bytes": len(raw.encode()), "source_sha256": hashlib.sha256(raw.encode()).hexdigest(),
        "libc_hash": actual_hash, "libc_exec_mapping": mapping,
        "complete_samples": len(samples), "first_sample": samples[0]["number"], "last_sample": samples[-1]["number"],
        "covered_wall_seconds": samples[-1]["end"] - samples[0]["begin"], "thread_records": len(all_rows), "unique_tids": len(by_tid),
        "per_sample_thread_counts": dict(collections.Counter(f"Common={sample['counts'][0]},BgSchPool={sample['counts'][1]}" for sample in samples)),
        "states": dict(collections.Counter(row["name"] + ":" + row["state"] for row in all_rows)),
        "sweep_seconds": distribution([sample["end"] - sample["begin"] for sample in samples]),
        "sample_begin_spacing_seconds": distribution([after["begin"] - before["begin"] for before, after in zip(samples, samples[1:])]),
        "between_sweeps_seconds": distribution([gap["seconds"] for gap in gaps]), "largest_gaps": sorted(gaps, key=lambda gap: gap["seconds"], reverse=True)[:10],
        "sweeps_above_two_seconds_with_neighbors": slow_windows,
        "observer_pids": sorted({sample["observer_pid"] for sample in samples}), "observer_schedstat_delta": observer_delta, "cgroup_cpu_delta": cpu_delta,
        "counter_read_positions": {"observer_schedstat": "After all thread reads, immediately before sample_end.", "cgroup_cpu": "Before sample_begin and all thread reads."},
        "observer_runtime_fraction_of_cgroup_cpu": observer_delta[0] / (1000 * cpu_delta["usage_usec"]),
        "syscall_classification": syscall_summary(all_rows), "runnable_status_rows": runnable,
        "repeated_R_with_unchanged_runtime_dispatch": intervals,
        "queries": {"count": len(queries), "errors": [query for query in queries if query["returncode"] != 0], "error_command_results": timeout_details, "host_duration_seconds": distribution([query["seconds"] for query in queries])},
        "observation_complete": complete, "cleanup": next((event for event in events if event["event"] == "cleanup"), None),
        "incomplete_tail": {"bytes": len(tail.encode()), "sample": int(partial[1]) if partial else None, "begin_epoch": float(partial[2]) if partial else None, "begin_minus_observation_complete_seconds": float(partial[2]) - complete_epoch if partial and complete_epoch else None, "complete_thread_records": len(threads(tail)), "syscalls_separate_from_complete_samples": syscall_summary(threads(tail)), "stderr": (case / "proc-samples.stderr").read_text()},
        "limits": ["Status, schedstat, syscall, and wchan are separate reads; one row is not an atomic snapshot.", "No per-thread timestamps exist; sweep boundaries only bound each observation time.", "Repeated R observations do not prove continuous R state between observations.", "Missing internal cond-signal waits in sparse samples does not exclude short waits or waits during gaps.", "Observer runtime excludes child processes, including sleep, and transport overhead; cgroup CPU also includes queries and healthchecks.", "CPU values are guest Linux accounting; these measurements do not independently measure physical host execution time.", "schedstat run_delay accrues on scheduling arrival or dequeue; an unchanged value does not exclude an ongoing runnable wait.", "Futex syscall PCs describe saved syscall state; completion or signal restart can affect the saved PC.", "PC classification requires the recorded libc SHA256 and an executable mapping with file offset zero; other binaries or mappings are rejected.", "The incomplete final sweep is excluded from complete-sample statistics and reported separately."]
    }


if __name__ == "__main__":
    case = Path(sys.argv[1]).resolve()
    report = summarize(case)
    output = Path(sys.argv[2]).resolve()
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"output": str(output), "complete_samples": report["complete_samples"], "thread_records": report["thread_records"], "repeated_R_intervals": len(report["repeated_R_with_unchanged_runtime_dispatch"]), "query_errors": len(report["queries"]["errors"])}))

"""Compare guest CPU charge with macOS ps readings across a wider sampling window.

Usage: python3 -B compare_host_guest_cpu.py CASE HOST_JSONL NEW_OUTPUT --helper-pid PID
"""

import argparse
import datetime
import hashlib
import json
import math
from pathlib import Path
import re
import subprocess
import time

from analyze_clickhouse_samples import read_samples, require


def process_rows(record):
    require(record["returncode"] == 0, "The host process read failed.")
    times = [record[key] for key in ("monotonic_before", "monotonic_after")]
    require(all(math.isfinite(value) and value >= 0 for value in times) and times[0] <= times[1], "Invalid host read times.")
    rows = {}
    for line in record["stdout"].splitlines():
        fields = line.split(maxsplit=8)
        require(len(fields) == 9, "Invalid host process record.")
        pid = int(fields[0])
        require(pid > 0 and pid not in rows, "Invalid or duplicate host PID.")
        cpu = re.fullmatch(r"(\d+):([0-5]\d)\.(\d{2})", fields[7])
        require(cpu is not None, "Expected macOS ps CPU time in minutes:seconds.centiseconds format.")
        start = " ".join(fields[1:6])
        datetime.datetime.strptime(start, "%a %b %d %H:%M:%S %Y")
        rows[pid] = {"pid": pid, "start": start, "command": fields[8],
                     "cpu_centiseconds": int(cpu[1]) * 6000 + int(cpu[2]) * 100 + int(cpu[3])}
    require(rows, "The host process record is empty.")
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("case", type=Path)
    parser.add_argument("host_jsonl", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--helper-pid", type=int, required=True)
    args = parser.parse_args()
    case, output = args.case.resolve(), args.output.resolve()
    with args.host_jsonl.open() as source:
        first_host = json.loads(source.readline())
    first_rows = process_rows(first_host)
    require(args.helper_pid in first_rows, "The helper PID is missing from the first host record.")
    events = [json.loads(line) for line in (case / "events.jsonl").read_text().splitlines()]
    starts = [event for event in events if event["event"] == "clickhouse_start_requested"]
    require(len(starts) == 1, "Expected exactly one application start request.")
    start = starts[0]["monotonic"]
    require(math.isfinite(start) and first_host["monotonic_after"] < start, "The first host read must finish before the application start request.")

    output.mkdir()
    (output / "host-first.json").write_text(json.dumps(first_host, indent=2) + "\n")
    snapshot_before = time.monotonic()
    snapshot = (case / "proc-samples.txt").read_bytes()
    (output / "proc-samples.txt").write_bytes(snapshot)
    snapshot_after = time.monotonic()
    raw = snapshot.decode()
    matches, samples = read_samples(raw)
    parse_after = time.monotonic()
    command = ["ps", "-p", ",".join(map(str, sorted(first_rows))), "-o", "pid=,lstart=,state=,time=,command="]
    ps_before = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True, timeout=5)
    last_host = {"monotonic_before": ps_before, "monotonic_after": time.monotonic(),
                 "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                 "command": command, "stdout": result.stdout, "stderr": result.stderr,
                 "returncode": result.returncode}
    (output / "host-endpoint.json").write_text(json.dumps(last_host, indent=2) + "\n")
    result.check_returncode()
    last_rows = process_rows(last_host)
    require(set(first_rows) == set(last_rows), "The host process set changed.")
    require(start < snapshot_before <= snapshot_after <= parse_after <= ps_before, "The snapshot and host read order is invalid.")
    host_deltas = {}
    for pid, before in first_rows.items():
        after = last_rows[pid]
        require(all(before[key] == after[key] for key in ("pid", "start", "command")), f"Host process {pid} changed identity.")
        delta = after["cpu_centiseconds"] - before["cpu_centiseconds"]
        require(delta >= 0, f"Host process {pid} CPU time decreased.")
        host_deltas[pid] = {"identity": {key: after[key] for key in ("pid", "start", "command")},
                            "first_cpu_seconds": before["cpu_centiseconds"] / 100,
                            "last_cpu_seconds": after["cpu_centiseconds"] / 100,
                            "delta_seconds": delta / 100,
                            "delta_plus_display_margin_seconds": (delta + 2) / 100}
    guest_cpu = {key: samples[-1]["cpu"][key] - value for key, value in samples[0]["cpu"].items()}
    guest_charge = guest_cpu["usage_usec"] / 1e6
    helper_with_margin = host_deltas[args.helper_pid]["delta_plus_display_margin_seconds"]
    all_with_margin = sum(row["delta_plus_display_margin_seconds"] for row in host_deltas.values())
    report = {
        "case": str(case), "snapshot": str(output / "proc-samples.txt"),
        "snapshot_bytes": len(snapshot), "snapshot_sha256": hashlib.sha256(snapshot).hexdigest(),
        "integrity": "PASS: all complete sweeps passed the strict parser.",
        "complete_samples": len(samples), "thread_records": sum(len(sample["threads"]) for sample in samples),
        "first_sample": samples[0]["number"], "last_sample": samples[-1]["number"],
        "guest_cgroup_delta": guest_cpu, "guest_charge_seconds": guest_charge,
        "host_processes": host_deltas, "helper_pid": args.helper_pid,
        "host_window_wall_seconds": last_host["monotonic_after"] - first_host["monotonic_before"],
        "timeline_host_monotonic": {"first_host_read_before": first_host["monotonic_before"],
                                    "first_host_read_after": first_host["monotonic_after"],
                                    "application_start_requested": start,
                                    "snapshot_copy_before": snapshot_before, "snapshot_copy_after": snapshot_after,
                                    "strict_parse_after": parse_after, "last_host_read_before": ps_before,
                                    "last_host_read_after": last_host["monotonic_after"]},
        "causal_bracket": "The first host read finished before application start was requested. The last host read began after the guest snapshot was copied and strictly parsed. The wider host sampling window contains every guest counter read in the copied file; this comparison does not synchronize guest and host realtime clocks.",
        "ps_display_margin_seconds_per_process": 0.02,
        "helper_ps_delta_plus_display_margin_seconds": helper_with_margin,
        "guest_minus_helper_ps_with_margin_seconds": guest_charge - helper_with_margin,
        "guest_to_helper_ps_with_margin_ratio": guest_charge / helper_with_margin,
        "all_ps_deltas_plus_display_margins_seconds": all_with_margin,
        "guest_minus_all_ps_with_margin_seconds": guest_charge - all_with_margin,
        "incomplete_tail_bytes_excluded": len(raw[matches[-1].end():].encode()),
        "limits": ["macOS ps CPU values have 0.01-second precision. The 0.02-second margin covers displayed resolution only.",
                   "macOS ps adds terminated-thread and live-thread CPU counters from separate Mach reads. A thread exit between those reads can omit that thread's accumulated CPU time.",
                   "The helper counter includes the whole VM; the guest counter includes only the measured ClickHouse cgroup.",
                   "The displayed ps delta and margin do not bound physical CPU execution. Guest charge above that value cannot independently prove overcharging."]
    }
    (output / "analysis.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"output": str(output / "analysis.json"), "complete_samples": len(samples),
                      "guest_charge_seconds": guest_charge, "helper_ps_delta_plus_display_margin_seconds": helper_with_margin,
                      "all_ps_deltas_plus_display_margins_seconds": all_with_margin}))


if __name__ == "__main__":
    main()

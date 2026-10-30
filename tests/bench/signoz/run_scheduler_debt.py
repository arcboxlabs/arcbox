"""Measure scheduler delay among eight busy tasks on one disposable guest CPU."""

import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import subprocess
import sys
import time


def read_samples(path):
    complete = path.read_text().rpartition("\n")[0]
    return [json.loads(line) for line in complete.splitlines()]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--container", required=True, help="Dedicated task-owned container")
    parser.add_argument("--cpu", type=int, default=1)
    parser.add_argument("--seconds", type=int, default=120)
    parser.add_argument("--pause", action="store_true")
    parser.add_argument("--host-vm-pid", type=int)
    parser.add_argument("--expected-start")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 30 <= args.seconds <= 300:
        parser.error("--seconds must be between 30 and 300")
    if args.pause and not (args.host_vm_pid and args.expected_start):
        parser.error("--pause requires --host-vm-pid and --expected-start")
    args.output.mkdir(exist_ok=False)
    docker = ["docker", "--host", "unix://" + args.socket]
    command = docker + [
        "exec", args.container, "/tmp/thread_accounting", str(args.cpu), str(args.seconds), "busy",
    ]
    paths = [args.output / f"worker-{index}.jsonl" for index in range(8)]
    workers = []
    completed = False
    started = time.monotonic()
    with ExitStack() as streams:
        try:
            for index, path in enumerate(paths):
                stdout = streams.enter_context(path.open("x"))
                stderr = streams.enter_context((args.output / f"worker-{index}.stderr").open("x"))
                workers.append(subprocess.Popen(command, stdout=stdout, stderr=stderr))
            deadline = time.monotonic() + 20
            while True:
                samples = [read_samples(path) for path in paths]
                if all(rows and rows[-1].get("elapsed_ns", 0) >= 2e9 for rows in samples):
                    break
                if any(worker.poll() is not None for worker in workers):
                    raise RuntimeError("A worker exited before all workers became ready.")
                if time.monotonic() >= deadline:
                    raise RuntimeError("The workers did not become ready within twenty seconds.")
                time.sleep(0.1)
            (args.output / "ready.json").write_text(json.dumps(samples, indent=2) + "\n")
            snapshot = docker + [
                "exec", args.container, "cat", "/proc/uptime",
                "/sys/fs/cgroup/cpu.max", "/sys/fs/cgroup/cpu.stat",
            ]
            for rows in samples:
                pid = rows[0]["pid"]
                snapshot.extend([f"/proc/{pid}/stat", f"/proc/{pid}/schedstat"])
            if args.pause:
                subprocess.run([
                    sys.executable, str(Path(__file__).with_name("pause_vm.py")),
                    "--pid", str(args.host_vm_pid), "--expected-start", args.expected_start,
                    "--output", str(args.output / "pause.jsonl"),
                ], check=True, timeout=25)
            resumed = time.monotonic()
            deadline = time.monotonic() + args.seconds + 20
            with (args.output / "runnable.jsonl").open("x") as output:
                while any(worker.poll() is None for worker in workers):
                    if time.monotonic() >= deadline:
                        raise RuntimeError("The workers exceeded their completion deadline.")
                    # Stop snapshots before normal worker exits to keep /proc reads unambiguous.
                    if time.monotonic() - started < args.seconds - 15:
                        read_started = time.monotonic() - resumed
                        state = subprocess.check_output(snapshot, text=True, timeout=15)
                        output.write(json.dumps({
                            "read_started_s": read_started,
                            "after_ready_or_resume_s": time.monotonic() - resumed,
                            "stdout": state,
                        }) + "\n")
                        output.flush()
                    time.sleep(3)
            for worker in workers:
                code = worker.wait(timeout=max(1, deadline - time.monotonic()))
                if code:
                    raise RuntimeError(f"A worker exited with status {code}.")
            completed = True
        finally:
            if workers and not completed:
                # Stopping the Docker client alone leaves its guest process alive.
                subprocess.run(docker + ["stop", "-t", "1", args.container], check=True, timeout=20)
                for worker in workers:
                    worker.wait(timeout=15)

    summary = {"command": command, "paused": args.pause, "workers": []}
    for index, path in enumerate(paths):
        rows = read_samples(path)
        metadata, data = rows[0], rows[1:]
        if len(data) < 2 or data[-1]["elapsed_ns"] < args.seconds * 1e9:
            raise RuntimeError(f"Worker {index} did not capture the complete interval.")
        if any(row["prof_ns"] % metadata["prof_resolution_ns"] for row in data):
            raise RuntimeError(f"Worker {index} has inconsistent raw PROF resolution.")
        gaps = [
            {"from": before, "to": after, "gap_ns": after["elapsed_ns"] - before["elapsed_ns"]}
            for before, after in zip(data, data[1:])
        ]
        summary["workers"].append({
            "index": index, "metadata": metadata, "last": data[-1],
            "largest_gaps": sorted(gaps, key=lambda row: row["gap_ns"], reverse=True)[:3],
            "max_read_span_ns": max(row["read_span_ns"] for row in data),
            "sched_minus_prof_ns": data[-1]["sched_ns"] - data[-1]["prof_ns"],
        })
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    for worker in summary["workers"]:
        print(json.dumps({
            "index": worker["index"], "last": worker["last"],
            "max_gap_ns": worker["largest_gaps"][0]["gap_ns"],
            "sched_minus_prof_ns": worker["sched_minus_prof_ns"],
        }), flush=True)


if __name__ == "__main__":
    main()

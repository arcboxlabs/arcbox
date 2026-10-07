"""Compare raw task ticks and scheduler runtime in an isolated Linux VM."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--container", required=True)
    parser.add_argument("--cpu", type=int, default=1)
    parser.add_argument("--seconds", type=int, default=20)
    parser.add_argument("--mode", choices=("busy", "sleep"), required=True)
    parser.add_argument("--host-vm-pid", type=int, required=True)
    parser.add_argument("--expected-start")
    parser.add_argument("--pause", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds < 16 or args.seconds > 300:
        parser.error("--seconds must be between 16 and 300")
    if args.pause and not args.expected_start:
        parser.error("--pause requires --expected-start")
    args.output.mkdir(exist_ok=False)
    docker = ["docker", "--host", "unix://" + args.socket, "exec", args.container]

    def snapshot(name):
        result = {"utc": datetime.now(timezone.utc).isoformat()}
        for path in ("/proc/stat", "/proc/interrupts", "/sys/fs/cgroup/cpu.stat"):
            result[path] = subprocess.check_output(
                docker + ["cat", path], text=True, timeout=15,
            )
        result["host_cpu"] = subprocess.check_output(
            ["ps", "-p", str(args.host_vm_pid), "-o", "time="], text=True,
        ).strip()
        (args.output / (name + ".json")).write_text(json.dumps(result, indent=2) + "\n")

    snapshot("before")
    command = docker + ["/tmp/thread_accounting", str(args.cpu), str(args.seconds), args.mode]
    with (args.output / "samples.jsonl").open("x") as stdout, \
            (args.output / "stderr.txt").open("x") as stderr:
        worker = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        try:
            if args.pause:
                deadline = time.monotonic() + 15
                while True:
                    complete = (args.output / "samples.jsonl").read_text().rpartition("\n")[0]
                    rows = [json.loads(line) for line in complete.splitlines()]
                    if rows and rows[-1].get("elapsed_ns", 0) >= 2e9:
                        (args.output / "pause_sample.json").write_text(json.dumps(rows[-1]) + "\n")
                        break
                    if worker.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("The worker did not start before suspension.")
                    time.sleep(0.1)
                subprocess.run([
                    sys.executable, str(Path(__file__).with_name("pause_vm.py")),
                    "--pid", str(args.host_vm_pid), "--expected-start", args.expected_start,
                    "--output", str(args.output / "pause.jsonl"),
                ], check=True, timeout=25)
            code = worker.wait(timeout=args.seconds + 20)
            if code:
                raise RuntimeError(f"The worker exited with status {code}.")
        finally:
            if worker.poll() is None:
                worker.terminate()
                worker.wait(timeout=15)
    snapshot("after")
    samples = [json.loads(line) for line in (args.output / "samples.jsonl").read_text().splitlines()]
    data = samples[1:]
    if len(data) < 2 or data[-1]["elapsed_ns"] < args.seconds * 1e9:
        raise RuntimeError("The worker did not capture the complete interval.")
    if any(row["prof_ns"] % samples[0]["prof_resolution_ns"] for row in data):
        raise RuntimeError("Raw task time does not match the reported clock resolution.")
    summary = {
        "command": command, "metadata": samples[0], "last": data[-1],
        "max_read_span_ns": max(row["read_span_ns"] for row in data),
        "sched_minus_prof_ns": data[-1]["sched_ns"] - data[-1]["prof_ns"],
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()

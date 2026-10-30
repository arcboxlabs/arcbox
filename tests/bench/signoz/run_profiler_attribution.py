"""Test CPU timer attribution while a known signal handler spans a VM pause."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--container", required=True)
    parser.add_argument("--pause", action="store_true")
    parser.add_argument("--host-vm-pid", type=int)
    parser.add_argument("--expected-start")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.pause and (not args.host_vm_pid or not args.expected_start):
        parser.error("--pause requires --host-vm-pid and --expected-start")
    args.output.mkdir(exist_ok=False)
    samples = args.output / "samples.jsonl"
    command = ["docker", "--host", "unix://" + args.socket, "exec", args.container,
               "/tmp/profiler_attribution"]
    with samples.open("x") as stdout, (args.output / "stderr.txt").open("x") as stderr:
        worker = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        try:
            if args.pause:
                deadline = time.monotonic() + 15
                while '{"event":"entered"}\n' not in samples.read_text():
                    if worker.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("The handler did not report entry before suspension.")
                    time.sleep(0.01)
                subprocess.run([
                    sys.executable, str(Path(__file__).with_name("pause_vm.py")),
                    "--pid", str(args.host_vm_pid), "--expected-start", args.expected_start,
                    "--output", str(args.output / "pause.jsonl"),
                ], check=True, timeout=25)
            code = worker.wait(timeout=40)
            if code:
                raise RuntimeError(f"The probe exited with status {code}.")
        finally:
            if worker.poll() is None:
                worker.terminate()
                worker.wait(timeout=15)
    rows = [json.loads(line) for line in samples.read_text().splitlines()]
    result = next(row for row in rows if row["event"] == "result")
    expected = int(args.pause)
    matched = (result["sched_count"] == expected and result["sched_in_real"] == expected
               and result["prof_count"] == 0 and result["prof_in_real"] == 0
               and 1e9 <= result["prof_delta_ns"] < 2e9)
    summary = {"command": command, "paused": args.pause, "prediction_matched": matched,
               "result": result}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary), flush=True)
    if not matched:
        raise RuntimeError("The observed timers do not match the attribution hypothesis.")


if __name__ == "__main__":
    main()

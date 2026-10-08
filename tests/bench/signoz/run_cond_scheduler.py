"""Measure notification latency after overcharging a disposable guest's scheduler."""

import argparse
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--container", required=True, help="Dedicated task-owned container")
    parser.add_argument("--runtime", choices=["old", "modern"], required=True)
    parser.add_argument("--mode", choices=["control", "pause"], required=True)
    parser.add_argument("--host-vm-pid", type=int)
    parser.add_argument("--expected-start")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.mode == "pause" and not (args.host_vm_pid and args.expected_start):
        parser.error("--mode pause requires --host-vm-pid and --expected-start")
    args.output.mkdir(exist_ok=False)
    docker = ["docker", "--host", "unix://" + args.socket]
    command = docker + ["exec", "-i", args.container]
    if args.runtime == "modern":
        command += [
            "/tmp/libc-modern/ld-linux-aarch64.so.1", "--library-path", "/tmp/libc-modern",
        ]
    expectation = "wait" if args.mode == "pause" and args.runtime == "old" else "fast"
    command += ["/tmp/cond_scheduler_latency", args.mode, expectation]
    (args.output / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    started = time.monotonic()
    deadline = started + 170
    completed = ready = False
    pending = b""
    with (args.output / "stdout.jsonl").open("xb") as output, \
            (args.output / "stderr.txt").open("xb") as errors, \
            selectors.DefaultSelector() as selector:
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=errors)
        selector.register(process.stdout, selectors.EVENT_READ)
        try:
            while True:
                remaining = min(deadline, started + 20 if not ready else deadline) - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError("The probe exceeded its readiness or completion deadline.")
                if not selector.select(timeout=remaining):
                    raise RuntimeError("The probe produced no output before its deadline.")
                chunk = os.read(process.stdout.fileno(), 65536)
                if not chunk:
                    break
                output.write(chunk)
                output.flush()
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    row = json.loads(line)
                    if row.get("stage") == "ready":
                        if ready:
                            raise RuntimeError("The probe emitted a duplicate ready event.")
                        ready = True
                        if args.mode == "pause":
                            subprocess.run([
                                sys.executable, str(Path(__file__).with_name("pause_vm.py")),
                                "--pid", str(args.host_vm_pid),
                                "--expected-start", args.expected_start,
                                "--output", str(args.output / "pause.jsonl"),
                            ], check=True, timeout=25)
                        process.stdin.write(b"go\n")
                        process.stdin.close()
                    print(json.dumps(row), flush=True)
            code = process.wait(timeout=max(1, deadline - time.monotonic()))
            if code or not ready or pending:
                raise RuntimeError(f"The probe failed: status={code}, ready={ready}.")
            completed = True
        finally:
            if not completed:
                # Terminating docker exec alone leaves the guest process running.
                subprocess.run(docker + ["stop", "-t", "1", args.container],
                               check=True, timeout=20)
                process.wait(timeout=15)
            process.stdout.close()
            if not process.stdin.closed:
                process.stdin.close()
    (args.output / "completion.json").write_text(json.dumps({
        "exit_code": code, "elapsed_seconds": time.monotonic() - started,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()

"""Record host process CPU time independently of the guest Docker connection."""

import argparse
import datetime
import json
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pids", type=int, nargs="+", required=True)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds <= 0 or any(pid <= 0 for pid in args.pids):
        parser.error("PIDs and observation seconds must be positive.")
    expected = set(args.pids)
    command = ["ps", "-p", ",".join(map(str, args.pids)), "-o", "pid=,lstart=,state=,time=,command="]
    with args.output.open("x") as output:
        started = time.monotonic()
        while time.monotonic() - started < args.seconds:
            before = time.monotonic()
            result = subprocess.run(command, capture_output=True, text=True, timeout=5)
            record = {"monotonic_before": before, "monotonic_after": time.monotonic(),
                      "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                      "command": command, "stdout": result.stdout,
                      "stderr": result.stderr, "returncode": result.returncode}
            output.write(json.dumps(record) + "\n")
            output.flush()
            result.check_returncode()
            observed = {int(line.split()[0]) for line in result.stdout.splitlines()}
            if observed != expected:
                raise RuntimeError(f"Host process set changed: {observed}; expected {expected}.")
            time.sleep(1)


if __name__ == "__main__":
    main()

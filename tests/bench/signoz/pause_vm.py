"""Suspend an explicitly identified disposable VZ helper for ten seconds."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--expected-start", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.pid <= 1:
        parser.error("--pid must be greater than 1")
    identity = subprocess.check_output(
        ["ps", "-p", str(args.pid), "-o", "lstart=", "-o", "comm="], text=True,
    ).strip()
    expected = args.expected_start.strip() + " "
    helper = "VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine"
    if not identity.startswith(expected) or not identity.endswith(helper):
        raise RuntimeError("The disposable VM process identity does not match.")

    with args.output.open("x") as output:
        # The detached watchdog resumes the VM even if the parent exits.
        resume = subprocess.Popen(
            [sys.executable, "-c",
             "import os, signal, sys, time; time.sleep(10); os.kill(int(sys.argv[1]), signal.SIGCONT)",
             str(args.pid)],
            start_new_session=True, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            os.kill(args.pid, signal.SIGSTOP)
            state = subprocess.check_output(
                ["ps", "-p", str(args.pid), "-o", "state="], text=True,
            ).strip()
            output.write(json.dumps({
                "event": "stopped", "pid": args.pid, "identity": identity,
                "state": state, "utc": datetime.now(timezone.utc).isoformat(),
            }) + "\n")
            output.flush()
            resume.wait(timeout=20)
            if resume.returncode != 0:
                raise RuntimeError("The VM resume watchdog failed.")
        finally:
            os.kill(args.pid, signal.SIGCONT)
            output.write(json.dumps({
                "event": "resumed", "pid": args.pid,
                "utc": datetime.now(timezone.utc).isoformat(),
            }) + "\n")


if __name__ == "__main__":
    main()

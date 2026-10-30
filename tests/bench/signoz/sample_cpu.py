"""Record guest and container CPU accounting through an explicit Docker socket."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--container", required=True)
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--host-vm-pid", type=int)
    args = parser.parse_args()
    docker = ["docker", "--host", f"unix://{args.socket}"]
    command = docker + [
        "exec", "--user", "0", args.container, "cat",
        "/proc/uptime", "/proc/stat", "/sys/fs/cgroup/cpu.stat",
        "/proc/loadavg", "/proc/1/status",
    ]
    deadline = time.monotonic() + args.seconds
    samples = []
    with args.output.open("x") as output:
        while time.monotonic() < deadline:
            started = time.monotonic()
            result = subprocess.run(command, capture_output=True, text=True, timeout=30)
            record = {
                "utc": datetime.now(timezone.utc).isoformat(),
                "elapsed_seconds": time.monotonic() - started,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
            if args.host_vm_pid:
                host = subprocess.run(
                    ["ps", "-p", str(args.host_vm_pid), "-o", "time="],
                    capture_output=True, text=True, check=True, timeout=5,
                )
                record["host_vm_cpu_time"] = host.stdout.strip()
            output.write(json.dumps(record) + "\n")
            output.flush()
            result.check_returncode()
            lines = result.stdout.splitlines()
            fields = next(line.split()[1:] for line in lines if line.startswith("cpu "))
            ticks = [int(value) for value in fields[:8]]
            usage = next(int(line.split()[1]) for line in lines if line.startswith("usage_usec "))
            sample = {
                "uptime": float(lines[0].split()[0]),
                "total": sum(ticks) / 100,
                "busy": (sum(ticks) - ticks[3] - ticks[4]) / 100,
                "usage": usage / 1_000_000,
            }
            if samples:
                previous = samples[-1]
                elapsed = sample["uptime"] - previous["uptime"]
                print(json.dumps({
                    "guest_cores": (sample["total"] - previous["total"]) / elapsed,
                    "container_cores": (sample["usage"] - previous["usage"]) / elapsed,
                    "read_seconds": record["elapsed_seconds"],
                }), flush=True)
            samples.append(sample)
            time.sleep(2)

    first, last = samples[0], samples[-1]
    elapsed = last["uptime"] - first["uptime"]
    total = last["total"] - first["total"]
    busy = last["busy"] - first["busy"]
    usage = last["usage"] - first["usage"]
    summary = {
        "samples": len(samples),
        "elapsed_seconds": elapsed,
        "guest_total_cpu_seconds": total,
        "guest_busy_cpu_seconds": busy,
        "container_cpu_seconds": usage,
        "guest_cores": total / elapsed,
        "container_cores": usage / elapsed,
    }
    args.output.with_suffix(".summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()

"""Observe an isolated ClickHouse restart for 300 seconds from stopped seed volumes.

Use control, contention at start +30..90 seconds, or startup-contention with
ClickHouse starting five seconds after all 36 host workers start.
"""

import argparse
from contextlib import ExitStack
import copy
import datetime
import hashlib
import http.client
import json
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
from urllib.parse import quote
import uuid

from clickhouse_trace import KernelTrace


SECONDS = 300


def stop_on_signal(number, _frame):
    raise SystemExit(128 + number)


def terminate_owned(processes):
    for process in processes:
        if process.poll() is None:
            process.terminate()
    for process in processes:
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def publish_contention(output, result):
    pending = output.with_suffix(".pending")
    pending.write_text(json.dumps(result, indent=2) + "\n")
    pending.replace(output)


def start_contention(output, streams, scheduled):
    return subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--contention-child", str(output / "contention.json"), str(scheduled)], start_new_session=True, stdout=streams.enter_context((output / "contention.stdout").open("w")), stderr=streams.enter_context((output / "contention.stderr").open("w")))


def contention_child(output, scheduled):
    """Bound the 36 owned yes processes even if the experiment parent exits."""
    processes = []
    result = {"scheduled_start_monotonic": scheduled, "scheduled_end_monotonic": scheduled + 60}
    try:
        while time.monotonic() < scheduled:
            time.sleep(max(0, min(0.2, scheduled - time.monotonic())))
        started = time.monotonic()
        if started - scheduled > 1:
            raise RuntimeError("The contention supervisor missed its start by more than one second.")
        result.update(started_monotonic=started, started_utc=datetime.datetime.now(datetime.timezone.utc).isoformat())
        for _ in range(36):
            processes.append(subprocess.Popen(["/usr/bin/yes"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        result["pids"] = [process.pid for process in processes]
        result["all_started_monotonic"] = time.monotonic()
        publish_contention(output, result)
        deadline = scheduled + 60
        while time.monotonic() < deadline:
            if any(process.poll() is not None for process in processes):
                raise RuntimeError("A contention process exited before the scheduled end.")
            time.sleep(max(0, min(0.2, deadline - time.monotonic())))
    finally:
        terminate_owned(processes)
        result["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        result["finished_monotonic"] = time.monotonic()
        result["returncodes"] = [process.returncode for process in processes]
        publish_contention(output, result)


class Engine(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(SOCKET)


def api(method, route, body=None, timeout=30):
    connection = Engine("localhost", timeout=timeout)
    try:
        connection.request(method, route, None if body is None else json.dumps(body), {"Content-Type": "application/json"})
        response = connection.getresponse()
        raw = response.read()
        if response.status >= 400:
            raise RuntimeError(f"Docker {method} {route}: {response.status}: {raw.decode()}")
        return json.loads(raw) if raw else None
    finally:
        connection.close()


def event(output, kind, **fields):
    record = {"event": kind, "monotonic": time.monotonic(), "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), **fields}
    with (output / "events.jsonl").open("a") as stream:
        stream.write(json.dumps(record) + "\n")


def cli(output, label, arguments, timeout=30, check=True):
    started = time.monotonic()
    command = DOCKER + arguments
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        record = {"command": command, "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr, "seconds": time.monotonic() - started}
    except subprocess.TimeoutExpired as error:
        record = {"command": command, "timeout_seconds": timeout, "seconds": time.monotonic() - started, "stdout": (error.stdout or b"").decode(), "stderr": (error.stderr or b"").decode()}
        (output / (label + ".json")).write_text(json.dumps(record, indent=2) + "\n")
        if check:
            raise
        return record
    (output / (label + ".json")).write_text(json.dumps(record, indent=2) + "\n")
    if check:
        result.check_returncode()
    return record


def volume_mount(source, target, readonly=False):
    return {"Type": "volume", "Source": source, "Target": target, "ReadOnly": readonly}


def clone_volume(output, prefix, image, source, target):
    api("POST", "/volumes/create", {"Name": target, "Labels": {"arcbox.stall.experiment": prefix}})
    name = target + "-copy"
    payload = {"Image": image, "Entrypoint": ["/bin/cp"], "Cmd": ["-a", "/from/.", "/to/"], "Labels": {"arcbox.stall.experiment": prefix}, "HostConfig": {"NetworkMode": "none", "Mounts": [volume_mount(source, "/from", True), volume_mount(target, "/to")]}}
    identifier = api("POST", "/containers/create?name=" + quote(name), payload)["Id"]
    try:
        api("POST", f"/containers/{identifier}/start")
        result = api("POST", f"/containers/{identifier}/wait?condition=not-running", timeout=90)
        event(output, "volume_copy", source=source, target=target, result=result)
        if result["StatusCode"]:
            cli(output, name + "-logs", ["logs", name], check=False)
            raise RuntimeError(f"Volume copy failed: {name}: {result}")
    finally:
        api("DELETE", f"/containers/{identifier}?force=true&v=true")


def container_payload(seed, kind, prefix, network, volume):
    keys = ("Env", "Cmd", "Entrypoint", "User", "WorkingDir", "Healthcheck", "StopSignal", "StopTimeout", "Hostname", "Domainname", "Tty", "OpenStdin")
    payload = {key: copy.deepcopy(seed["Config"][key]) for key in keys if seed["Config"].get(key) is not None}
    host_keys = ("CapAdd", "CapDrop", "SecurityOpt", "Privileged", "Ulimits", "ReadonlyRootfs", "ShmSize", "IpcMode", "PidMode", "CgroupnsMode", "OomKillDisable", "OomScoreAdj", "PidsLimit", "GroupAdd", "Init", "CpuShares", "NanoCpus", "CpuPeriod", "CpuQuota", "CpusetCpus", "Memory", "MemorySwap", "MemoryReservation")
    host = {key: copy.deepcopy(seed["HostConfig"][key]) for key in host_keys if seed["HostConfig"].get(key) is not None}
    host.update(NetworkMode=network, RestartPolicy={"Name": "no"}, Mounts=[volume_mount(volume, SEEDS[kind][4])])
    if kind == "ch":
        host["Mounts"].append(volume_mount(UDF_VOLUME, "/var/lib/clickhouse/user_scripts", True))
    payload.update(Image=seed["Image"], Labels={"arcbox.stall.experiment": prefix, "arcbox.stall.role": kind}, HostConfig=host, NetworkingConfig={"EndpointsConfig": {network: {"Aliases": [SEEDS[kind][0]]}}})
    return payload


def wait_for_keeper(output, name):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        state = api("GET", f"/containers/{name}/json")["State"]
        if not state["Running"]:
            raise RuntimeError("The cloned Keeper exited before readiness.")
        if state.get("Health", {}).get("Status") == "healthy":
            event(output, "keeper_ready")
            return
        time.sleep(0.5)
    raise RuntimeError("The cloned Keeper did not become healthy within 90 seconds.")


def wait_for_clickhouse(output, name, started):
    while time.monotonic() - started < 25:
        state = api("GET", f"/containers/{name}/json")["State"]
        if not state["Running"]:
            raise RuntimeError("The cloned ClickHouse exited before readiness.")
        logs = cli(output, "readiness-logs", ["logs", "--tail", "200", name], timeout=5)
        if "Ready for connections." in logs["stdout"] + logs["stderr"]:
            event(output, "clickhouse_ready", since_start_seconds=time.monotonic() - started)
            return
        time.sleep(0.5)
    raise RuntimeError("ClickHouse readiness exceeded 25 seconds; the planned contention window would be invalid.")


def run(args):
    output = args.output.resolve()
    output.mkdir(exist_ok=False)
    if args.kernel_trace is None:
        observer_source = args.native_observer or Path(__file__).resolve().with_name("observe-clickhouse.sh")
        observer_bytes = observer_source.read_bytes()
        observer = output / "observer"
        observer.write_bytes(observer_bytes)
        observer.chmod(observer_source.stat().st_mode & 0o777)
        observer_command = ["/tmp/stall-observe"] if args.native_observer else ["bash", "/tmp/stall-observe"]
        observer_arguments = [args.observer_policy] if args.native_observer else []
        (output / "observer.json").write_text(json.dumps({"path": str(observer_source.resolve()), "sha256": hashlib.sha256(observer_bytes).hexdigest(), "command": observer_command, "policy": args.observer_policy if args.native_observer else "other"}, indent=2) + "\n")
    (output / "fixture.json").write_text(json.dumps(FIXTURE, indent=2) + "\n")
    prefix = "stall-warm-" + args.case + "-" + uuid.uuid4().hex[:8]
    names = {kind: prefix + "-" + kind for kind in SEEDS}
    volumes = {kind: name + "-data" for kind, name in names.items()}
    trace = KernelTrace(sys.modules[__name__], output, prefix, args.kernel_trace) if args.kernel_trace is not None else None
    resources = {"socket": SOCKET, "prefix": prefix, "containers": names, "volumes": volumes, "network": prefix, "case": args.case}
    if trace is not None:
        resources["tracer"] = trace.name
    (output / "resources.json").write_text(json.dumps(resources, indent=2) + "\n")
    seeds = {}
    for kind, (name, identifier, image, volume, destination) in SEEDS.items():
        seed = api("GET", f"/containers/{name}/json")
        if seed["Id"] != identifier or seed["Image"] != image or seed["State"]["Running"]:
            raise RuntimeError(f"The stopped seed identity does not match: {name}.")
        if seed["Config"]["Labels"].get("com.docker.compose.project") != FIXTURE["seed_project"]:
            raise RuntimeError(f"The seed project does not match: {name}.")
        if not any(mount.get("Name") == volume and mount["Destination"] == destination for mount in seed["Mounts"]):
            raise RuntimeError(f"The seed volume does not match: {name}.")
        if seed["HostConfig"].get("CapAdd") or seed["HostConfig"].get("Privileged"):
            raise RuntimeError(f"The seed capability baseline changed: {name}.")
        api("GET", "/images/" + quote(image, safe="") + "/json")
        seeds[kind] = seed
        (output / (kind + "-seed.json")).write_text(json.dumps(seed, indent=2) + "\n")
    udf = api("GET", "/containers/" + quote(FIXTURE["udf_volume"]["writer"]) + "/json")
    if udf["State"]["Running"]:
        raise RuntimeError("The UDF seed writer must remain stopped.")
    if args.case != "control" and not Path("/usr/bin/yes").is_file():
        raise RuntimeError("The host prerequisite /usr/bin/yes is missing. The user must restore the prerequisite.")
    (output / "configs").mkdir()
    frozen_configs = []
    for kind, relative, destination, expected in CONFIGS:
        contents = (CONFIG_ROOT / relative).read_bytes()
        if hashlib.sha256(contents).hexdigest() != expected:
            raise RuntimeError(f"Retained config hash changed: {relative}.")
        frozen = output / "configs" / Path(relative).name
        frozen.write_bytes(contents)
        frozen_configs.append((kind, frozen, destination))
    created = []
    sampler = None
    contention = None
    guest_pid = None
    cleanup_errors = []
    with ExitStack() as streams:
        try:
            for kind, seed in seeds.items():
                clone_volume(output, prefix, seed["Image"], SEEDS[kind][3], volumes[kind])
            for kind, seed in seeds.items():
                current = api("GET", f"/containers/{SEEDS[kind][0]}/json")["State"]
                if current["Running"] or current["StartedAt"] != seed["State"]["StartedAt"]:
                    raise RuntimeError("A seed changed while its volume was being copied.")
            api("POST", "/networks/create", {"Name": prefix, "Labels": {"arcbox.stall.experiment": prefix}})
            for kind, seed in seeds.items():
                payload = container_payload(seed, kind, prefix, prefix, volumes[kind])
                if kind == "ch" and trace is None:
                    # Cross-process syscall reads require this diagnostic capability.
                    payload["HostConfig"]["CapAdd"] = ["SYS_PTRACE"]
                if kind == "ch" and args.native_observer and args.observer_policy == "fifo":
                    payload["HostConfig"]["CapAdd"].append("SYS_NICE")
                (output / (kind + "-create.json")).write_text(json.dumps(payload, indent=2) + "\n")
                api("POST", "/containers/create?name=" + quote(names[kind]), payload)
                created.append(names[kind])
            for kind, path, destination in frozen_configs:
                cli(output, kind + "-copy-" + path.name, ["cp", str(path), names[kind] + ":" + destination])
            if trace is None:
                cli(output, "copy-observer", ["cp", str(observer), names["ch"] + ":/tmp/stall-observe"])
            api("POST", f"/containers/{names['keeper']}/start")
            wait_for_keeper(output, names["keeper"])
            if trace is not None:
                trace.prepare(seeds["ch"]["Image"])
            start_target = None
            if args.case == "startup-contention":
                scheduled = time.monotonic() + 1
                contention = start_contention(output, streams, scheduled)
                event(output, "contention_scheduled", pid=contention.pid, start_monotonic=scheduled, end_monotonic=scheduled + 60, workers=36)
                result_path = output / "contention.json"
                while not result_path.exists() and time.monotonic() < scheduled + 60:
                    if contention.poll() is not None:
                        raise RuntimeError("The contention supervisor exited before publishing worker readiness.")
                    time.sleep(0.05)
                status = json.loads(result_path.read_text())
                if len(status.get("pids", [])) != 36 or "finished_monotonic" in status:
                    raise RuntimeError("The contention supervisor did not publish 36 active workers.")
                start_target = status["all_started_monotonic"] + 5
                while time.monotonic() < start_target:
                    if contention.poll() is not None:
                        raise RuntimeError("The contention supervisor exited before ClickHouse start.")
                    time.sleep(max(0, min(0.05, start_target - time.monotonic())))
            event(output, "clickhouse_start_requested", target_monotonic=start_target)
            requested = time.monotonic()
            if start_target is not None and (contention.poll() is not None or requested - start_target > 1):
                raise RuntimeError("ClickHouse missed its contention start window by more than one second, or contention exited.")
            api("POST", f"/containers/{names['ch']}/start")
            started = time.monotonic()
            event(output, "clickhouse_started", requested_monotonic=requested, completed_monotonic=started, observation_seconds=SECONDS)
            wait_for_clickhouse(output, names["ch"], started)
            libc = cli(output, "libc-hash", ["exec", names["ch"], "sha256sum", "/lib/aarch64-linux-gnu/libc.so.6"])
            if libc["stdout"].split()[0] != LIBC_HASH:
                raise RuntimeError("The running libc hash does not match the retained disassembly.")
            identity_files = ["/proc/1/status", "/proc/self/status", "/sys/fs/cgroup/cpu.max"]
            if trace is None:
                identity_files.insert(0, "/proc/1/maps")
            cli(output, "identity", ["exec", "--user", "0", names["ch"], "cat", *identity_files])
            cli(output, "query-prerequisites", ["exec", names["ch"], "bash", "-e", "-c", "command -v timeout; command -v clickhouse-client"])
            if trace is None:
                cli(output, "proc-preflight", ["exec", "--user", "0", names["ch"], *observer_command, "1", *observer_arguments], timeout=10)
                sampler = subprocess.Popen(DOCKER + ["exec", "--user", "0", names["ch"], *observer_command, "360", *observer_arguments], stdout=streams.enter_context((output / "proc-samples.txt").open("w")), stderr=streams.enter_context((output / "proc-samples.stderr").open("w")))
            else:
                guest_pid = api("GET", f"/containers/{names['ch']}/json")["State"]["Pid"]
                cli(output, "runtime-settings", ["exec", names["ch"], "timeout", "--kill-after=1", "3", "clickhouse-client", "--query", "SELECT name,value FROM system.server_settings WHERE name IN ('global_profiler_real_time_period_ns','global_profiler_cpu_time_period_ns','background_schedule_pool_size') FORMAT JSONEachRow"], timeout=4)
                trace.identity("before", guest_pid)
            if time.monotonic() - started >= 30:
                raise RuntimeError("Observation setup missed the 30-second contention boundary.")
            if args.case == "contention":
                contention = start_contention(output, streams, started + 30)
                event(output, "contention_scheduled", pid=contention.pid, start_seconds=30, end_seconds=90, workers=36)
            next_query = started
            query_number = 0
            while time.monotonic() - started < SECONDS:
                elapsed = time.monotonic() - started
                if sampler is not None and sampler.poll() is not None:
                    raise RuntimeError(f"The independent proc observer exited early: {sampler.returncode}.")
                if contention is not None and contention.poll() not in (None, 0):
                    raise RuntimeError(f"The bounded contention supervisor failed: {contention.returncode}.")
                if time.monotonic() >= next_query:
                    # The guest timeout bounds the client after a host-side Docker timeout.
                    record = cli(output, f"query-{query_number:03}", ["exec", names["ch"], "timeout", "--kill-after=1", "3", "clickhouse-client", "--connect_timeout", "1", "--receive_timeout", "2", "--send_timeout", "2", "--query", "SELECT 1 SETTINGS max_execution_time=1, max_threads=1"], timeout=4, check=False)
                    event(output, "query", number=query_number, since_start_seconds=elapsed, returncode=record.get("returncode"), timeout_seconds=record.get("timeout_seconds"), seconds=record["seconds"])
                    if trace is not None and record.get("returncode") != 0:
                        trace.freeze(f"query-{query_number:03}")
                    query_number += 1
                    next_query = started + 5 * (int((time.monotonic() - started) / 5) + 1)
                time.sleep(0.05)
            event(output, "observation_complete", elapsed_seconds=time.monotonic() - started)
        finally:
            if trace is not None and trace.created:
                try:
                    trace.collect(guest_pid)
                except Exception as error:
                    cleanup_errors.append(f"kernel trace: {error}")
                created.append(trace.name)
            if contention is not None:
                try:
                    terminate_owned([contention])
                except Exception as error:
                    cleanup_errors.append(f"contention supervisor: {error}")
            for name in created:
                try:
                    event(output, "container_stop_requested", container=name)
                    api("POST", f"/containers/{name}/stop?t=10", timeout=25)
                except Exception as error:
                    cleanup_errors.append(f"{name} stop: {error}")
                try:
                    logs = cli(output, name + "-logs", ["logs", "--timestamps", name], timeout=20, check=False)
                    if logs.get("returncode") != 0:
                        cleanup_errors.append(f"{name} logs: inspect the saved command result.")
                    final = api("GET", f"/containers/{name}/json")
                    (output / (name + "-final.json")).write_text(json.dumps(final, indent=2) + "\n")
                except Exception as error:
                    cleanup_errors.append(f"{name} diagnostics: {error}")
            if sampler is not None:
                terminate_owned([sampler])
            event(output, "cleanup", errors=cleanup_errors, retained_containers=created, retained_volumes=volumes, retained_network=prefix)
            if cleanup_errors and sys.exc_info()[0] is None:
                raise RuntimeError("Cleanup failed: " + "; ".join(cleanup_errors))


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, stop_on_signal)
    signal.signal(signal.SIGINT, stop_on_signal)
    if len(sys.argv) == 4 and sys.argv[1] == "--contention-child":
        contention_child(Path(sys.argv[2]), float(sys.argv[3]))
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--fixture", type=Path, required=True, help="Verified stopped-seed identities, config hashes, and Docker socket")
        parser.add_argument("--case", choices=("control", "contention", "startup-contention"), required=True)
        parser.add_argument("--output", type=Path, required=True)
        observer_mode = parser.add_mutually_exclusive_group()
        observer_mode.add_argument("--native-observer", type=Path, help="Static Linux observe_clickhouse binary; adds SYS_NICE for FIFO sampling")
        observer_mode.add_argument("--kernel-trace", choices=("off", "on"), help="Replace proc polling with an isolated tracefs recorder; off records markers only")
        parser.add_argument("--observer-policy", choices=("fifo", "other"), default="fifo", help="Scheduling policy for the native observer")
        args = parser.parse_args()
        FIXTURE = json.loads(args.fixture.read_text())
        SOCKET = FIXTURE["socket"]
        DOCKER = ["docker", "--host", "unix://" + SOCKET]
        CONFIG_ROOT = Path(FIXTURE["config_root"])
        # Fix resource order so cleanup always stops ClickHouse before Keeper.
        SEEDS = {kind: FIXTURE["seeds"][kind] for kind in ("ch", "keeper")}
        CONFIGS = FIXTURE["configs"]
        LIBC_HASH = FIXTURE["libc_hash"]
        UDF_VOLUME = FIXTURE["udf_volume"]["name"]
        if not Path(SOCKET).is_absolute() or not CONFIG_ROOT.is_absolute():
            parser.error("Fixture socket and config_root must be absolute paths.")
        run(args)

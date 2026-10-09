"""Control an isolated tracefs recorder and retain evidence before container cleanup."""

import hashlib
import json
import time
from pathlib import Path


class KernelTrace:
    def __init__(self, driver, output, prefix, mode, futex_pc=None):
        self.driver = driver
        self.output = output
        self.instance = prefix
        self.name = prefix + "-trace"
        self.mode = mode
        self.futex_pc = futex_pc
        self.created = False
        self.frozen = False
        self.pc_frozen = False
        self.sequence = 0

    def prepare(self, image):
        source = Path(__file__).with_name("trace-clickhouse.sh")
        contents = source.read_bytes()
        frozen = self.output / "trace-clickhouse.sh"
        frozen.write_bytes(contents)
        observer = {"kind": "kernel-events", "mode": self.mode, "script_sha256": hashlib.sha256(contents).hexdigest(), "container": self.name, "instance": self.instance}
        if self.futex_pc is not None:
            pc_contents = source.with_name("trace-clickhouse-pc.sh").read_bytes()
            pc_frozen = self.output / "trace-clickhouse-pc.sh"
            pc_frozen.write_bytes(pc_contents)
            observer["futex_pc"] = {"guest_pid_namespace": self.futex_pc, "script_sha256": hashlib.sha256(pc_contents).hexdigest(), "instance": self.instance + "-pc"}
        payload = {"Image": image, "User": "0", "Entrypoint": ["/bin/sleep"], "Cmd": ["infinity"], "Healthcheck": {"Test": ["NONE"]}, "Labels": {"arcbox.stall.experiment": self.instance, "arcbox.stall.role": "trace"}, "HostConfig": {"Privileged": True, "PidMode": "host", "NetworkMode": "none", "RestartPolicy": {"Name": "no"}}}
        (self.output / "trace-create.json").write_text(json.dumps(payload, indent=2) + "\n")
        (self.output / "observer.json").write_text(json.dumps(observer, indent=2) + "\n")
        self.driver.api("POST", "/containers/create?name=" + self.name, payload)
        self.created = True
        self.driver.api("POST", f"/containers/{self.name}/start")
        self.driver.cli(self.output, "copy-trace-helper", ["cp", str(frozen), self.name + ":/tmp/trace-clickhouse.sh"])
        self.command("prepare", self.mode, timeout=30)
        if self.futex_pc is not None:
            self.driver.cli(self.output, "copy-pc-trace-helper", ["cp", str(pc_frozen), self.name + ":/tmp/trace-clickhouse-pc.sh"])
            self.command("prepare", self.futex_pc, pc=True)

    def command(self, action, *arguments, timeout=30, pc=False):
        kind = "pc_trace" if pc else "trace"
        helper = "trace-clickhouse-pc.sh" if pc else "trace-clickhouse.sh"
        label = f"{kind}-{self.sequence:03}-{action}"
        self.sequence += 1
        self.driver.event(self.output, kind + "_control_requested", label=label, action=action, arguments=arguments)
        try:
            return self.driver.cli(self.output, label, ["exec", "--user", "0", self.name, "bash", "/tmp/" + helper, action, self.instance, *map(str, arguments)], timeout=timeout)
        finally:
            self.driver.event(self.output, kind + "_control_returned", label=label, action=action)

    def identity(self, label, pid):
        self.command("identity", label, pid)

    def early_identity(self, pid, started):
        if self.futex_pc is None:
            return
        remaining = started + 25 - time.monotonic()
        if remaining < 1:
            raise RuntimeError("The startup deadline leaves less than one second for PC identity capture.")
        self.command("identity", "early", pid, min(20, remaining), timeout=remaining, pc=True)

    def freeze(self, reason):
        errors = []
        for pc, frozen in ((False, self.frozen), (True, self.pc_frozen)):
            if frozen or (pc and self.futex_pc is None):
                continue
            try:
                self.command("freeze", reason, timeout=90, pc=pc)
                if pc:
                    self.pc_frozen = True
                else:
                    self.frozen = True
            except Exception as error:
                errors.append(f"{'PC' if pc else 'base'} freeze: {error}")
        if errors:
            raise RuntimeError("Kernel trace freeze failed: " + "; ".join(errors))

    def collect(self, pid):
        # Retain partial evidence even when another cleanup operation fails.
        errors = []
        operations = [("freeze", lambda: self.freeze("finalize"))]
        if pid is not None:
            operations.append(("identity", lambda: self.identity("after", pid)))
            if self.futex_pc is not None:
                operations.append(("PC identity", lambda: self.command("identity", "after", pid, pc=True)))
        operations.append(("export", lambda: self.command("export", timeout=120)))
        if self.futex_pc is not None:
            operations.append(("PC export", lambda: self.command("export", timeout=120, pc=True)))
        operations.append(("copy", lambda: self.driver.cli(self.output, "trace-copy-capture", ["cp", self.name + ":/capture/.", str(self.output / "kernel-trace")], timeout=120)))
        for label, operation in operations:
            try:
                operation()
            except Exception as error:
                errors.append(f"{label}: {error}")
        if errors:
            raise RuntimeError("Kernel trace capture is incomplete: " + "; ".join(errors))

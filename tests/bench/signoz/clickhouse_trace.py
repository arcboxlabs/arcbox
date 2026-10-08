"""Control an isolated tracefs recorder and retain evidence before container cleanup."""

import hashlib
import json
from pathlib import Path


class KernelTrace:
    def __init__(self, driver, output, prefix, mode):
        self.driver = driver
        self.output = output
        self.instance = prefix
        self.name = prefix + "-trace"
        self.mode = mode
        self.created = False
        self.frozen = False
        self.sequence = 0

    def prepare(self, image):
        source = Path(__file__).with_name("trace-clickhouse.sh")
        contents = source.read_bytes()
        frozen = self.output / "trace-clickhouse.sh"
        frozen.write_bytes(contents)
        payload = {"Image": image, "User": "0", "Entrypoint": ["/bin/sleep"], "Cmd": ["infinity"], "Healthcheck": {"Test": ["NONE"]}, "Labels": {"arcbox.stall.experiment": self.instance, "arcbox.stall.role": "trace"}, "HostConfig": {"Privileged": True, "PidMode": "host", "NetworkMode": "none", "RestartPolicy": {"Name": "no"}}}
        (self.output / "trace-create.json").write_text(json.dumps(payload, indent=2) + "\n")
        (self.output / "observer.json").write_text(json.dumps({"kind": "kernel-events", "mode": self.mode, "script_sha256": hashlib.sha256(contents).hexdigest(), "container": self.name, "instance": self.instance}, indent=2) + "\n")
        self.driver.api("POST", "/containers/create?name=" + self.name, payload)
        self.created = True
        self.driver.api("POST", f"/containers/{self.name}/start")
        self.driver.cli(self.output, "copy-trace-helper", ["cp", str(frozen), self.name + ":/tmp/trace-clickhouse.sh"])
        self.command("prepare", self.mode, timeout=30)

    def command(self, action, *arguments, timeout=30):
        label = f"trace-{self.sequence:03}-{action}"
        self.sequence += 1
        self.driver.event(self.output, "trace_control_requested", label=label, action=action, arguments=arguments)
        try:
            return self.driver.cli(self.output, label, ["exec", "--user", "0", self.name, "bash", "/tmp/trace-clickhouse.sh", action, self.instance, *map(str, arguments)], timeout=timeout)
        finally:
            self.driver.event(self.output, "trace_control_returned", label=label, action=action)

    def identity(self, label, pid):
        self.command("identity", label, pid)

    def freeze(self, reason):
        if not self.frozen:
            self.command("freeze", reason, timeout=90)
            self.frozen = True

    def collect(self, pid):
        # Retain partial evidence even when another cleanup operation fails.
        errors = []
        operations = [("freeze", lambda: self.freeze("finalize"))]
        if pid is not None:
            operations.append(("identity", lambda: self.identity("after", pid)))
        operations.extend([
            ("export", lambda: self.command("export", timeout=120)),
            ("copy", lambda: self.driver.cli(self.output, "trace-copy-capture", ["cp", self.name + ":/capture/.", str(self.output / "kernel-trace")], timeout=120)),
        ])
        for label, operation in operations:
            try:
                operation()
            except Exception as error:
                errors.append(f"{label}: {error}")
        if errors:
            raise RuntimeError("Kernel trace capture is incomplete: " + "; ".join(errors))

"""Verify recorder failures preserve cleanup and the original startup deadline."""

import hashlib
import json
import tempfile
import unittest
from collections import Counter
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

import clickhouse_trace
import run_clickhouse_restart as restart


class KernelTraceTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.output = Path(directory.name)
        self.driver = SimpleNamespace(api=Mock(), cli=Mock(), event=Mock())
        self.trace = clickhouse_trace.KernelTrace(self.driver, self.output, "experiment", "on", "pid:[123]")

    def test_early_identity_uses_remaining_startup_budget(self):
        for now, budget, timeout in ((100, 20, 25), (111.5, 13.5, 13.5)):
            with self.subTest(now=now), patch.object(clickhouse_trace.time, "monotonic", return_value=now):
                self.trace.early_identity(42, 100)
                call = self.driver.cli.call_args
                self.assertEqual(call.args[2][-3:], ["early", "42", str(budget)])
                self.assertEqual(call.kwargs["timeout"], timeout)
        self.driver.cli.reset_mock()
        with patch.object(clickhouse_trace.time, "monotonic", return_value=124.1):
            with self.assertRaisesRegex(RuntimeError, "startup deadline"):
                self.trace.early_identity(42, 100)
        self.driver.cli.assert_not_called()
        self.trace.futex_pc = None
        self.trace.early_identity(42, 100)
        self.driver.cli.assert_not_called()

    def test_collect_attempts_both_recorders_and_copy_after_each_failure(self):
        for failure in (
            "trace-000-freeze",
            "pc_trace-001-freeze",
            "trace-002-identity",
            "pc_trace-003-identity",
            "trace-004-export",
            "pc_trace-005-export",
        ):
            with self.subTest(failure=failure):
                self.driver.cli.reset_mock()
                self.driver.event.reset_mock()
                self.trace = clickhouse_trace.KernelTrace(self.driver, self.output, "experiment", "on", "pid:[123]")
                calls = []

                def cli(_output, label, arguments, **_kwargs):
                    calls.append((label, arguments))
                    if label == failure:
                        raise RuntimeError("injected failure")

                self.driver.cli.side_effect = cli
                with self.assertRaisesRegex(RuntimeError, "injected failure"):
                    self.trace.collect(42)
                self.assertEqual(len(calls), 7)
                self.assertEqual(calls[-1][0], "trace-copy-capture")
                self.assertEqual(calls[-1][1][1], "experiment-trace:/capture/.")
                self.assertEqual(
                    [args[6] for _, args in calls[:-1]],
                    ["freeze", "freeze", "identity", "identity", "export", "export"],
                )
                events = [call.args[1] for call in self.driver.event.call_args_list]
                self.assertEqual(events.count("trace_control_returned"), 3)
                self.assertEqual(events.count("pc_trace_control_returned"), 3)

    def test_freeze_retries_only_the_failed_recorder(self):
        self.driver.cli.side_effect = [RuntimeError("base freeze failed"), None, None]
        with self.assertRaisesRegex(RuntimeError, "base freeze failed"):
            self.trace.freeze("query")
        self.assertFalse(self.trace.frozen)
        self.assertTrue(self.trace.pc_frozen)
        self.trace.freeze("finalize")
        self.trace.freeze("already frozen")
        self.assertEqual(self.driver.cli.call_count, 3)
        self.assertEqual(self.driver.cli.call_args.args[2][5], "/tmp/trace-clickhouse.sh")

    def test_prepare_preserves_helper_hashes_and_separate_lifecycle(self):
        with patch.object(Path, "read_bytes", side_effect=[b"base helper", b"pc helper"]):
            self.trace.prepare("sha256:image")
        observer = json.loads((self.output / "observer.json").read_text())
        self.assertEqual((self.output / "trace-clickhouse-pc.sh").read_bytes(), b"pc helper")
        self.assertEqual(observer["futex_pc"]["script_sha256"], hashlib.sha256(b"pc helper").hexdigest())
        self.assertEqual(observer["futex_pc"]["guest_pid_namespace"], "pid:[123]")
        preparations = [call for call in self.driver.cli.call_args_list if call.args[2][0] == "exec"]
        self.assertEqual([call.args[2][-1] for call in preparations], ["on", "pid:[123]"])
        self.assertEqual(preparations[0].args[2][7], "experiment")
        self.assertEqual(preparations[1].args[2][7], "experiment")

    def test_pc_requires_event_tracing_before_creating_output(self):
        for mode in (None, "off"):
            args = SimpleNamespace(futex_pc="pid:[123]", kernel_trace=mode, output=self.output / "unused")
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "requires --kernel-trace on"):
                restart.run(args)
            self.assertFalse(args.output.exists())

    def test_default_recorder_keeps_the_full_validator_lifecycle(self):
        trace = clickhouse_trace.KernelTrace(self.driver, self.output, "experiment", "on")
        trace.prepare("sha256:image")
        trace.early_identity(42, 0)
        trace.identity("before", 42)
        trace.freeze("query")
        trace.collect(42)
        requests = [call for call in self.driver.event.call_args_list if call.args[1] == "trace_control_requested"]
        self.assertEqual(
            Counter(call.kwargs["action"] for call in requests), {"prepare": 1, "identity": 2, "freeze": 1, "export": 1}
        )
        self.assertFalse(any(call.args[1].startswith("pc_trace_") for call in self.driver.event.call_args_list))
        self.assertNotIn("futex_pc", json.loads((self.output / "observer.json").read_text()))

    def test_startup_identity_failure_collects_evidence_before_stopping_containers(self):
        output = self.output / "run"
        args = SimpleNamespace(
            output=output,
            case="control",
            network=None,
            kernel_trace="on",
            native_observer=None,
            observer_policy="other",
            futex_pc="pid:[123]",
        )
        seeds = {kind: (kind, kind + "-id", "image", kind + "-data", "/data") for kind in ("ch", "keeper")}
        records = {
            name: {
                "Id": identifier,
                "Image": image,
                "State": {"Running": False, "StartedAt": "seed"},
                "Config": {"Labels": {"com.docker.compose.project": "seeds"}},
                "HostConfig": {},
                "Mounts": [{"Name": volume, "Destination": destination}],
            }
            for name, identifier, image, volume, destination in seeds.values()
        }
        order = []

        def api(method, route, _body=None, **_kwargs):
            if method == "GET" and route == "/containers/udf/json":
                return {"State": {"Running": False}}
            if method == "GET" and route.startswith("/containers/"):
                return records.get(route.split("/")[2], {"State": {"Pid": 42}})
            if method == "POST" and "/stop?" in route:
                order.append("stop")
            return {}

        trace = Mock(name="recorder", created=True)
        trace.name = "experiment-trace"
        trace.early_identity.side_effect = RuntimeError("early identity failed")
        trace.collect.side_effect = lambda pid: order.append(("collect", pid))
        with patch.multiple(
            restart,
            create=True,
            SOCKET="/unused.sock",
            SEEDS=seeds,
            CONFIGS=[],
            CONFIG_ROOT=self.output,
            UDF_VOLUME="udf-data",
            FIXTURE={"seed_project": "seeds", "udf_volume": {"writer": "udf"}},
        ):
            with (
                patch.object(restart, "api", side_effect=api),
                patch.object(restart, "clone_volume"),
                patch.object(restart, "wait_for_keeper"),
                patch.object(restart, "wait_for_clickhouse") as readiness,
                patch.object(restart, "KernelTrace", return_value=trace),
                patch.object(restart, "cli", return_value={"returncode": 0}),
            ):
                with self.assertRaisesRegex(RuntimeError, "early identity failed"):
                    restart.run(args)
        trace.early_identity.assert_called_once()
        self.assertEqual(trace.early_identity.call_args.args[0], 42)
        trace.collect.assert_called_once_with(42)
        self.assertEqual(order, [("collect", 42), "stop", "stop", "stop"])
        readiness.assert_not_called()
        self.assertEqual(json.loads((output / "ch-started.json").read_text())["State"]["Pid"], 42)
        cleanup = json.loads((output / "events.jsonl").read_text().splitlines()[-1])
        self.assertEqual(cleanup["errors"], [])


if __name__ == "__main__":
    unittest.main(verbosity=2)

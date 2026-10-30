"""Exercise PC probe ownership and failed-export cleanup without tracefs."""

import subprocess
import tempfile
import unittest
from pathlib import Path

HELPER = Path(__file__).with_name("trace-clickhouse-pc.sh").read_text()
FUNCTIONS = HELPER[HELPER.index("definition() {") : HELPER.index("\nif [[ $action == prepare ]]; then")]
# Redirect the two global tracefs files. Instance paths already use variables.
FUNCTIONS = FUNCTIONS.replace("/trace/kprobe_events", '"$trace_root/kprobe_events"').replace(
    "/trace/kprobe_profile", '"$trace_root/kprobe_profile"'
)
REQUEST = next(line for line in HELPER.splitlines() if line.lstrip().startswith("printf 'p:"))
UNRELATED = "p:other/other __arm64_sys_futex op=+8($arg1):x32\n"
HARNESS = r"""
set -euo pipefail
fixture=$1
trace_root=$fixture/trace
capture=$fixture/capture
trace=$trace_root/instances/fixture-pc
group=arcbox_pc_fixture
event_name=$group
event=$trace/events/$group/$event_name
instance_owned=1
probe_owned=1
fail() { printf '%s\n' "$*" >&2; exit 1; }
# Ordinary files cannot implement tracefs deletion; emulate only that write.
printf() {
    if [[ $# == 4 && $1 == -- && $2 == '-:%s/%s\n' ]]; then
        builtin printf "$@" >>"$fixture/deletions"
        awk -v key="p:$3/$4" '$1 != key' "$trace_root/kprobe_events" >"$fixture/events-next"
        mv "$fixture/events-next" "$trace_root/kprobe_events"
    else
        builtin printf "$@"
    fi
}
rmdir() { [[ $1 == "$trace" ]] && builtin printf 'removed\n' >"$fixture/instance-removed"; }
"""


class PcTraceHelperTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="arcbox-pc-offline-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.capture = self.root / "capture"
        self.trace = self.root / "trace/instances/fixture-pc"
        self.event = self.trace / "events/arcbox_pc_fixture/arcbox_pc_fixture"
        self.cpu = self.trace / "per_cpu/cpu0"
        for path in (self.capture, self.event, self.cpu):
            path.mkdir(parents=True)
        for name in (
            "tracing_on",
            "current_tracer",
            "trace_clock",
            "tracing_cpumask",
            "buffer_size_kb",
            "buffer_total_size_kb",
            "buffer_subbuf_size_kb",
            "trace_options",
            "set_event_pid",
            "set_event_notrace_pid",
            "events/enable",
        ):
            (self.trace / name).write_text("1\n")
        for name in ("enable", "filter", "format", "trigger"):
            (self.event / name).write_text("1\n")
        (self.cpu / "stats").write_text("overrun: 0\n")
        (self.cpu / "trace").write_text("retained event\n")
        (self.root / "trace/kprobe_profile").write_text("arcbox_pc_fixture 7 0\n")
        self.assertNotIn("/trace/", FUNCTIONS)
        self.assertEqual(self.run_helper(REQUEST).returncode, 0)
        self.requested = (self.capture / "requested-definition").read_text()
        self.reset_definitions()

    def reset_definitions(self):
        for name in ("requested-definition", "registered-definition"):
            (self.capture / name).write_text(self.requested)
        (self.root / "trace/kprobe_events").write_text(UNRELATED + self.requested)

    def run_helper(self, command):
        script = self.root / "check.sh"
        script.write_text(HARNESS + FUNCTIONS + "\n" + command + "\n")
        return subprocess.run(
            ["/bin/bash", str(script), str(self.root)],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def assert_stopped(self):
        for path in (
            self.trace / "tracing_on",
            self.trace / "events/enable",
            self.event / "enable",
        ):
            self.assertEqual(path.read_text(), "0\n")

    def test_exact_owned_definition_can_be_removed_without_touching_other_probe(self):
        result = self.run_helper("remove_owned 1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.root / "deletions").read_text(),
            "-:arcbox_pc_fixture/arcbox_pc_fixture\n",
        )
        self.assertEqual((self.root / "trace/kprobe_events").read_text(), UNRELATED)
        self.assertEqual((self.capture / "kprobe-events-after").read_text(), UNRELATED)
        self.assertEqual((self.capture / "cleanup-exit").read_text(), "0\n")
        self.assertTrue((self.root / "instance-removed").exists())
        self.assert_stopped()

    def test_changed_offset_or_type_in_any_ownership_record_prevents_probe_deletion(
        self,
    ):
        records = (
            self.capture / "requested-definition",
            self.capture / "registered-definition",
            self.root / "trace/kprobe_events",
        )
        for record in records:
            for original, changed in (
                ("+256($arg1):x64", "+248($arg1):x64"),
                ("x20=+160($arg1):u64", "x20=+160($arg1):u32"),
                ("comm=$comm ", "comm=$comm:string "),
            ):
                with self.subTest(record=record.name, changed=changed):
                    self.reset_definitions()
                    record.write_text(record.read_text().replace(original, changed))
                    result = self.run_helper("remove_owned 1")
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn("Owned probe definition changed", result.stderr)
                    self.assertFalse((self.root / "deletions").exists())
                    self.assertIn(
                        "p:arcbox_pc_fixture/arcbox_pc_fixture",
                        (self.root / "trace/kprobe_events").read_text(),
                    )
                    self.assert_stopped()

    def test_incomplete_export_retains_disabled_probe_and_frozen_instance(self):
        for missing in (self.event / "format", self.cpu / "trace"):
            with self.subTest(missing=missing.name):
                original = missing.read_text()
                missing.unlink()
                result = self.run_helper("action=export\ntrap failure_cleanup EXIT\nexit 7")
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertIn("preserve the disabled probe and frozen instance", result.stderr)
                self.assertFalse((self.root / "deletions").exists())
                self.assertFalse((self.root / "instance-removed").exists())
                self.assertEqual(
                    (self.root / "trace/kprobe_events").read_text(),
                    UNRELATED + self.requested,
                )
                self.assertEqual((self.capture / "cleanup-exit").read_text(), "1\n")
                self.assert_stopped()
                missing.write_text(original)


if __name__ == "__main__":
    unittest.main(verbosity=2)

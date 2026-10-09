"""Verify ktrace target extraction retains cross-thread scheduler transitions."""

import csv
import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

from analyze_host_ktrace import analyze


class TargetRecordsTests(unittest.TestCase):
    def test_cross_thread_transitions_preserve_current_spans(self):
        records = """timestampns,eventname,debugid,arg1,arg2,arg3,arg4,cpuid,threadid,pid,execname
10739166,MACH_MKRUNNABLE,20971544,26851545,31,0,16,11,28452963,58061,com.apple.Virtualization.Virtua
10739500,MACH_SCHED_CHOOSE_PROCESSOR,20971616,26851545,18446744073709551615,10,6,11,28452963,58061,com.apple.Virtualization.Virtua
10740000,MACH_MKRUNNABLE,20971544,26851546,31,0,16,11,28452963,58061,com.apple.Virtualization.Virtua
10741000,MACH_DISPATCH,20971648,26851546,0,1,15,11,28452963,58061,com.apple.Virtualization.Virtua
10744375,MACH_STKHANDOFF,20971528,0,26851545,31,31,11,28452963,58061,com.apple.Virtualization.Virtua
10940000,MACH_WAIT,20971584,16207272001901045139,2,0,0,11,26851545,58061,com.apple.Virtualization.Virtua
10942166,MACH_STKHANDOFF,20971528,0,28452963,31,31,11,26851545,58061,com.apple.Virtualization.Virtua
10943333,MACH_DISPATCH,20971648,26851545,0,1,15,11,28452963,58061,com.apple.Virtualization.Virtua
10944000,MACH_SCHED_THREAD_SELECT,20971810,26851545,0,0,4,11,28452963,58061,com.apple.Virtualization.Virtua
"""
        rows = list(csv.reader(io.StringIO(records)))
        process = Mock(stdout=io.StringIO(records))
        process.wait.return_value = 0
        process.poll.return_value = 0
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory) / "result"
            with patch("analyze_host_ktrace.subprocess.Popen", return_value=process), redirect_stdout(io.StringIO()):
                analyze(Path(directory) / "saved.ktrace", prefix, {26851545})
            with prefix.with_suffix(".target.csv").open() as output:
                retained = list(csv.reader(output))
            self.assertEqual(retained, [rows[index] for index in (0, 1, 5, 6, 7, 8)])
            spans = json.loads(prefix.with_suffix(".spans.json").read_text())
            self.assertEqual(spans, {"scheduled_current": [[26851545, 11, 10744375, 10942166]], "interrupts": []})
            summary = json.loads(prefix.with_suffix(".json").read_text())
            self.assertEqual(summary["event_count"], 9)
            self.assertEqual(summary["switch_chain_mismatch_count"], 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)

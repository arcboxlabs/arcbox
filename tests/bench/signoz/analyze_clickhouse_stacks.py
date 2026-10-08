#!/usr/bin/env python3
"""Read an exported stack capture. Usage: analyze_clickhouse_stacks.py CASE_DIR | --self-test."""
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

CPUS = range(18)
COUNTERS = {"entries", "overrun", "commit overrun", "bytes", "dropped events", "read events"}
LOSS = {"overrun", "commit overrun", "dropped events", "read events"}
PREFIX = re.compile(r"^\s*(?P<comm>.+)-(?P<tid>\d+)\s+\[(?P<cpu>\d+)\]\s+(?P<context>\S+)\s+(?P<stamp>\d+\.\d+):\s+(?P<payload>.*)$")
HEADER = re.compile(r"^# entries-in-buffer/entries-written: (\d+)/(\d+)\s+#P:(\d+)\s*$")
BEGIN = re.compile(r"contention_begin:\s+([0-9a-f]+)\s+\(flags=SPIN\)")
FRAME = re.compile(r"^\s*=>\s+(.+?)\s*$")
TRIGGER = re.compile(r"stacktrace:count=(\d+) if flags == 1")
LIMITS = [
    "This checker validates stack settings and ring records. Use validate_clickhouse_trace.py to validate a completed application case.",
    "The application lifecycle status is separate from trace integrity. A partial startup is not a completed case.",
    "Adjacent same-CPU/TID/context records support a contender-stack association, not identification of the lock owner.",
    "Consecutive begin records, changed context, and nonadjacent stacks remain ambiguous; no hash is assigned to those stacks.",
    "Context is sampled twice. A need-resched flag change alone does not prove IRQ/NMI insertion; this parser still rejects that pair conservatively.",
    "Trace comm is printed metadata, not an identity condition. A later identity snapshot may report a different name for the same TID.",
    "The stack trigger count is shared and decremented without atomic synchronization. 128 is a requested budget, not a strict global limit.",
    "A trigger may consume count without producing a stack. Ring-buffer loss counters do not detect every stack-capture failure.",
    "An IRQ stack can contain the interrupted task's frames. Its common_pid is not proof that the interrupted task initiated the IRQ work.",
    "Stack capture perturbs contention timing. Guest timestamps and scheduling records do not establish host physical CPU execution.",
    "Displayed lock hashes are scoped to this capture. The hashes are not raw filter addresses or lock ownership identities.",
]


def contents(path):
    return path.read_text().strip()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def nanoseconds(stamp):
    whole, fraction = stamp.split(".")
    require(len(fraction) <= 9, f"Unexpected timestamp precision: {stamp}")
    return int(whole) * 1_000_000_000 + int(fraction.ljust(9, "0"))


def parse_stats(path):
    pairs = [line.split(":", 1) for line in contents(path).splitlines()]
    require(all(len(pair) == 2 for pair in pairs), f"Malformed statistics: {path}")
    fields = dict(pairs)
    require(len(fields) == len(pairs) and set(fields) == COUNTERS | {"oldest event ts", "now ts"}, f"Unexpected statistics fields: {path}")
    return {key: int(value) if key in COUNTERS else value.strip() for key, value in fields.items()}


def parse_trace(path, cpu, stack_mode):
    records, headers = [], []
    for line_number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        if line.startswith("#"):
            require(not records, f"Unexpected annotation after data: {path}:{line_number}: {line}")
            header = HEADER.fullmatch(line)
            if header:
                headers.append(tuple(map(int, header.groups())))
            continue
        frame = FRAME.fullmatch(line)
        if frame:
            require(stack_mode and records and records[-1]["kind"] == "stack", f"Orphan stack frame: {path}:{line_number}")
            records[-1]["frames"].append(frame[1])
            records[-1]["end_line"] = line_number
            continue
        match = PREFIX.fullmatch(line)
        require(match is not None, f"Unparsed trace record: {path}:{line_number}: {line}")
        record = match.groupdict()
        record.update(cpu=int(record["cpu"]), tid=int(record["tid"]), line=line_number, end_line=line_number, ns=nanoseconds(record["stamp"]))
        record["comm"] = record["comm"].strip()
        require(record["cpu"] == cpu, f"Wrong CPU in {path}:{line_number}")
        require(not records or records[-1]["ns"] <= record["ns"], f"Timestamp regression in {path}:{line_number}")
        if not stack_mode:
            require(record["payload"] != "<stack trace>", f"Unexpected stack in main trace: {path}:{line_number}")
            record["kind"] = "event"
        elif record["payload"] == "<stack trace>":
            record.update(kind="stack", frames=[])
        else:
            begin = BEGIN.fullmatch(record["payload"])
            require(begin is not None, f"Unexpected stack-instance event: {path}:{line_number}: {record['payload']}")
            record.update(kind="begin", lock_hash=begin[1])
        records.append(record)
    require(len(headers) == 1, f"Expected one ring header in {path}")
    require(all(record["frames"] for record in records if record["kind"] == "stack"), f"Empty stack in {path}")
    return records, headers[0]


def associate(records):
    stacks = []
    for index, record in enumerate(records):
        if record["kind"] != "stack":
            continue
        result = {key: record[key] for key in ("cpu", "tid", "comm", "context", "stamp", "line", "end_line", "frames")}
        reasons = []
        previous = records[index - 1] if index else None
        if previous is None or previous["kind"] != "begin":
            reasons.append("immediately_previous_record_is_not_a_begin")
        else:
            result["candidate_begin"] = {key: previous[key] for key in ("cpu", "tid", "comm", "context", "stamp", "line", "lock_hash")}
            result["begin_to_stack_printed_ns"] = record["ns"] - previous["ns"]
            for key in ("cpu", "tid", "context"):
                if previous[key] != record[key]:
                    reasons.append(f"{key}_changed_between_begin_and_stack")
            if index >= 2 and records[index - 2]["kind"] == "begin":
                reasons.append("consecutive_begins_may_include_nested_or_uncaptured_stack")
        result["association"] = "ambiguous" if reasons else "adjacent_same_context"
        result["ambiguity_reasons"] = reasons
        if not reasons:
            result["lock_hash"] = previous["lock_hash"]
        stacks.append(result)
    return stacks


def verify_ring(folder, stack_mode, errors):
    def check(condition, message):
        if not condition:
            errors.append(message)
    expected_names = {f"cpu{cpu}.trace" for cpu in CPUS}
    check({p.name for p in (folder / "export").glob("cpu*.trace")} == expected_names, f"{folder}: expected exactly 18 CPU traces")
    cpus, headers, all_stacks, begin_counts = [], [], [], Counter()
    for cpu in CPUS:
        before = parse_stats(folder / "export" / f"cpu{cpu}.stats-before")
        after = parse_stats(folder / "export" / f"cpu{cpu}.stats-after")
        initial = parse_stats(folder / "initial" / f"cpu{cpu}.stats")
        check(all(initial[key] == 0 for key in COUNTERS), f"{folder.name} CPU {cpu}: nonempty initial counters")
        check(all(before[key] == after[key] for key in before if key != "now ts"), f"{folder.name} CPU {cpu}: counters changed during export")
        check(nanoseconds(after["now ts"]) >= nanoseconds(before["now ts"]), f"{folder.name} CPU {cpu}: export timestamp regressed")
        check(all(before[key] >= 0 for key in COUNTERS), f"{folder.name} CPU {cpu}: negative counter")
        check(all(before[key] == 0 for key in LOSS), f"{folder.name} CPU {cpu}: nonzero loss or consumer counter")
        path = folder / "export" / f"cpu{cpu}.trace"
        records, header = parse_trace(path, cpu, stack_mode)
        headers.append(header)
        check(len(records) == before["entries"], f"{folder.name} CPU {cpu}: parsed {len(records)} records, expected {before['entries']}")
        counts = Counter(record["kind"] for record in records)
        cpus.append({"cpu": cpu, "stats_before": before, "stats_after": after, "parsed_records": len(records), "record_types": dict(counts), "first_stamp": records[0]["stamp"] if records else None, "last_stamp": records[-1]["stamp"] if records else None})
        if stack_mode:
            for record in records:
                if record["kind"] == "begin":
                    begin_counts[record["lock_hash"]] += 1
            for record in associate(records):
                record["path"] = str(path)
                all_stacks.append(record)
    total = sum(cpu["stats_before"]["entries"] for cpu in cpus)
    written = sum(cpu["stats_before"]["entries"] + cpu["stats_before"]["overrun"] for cpu in cpus)
    check(all(header == (total, written, 18) for header in headers), f"{folder.name}: trace header totals disagree with per-CPU statistics")
    return {"cpus": cpus, "total_records": total, "loss_counters": {key: sum(cpu["stats_before"][key] for cpu in cpus) for key in sorted(LOSS)}}, all_stacks, begin_counts


def active_trigger(path):
    return [line for line in contents(path).splitlines() if line and not line.startswith("#")]


def verify_config(capture, errors):
    stacks = capture / "stacks"
    def check(condition, message):
        if not condition:
            errors.append(message)
    check(contents(stacks / "instance") == contents(capture / "instance") + "-stacks", "Stack instance ownership mismatch")
    check(contents(capture / "mode") == "on", "Expected an events-on capture")
    check(float(contents(capture / "export-complete")) > 0 and float(contents(stacks / "freeze-complete")) > 0, "Missing export or stack freeze completion")
    payload = int(re.search(r"field: char data;.*?size:\s*(\d+);", contents(capture / "header_page"))[1])
    blocks = max(2, (1024 * 1024 + payload - 1) // payload)
    capacity = blocks * payload // 1024
    sizing = {key: int(value) for key, value in (line.split("\t") for line in contents(stacks / "buffer-sizing.tsv").splitlines())}
    check(sizing == {"requested_kb": 1024, "payload_bytes": payload, "blocks": blocks, "expected_per_cpu_kb": capacity, **{f"cpu{cpu}_kb": capacity for cpu in CPUS}}, "Stack buffer sizing mismatch")
    format_text = contents(stacks / "initial/lock-contention_begin/format")
    for phase in ("initial", "frozen", "export"):
        folder = stacks / phase
        expected = {"tracing_on": "0", "current_tracer": "nop", "buffer_size_kb": str(capacity), "buffer_total_size_kb": str(18 * capacity), "set_event_pid": "", "set_event_notrace_pid": "", "events-enable": "0"}
        check(all(contents(folder / key) == value for key, value in expected.items()), f"Invalid stack {phase} configuration")
        check("[mono]" in contents(folder / "trace_clock") and int(contents(folder / "tracing_cpumask").replace(",", ""), 16) == 0x3FFFF, f"Invalid stack {phase} clock or CPU mask")
        options = set(contents(folder / "trace_options").split())
        check(set("overwrite context-info hash-ptr nostacktrace nouserstacktrace noevent-fork noverbose noraw nohex nobin nolatency-format norecord-tgid".split()) <= options, f"Unexpected stack {phase} options")
        event = folder / "lock-contention_begin"
        check(contents(event / "enable") == "0" and contents(event / "filter") == "flags == 1", f"Invalid stack {phase} event settings")
        check(not active_trigger(event / "trigger") and contents(event / "format") == format_text, f"Invalid stack {phase} trigger or format")
    armed = TRIGGER.fullmatch(contents(stacks / "armed-trigger"))
    frozen = TRIGGER.fullmatch(contents(stacks / "frozen-trigger-before-remove"))
    check(armed is not None and frozen is not None, "Unexpected counted-trigger readback")
    check(armed is None or 0 <= int(armed[1]) <= 128, "Armed trigger count exceeds the requested budget")
    check(frozen is None or 0 <= int(frozen[1]) <= 128, "Frozen trigger count exceeds the requested budget")
    check(not active_trigger(stacks / "frozen-trigger-after-remove"), "A trigger remained after freeze")
    if (capture / "failures.tsv").exists():
        errors.append("The helper recorded failed operations: " + contents(capture / "failures.tsv"))
    return {"requested_count": 128, "armed_readback_count": int(armed[1]) if armed else None, "frozen_readback_count": int(frozen[1]) if frozen else None, "requested_per_cpu_kib": 1024, "effective_per_cpu_kib": capacity}


def summarize(stacks, begin_counts):
    grouped = defaultdict(list)
    for stack in stacks:
        if stack["association"] == "adjacent_same_context":
            grouped[stack["lock_hash"]].append(stack)
    result = []
    for lock_hash, begins in begin_counts.items():
        matched = grouped[lock_hash]
        paths = defaultdict(list)
        for stack in matched:
            paths[tuple(stack["frames"])].append(stack)
        path_summaries = []
        for frames, samples in sorted(paths.items(), key=lambda entry: (-len(entry[1]), entry[0])):
            sample = samples[0]
            path_summaries.append({"count": len(samples), "frames": list(frames), "comms": dict(Counter(s["comm"] for s in samples)), "contexts": dict(Counter(s["context"] for s in samples)), "example": {key: sample[key] for key in ("path", "line", "end_line", "stamp", "cpu", "tid")}})
        result.append({"lock_hash": lock_hash, "begin_records": begins, "associated_stacks": len(matched), "comms": dict(Counter(stack["comm"] for stack in matched)), "contexts": dict(Counter(stack["context"] for stack in matched)), "paths": path_summaries})
    return sorted(result, key=lambda item: (-item["associated_stacks"], -item["begin_records"], item["lock_hash"]))


def analyze(case):
    capture = case / "kernel-trace"
    errors = []
    report = {"case": str(case), "trace_integrity": "FAIL", "limitations": LIMITS, "errors": errors}
    try:
        records = [json.loads(line) for line in contents(case / "events.jsonl").splitlines()]
        completed = [record for record in records if record.get("event") == "observation_complete"]
        before = (capture / "identity-before/completeness").exists()
        report["application_status"] = "completion_marker_present_unvalidated" if len(completed) == 1 and before else "partial"
        report["application_failure_events"] = [record for record in records if record.get("event") in {"error", "failure", "cleanup", "observation_failed"}]
        report["has_before_identity"] = before
        report["has_after_identity"] = (capture / "identity-after/completeness").exists()
        report["freeze_reason"] = contents(capture / "freeze-reason")
        report["trigger"] = verify_config(capture, errors)
        report["stack_ring"], stacks, begin_counts = verify_ring(capture / "stacks", True, errors)
        report["main_ring"], _, _ = verify_ring(capture, False, errors)
        report["stack_records"] = stacks
        report["total_stacks"] = len(stacks)
        report["associations"] = dict(Counter(stack["association"] for stack in stacks))
        report["ambiguous_reasons"] = dict(Counter(reason for stack in stacks for reason in stack["ambiguity_reasons"]))
        report["hashes"] = summarize(stacks, begin_counts)
        report["trace_integrity"] = "PASS" if not errors else "FAIL"
    except (OSError, ValueError, KeyError, TypeError, IndexError) as error:
        errors.append(f"{type(error).__name__}: {error}")
    return report



def self_test():
    import tempfile
    import unittest

    header = "# tracer: nop\n# entries-in-buffer/entries-written: 2/2   #P:18\n"

    def begin(tid=42, stamp="1.000001", cpu=0, context="d....", lock="0000000000000001"):
        return f"worker-{tid} [{cpu:03}] {context} {stamp}: contention_begin: {lock} (flags=SPIN)\n"

    def stack(tid=42, stamp="1.000002", cpu=0, context="d...."):
        return f"worker-{tid} [{cpu:03}] {context} {stamp}: <stack trace>\n => queued_spin_lock_slowpath\n => do_sigaction\n"

    class Checks(unittest.TestCase):
        def parse(self, data, stack_mode=True):
            with tempfile.TemporaryDirectory(prefix="arcbox-stack-parser-") as directory:
                path = Path(directory) / "cpu0.trace"
                path.write_text(header + data)
                return parse_trace(path, 0, stack_mode)[0]

        def test_frames_do_not_increment_ring_count(self):
            records = self.parse(begin() + stack())
            self.assertEqual(len(records), 2)
            self.assertEqual(records[1]["frames"], ["queued_spin_lock_slowpath", "do_sigaction"])
            self.assertEqual(associate(records)[0]["association"], "adjacent_same_context")

        def test_equal_printed_timestamps_are_allowed(self):
            self.assertEqual(associate(self.parse(begin() + stack(stamp="1.000001")))[0]["begin_to_stack_printed_ns"], 0)

        def test_nested_pairs_are_not_guessed(self):
            records = self.parse(begin() + begin(stamp="1.000002", lock="0000000000000002") + stack(stamp="1.000003") + stack(stamp="1.000004"))
            samples = associate(records)
            self.assertEqual([sample["association"] for sample in samples], ["ambiguous", "ambiguous"])
            self.assertTrue(all("lock_hash" not in sample for sample in samples))

        def test_changed_pid_is_ambiguous(self):
            self.assertEqual(associate(self.parse(begin() + stack(tid=43)))[0]["ambiguity_reasons"], ["tid_changed_between_begin_and_stack"])

        def test_changed_context_is_ambiguous_without_irq_diagnosis(self):
            self.assertEqual(associate(self.parse(begin() + stack(context="dN...")))[0]["ambiguity_reasons"], ["context_changed_between_begin_and_stack"])

        def test_comm_is_not_an_identity_condition(self):
            sample = associate(self.parse(begin() + stack().replace("worker-", "renamed-")))[0]
            self.assertEqual(sample["association"], "adjacent_same_context")

        def test_orphan_frame_and_empty_stack_are_rejected(self):
            for data in [" => do_sigaction\n", "worker-42 [000] d.... 1.000002: <stack trace>\n"]:
                with self.subTest(data=data), self.assertRaises(ValueError):
                    self.parse(data)

        def test_wrong_cpu_and_regressed_time_are_rejected(self):
            for data in [begin(cpu=1), begin() + stack(stamp="1.000000")]:
                with self.subTest(data=data), self.assertRaises(ValueError):
                    self.parse(data)

        def test_unknown_event_or_main_stack_is_rejected(self):
            with self.assertRaises(ValueError):
                self.parse("worker-42 [000] d.... 1.000001: contention_end: 0000000000000001 (ret=0)\n")
            with self.assertRaises(ValueError):
                self.parse(stack(), stack_mode=False)

    return unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(Checks)).wasSuccessful()


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(0 if self_test() else 1)
    require(len(sys.argv) == 2, __doc__)
    report = analyze(Path(sys.argv[1]).resolve())
    print(json.dumps(report, indent=2))
    sys.exit(0 if report["trace_integrity"] == "PASS" else 1)

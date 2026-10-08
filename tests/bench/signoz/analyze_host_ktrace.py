#!/usr/bin/env python3
"""Validate a completed macOS ktrace stream and retain vCPU scheduling evidence."""
import argparse
import collections
import csv
import json
import pathlib
import subprocess
import time

SWITCHES = {0x01400000, 0x01400008}
LOSS = {0x07020008, 0x07020014, 0x07020018}


class Switches:
    def __init__(self, targets):
        self.targets = targets
        self.state = {}
        self.mismatches = []
        self.mismatch_count = 0
        self.spans = []
        self.self_switches = 0

    def add(self, ts, cpu, old, new):
        previous = self.state.get(cpu)
        if previous is not None:
            start, current = previous
            assert ts >= start, (ts, start, cpu)
            if old != current:
                self.mismatch_count += 1
                if len(self.mismatches) < 20:
                    self.mismatches.append([cpu, start, ts, current, old, new])
            elif old in self.targets:
                self.spans.append([old, cpu, start, ts])
        self.self_switches += old == new
        self.state[cpu] = (ts, new)


def self_check():
    tracker = Switches({10, 11})
    tracker.add(10, 0, 1, 10)
    tracker.add(15, 0, 10, 10)
    tracker.add(20, 0, 10, 2)
    tracker.add(25, 0, 3, 10)
    tracker.add(30, 1, 4, 11)
    tracker.add(35, 1, 11, 4)
    assert tracker.spans == [[10, 0, 10, 15], [10, 0, 15, 20], [11, 1, 30, 35]]
    assert tracker.mismatch_count == 1
    assert tracker.self_switches == 1


def analyze(trace, prefix, targets):
    command = ['/usr/bin/ktrace', 'trace', '-R', str(trace),
               '-f', 'S0x0140,S0x0105,S0x0109,C0x07', '--csv', '-t', '-n']
    counts, seconds, switch_seconds = collections.Counter(), collections.Counter(), collections.Counter()
    cpu_bounds, switch_bounds, seen_targets = {}, {}, {}
    losses, irq_stacks, irq_spans = [], collections.defaultdict(list), []
    irq_unmatched_end, irq_tid_mismatch = 0, 0
    tracker, last_ts, count, first_ts, started = Switches(targets), None, 0, None, time.monotonic()
    with pathlib.Path(f'{prefix}.stderr').open('x') as errors, pathlib.Path(f'{prefix}.target.csv').open('x') as target:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=errors, text=True)
        assert process.stdout is not None
        try:
            reader = csv.reader(process.stdout)
            header = next(reader)
            assert header == ['timestampns', 'eventname', 'debugid', 'arg1', 'arg2', 'arg3', 'arg4', 'cpuid', 'threadid', 'pid', 'execname'], header
            writer = csv.writer(target)
            writer.writerow(header)
            for row in reader:
                assert len(row) == 11, row
                ts, code, cpu, tid = (int(row[i]) for i in (0, 2, 7, 8))
                assert last_ts is None or ts >= last_ts, (last_ts, ts)
                first_ts = ts if first_ts is None else first_ts
                last_ts = ts
                count += 1
                counts[row[1]] += 1
                seconds[ts // 1_000_000_000] += 1
                cpu_bounds.setdefault(cpu, [ts, ts, 0])[1] = ts
                cpu_bounds[cpu][2] += 1
                base = code & ~3
                if tid in targets:
                    seen_targets.setdefault(tid, set()).add((row[9], row[10]))
                relevant = tid in targets
                if base in SWITCHES:
                    incoming = int(row[4])
                    tracker.add(ts, cpu, tid, incoming)
                    switch_seconds[ts // 1_000_000_000] += 1
                    switch_bounds.setdefault(cpu, [ts, ts, 0])[1] = ts
                    switch_bounds[cpu][2] += 1
                    relevant = relevant or incoming in targets
                if base in LOSS:
                    losses.append(row)
                if base == 0x01050000:
                    if code & 3 == 1:
                        irq_stacks[cpu].append((ts, tid))
                    elif code & 3 == 2:
                        if irq_stacks[cpu]:
                            begin, interrupted = irq_stacks[cpu].pop()
                            irq_tid_mismatch += interrupted != tid
                            if interrupted in targets:
                                irq_spans.append([interrupted, cpu, begin, ts])
                        else:
                            irq_unmatched_end += 1
                if relevant:
                    writer.writerow(row)
                if count % 5_000_000 == 0:
                    print(f'{count} events; trace {ts / 1e9:.3f}s; decode {time.monotonic() - started:.1f}s', flush=True)
            status = process.wait()
        finally:
            process.stdout.close()
            if process.poll() is None:
                process.terminate()
                process.wait()
    assert status == 0, status
    residency, previous_end, overlaps = collections.Counter(), {}, []
    for tid, cpu, start, end in sorted(tracker.spans, key=lambda x: (x[0], x[2], x[3])):
        if tid in previous_end and start < previous_end[tid]:
            overlaps.append([tid, cpu, start, end, previous_end[tid]])
        previous_end[tid] = max(previous_end.get(tid, 0), end)
        residency[tid] += end - start
    result = {
        'command': command, 'decode_exit_status': status, 'event_count': count,
        'timestamp_unit': 'nanoseconds relative to trace origin; integer CSV fields',
        'first_event_ns': first_ts, 'last_event_ns': last_ts, 'counts': counts,
        'per_second_events': seconds, 'per_second_switch_records': switch_seconds,
        'per_cpu_event_bounds': cpu_bounds, 'per_cpu_switch_bounds': switch_bounds,
        'loss_or_order_markers': losses, 'switch_chain_mismatch_count': tracker.mismatch_count,
        'switch_chain_mismatch_examples': tracker.mismatches, 'self_switch_records': tracker.self_switches,
        'expected_vcpu_tids': sorted(targets), 'observed_vcpu_process_identity': {k: sorted(v) for k, v in seen_targets.items()},
        'vcpu_closed_scheduled_current_ns': residency, 'vcpu_cross_cpu_overlap_count': len(overlaps),
        'vcpu_cross_cpu_overlaps': overlaps[:20], 'vcpu_span_count': len(tracker.spans),
        'irq_span_count': len(irq_spans), 'irq_unmatched_end': irq_unmatched_end,
        'irq_open_at_end': dict(irq_stacks), 'irq_tid_mismatch': irq_tid_mismatch,
        'decode_elapsed_seconds': time.monotonic() - started,
        'limits': ['Event continuity does not measure tracer overhead or prove application failure.',
                   'Scheduled-current spans include host interrupts and hypervisor work; they are not exact guest execution.',
                   'Open boundary spans are excluded. No time is imputed across switch-chain mismatches.',
                   'Absent target events do not establish the target thread state.']}
    with pathlib.Path(f'{prefix}.json').open('x') as output:
        json.dump(result, output, indent=2)
        output.write('\n')
    with pathlib.Path(f'{prefix}.spans.json').open('x') as output:
        json.dump({'scheduled_current': tracker.spans, 'interrupts': irq_spans}, output)
        output.write('\n')
    print(json.dumps({k: result[k] for k in ('event_count', 'first_event_ns', 'last_event_ns', 'switch_chain_mismatch_count', 'vcpu_span_count')}), flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--trace', type=pathlib.Path)
    parser.add_argument('--output-prefix', type=pathlib.Path)
    parser.add_argument('--vcpu-tids', type=int, nargs='+')
    args = parser.parse_args()
    self_check()
    if args.self_test:
        print('Switch continuity, self-switch, and mismatch checks passed.')
    else:
        if not (args.trace and args.output_prefix and args.vcpu_tids):
            parser.error('--trace, --output-prefix, and --vcpu-tids are required')
        analyze(args.trace, args.output_prefix, set(args.vcpu_tids))

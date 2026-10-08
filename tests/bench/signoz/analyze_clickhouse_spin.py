"""Usage: analyze_clickhouse_spin.py CASE_DIR | --self-test.

Validate the capture with validate_clickhouse_trace.py before interpreting results.
Pair exact SPIN events; other lock classes have different event nesting semantics.
"""
from collections import Counter, defaultdict
import heapq
import itertools
import json
from pathlib import Path
import re
import sys

LINE = re.compile(r'^\s*(.*)-(\d+)\s+\[(\d+)\]\s+(\S+)\s+(\d+)\.(\d+):\s+(.*)$')
BEGIN = re.compile(r'([0-9a-f]+) \(flags=([^)]*)\)')
END = re.compile(r'([0-9a-f]+) \(ret=(-?\d+)\)')
RUNTIME = re.compile(r'comm=(.*?) pid=(\d+) runtime=(\d+) \[ns\]')

def parse(line, cpu, number):
    match = LINE.fullmatch(line.rstrip('\n'))
    if match is None:
        raise ValueError(f'Unparsed CPU {cpu} line {number}: {line[:160]}')
    comm, tid, printed_cpu, flags, seconds, fraction, payload = match.groups()
    assert int(printed_cpu) == cpu
    stamp = int(seconds) * 1_000_000_000 + int(fraction.ljust(9, '0'))
    return {'ns': stamp, 'cpu': cpu, 'line': number, 'tid': int(tid), 'comm': comm.strip(), 'context_flags': flags, 'payload': payload}

def events(path, cpu, counts):
    previous = -1
    for number, line in enumerate(path.open(), 1):
        if line.startswith('#') or not line.strip():
            continue
        row = parse(line, cpu, number)
        assert row['ns'] >= previous
        previous = row['ns']
        kind = row['payload'].split(':', 1)[0]
        if kind in {'contention_begin', 'contention_end', 'sched_stat_runtime'}:
            row['kind'] = kind
            row['detail'] = row.pop('payload').split(': ', 1)[1]
            counts[kind] += 1
            yield row['ns'], cpu, number, row

def lock_key(row, address):
    return row['tid'], row['cpu'] if row['tid'] == 0 else None, address

def take_end(pending, row):
    match = END.fullmatch(row['detail'])
    assert match, row
    address, result = match.groups()
    start = pending.pop(lock_key(row, address), None)
    if start is None:
        return None
    assert row['ns'] >= start['ns']
    return {'seconds': (row['ns'] - start['ns']) / 1e9, 'address': address, 'ret': int(result), 'begin': start, 'end': row}

def self_check():
    row = parse('       BgSchPool-42 [003] d.... 1.000002: contention_end: aabb (ret=0)', 3, 7)
    assert row['ns'] == 1_000_002_000 and row['tid'] == 42 and row['comm'] == 'BgSchPool'
    begin = {'tid': 42, 'ns': 1_000_000_000, 'detail': 'aabb (flags=SPIN)'}
    end = {'tid': 42, 'ns': 1_500_000_000, 'detail': 'aabb (ret=0)'}
    pending = {(42, None, 'aabb'): begin, (43, None, 'aabb'): begin}
    assert take_end(pending, end)['seconds'] == 0.5 and (43, None, 'aabb') in pending
    assert take_end(pending, end) is None
    try:
        take_end({(42, None, 'aabb'): begin}, {**end, 'ns': 0})
    except AssertionError:
        pass
    else:
        raise AssertionError('Negative interval accepted')
    assert RUNTIME.fullmatch('comm=BgSchPool pid=42 runtime=500000000 [ns]').group(3) == '500000000'
    idle = {(0, 0, 'aabb'): {**begin, 'cpu': 0}, (0, 1, 'aabb'): {**begin, 'cpu': 1}}
    assert take_end(idle, {**end, 'tid': 0, 'cpu': 1})['begin']['cpu'] == 1
    assert len(idle) == 1 and (0, 0, 'aabb') in idle

def analyze(root):
    counts, pending, active = Counter(), {}, Counter()
    serial, top, by_lock = itertools.count(), defaultdict(list), defaultdict(Counter)
    duplicates = []
    identities = root / 'kernel-trace/identity-after'
    target_tids = {int(path.parent.name) for path in identities.glob('*/status')}
    def keep(kind, metric, record):
        heapq.heappush(top[kind], (metric, next(serial), record))
        if len(top[kind]) > 50:
            heapq.heappop(top[kind])
    streams = [events(root / f'kernel-trace/export/cpu{cpu}.trace', cpu, counts) for cpu in range(18)]
    for _, _, _, row in heapq.merge(*streams):
        row['clickhouse_identity_after'] = row['tid'] in target_tids
        if row['kind'] == 'contention_begin':
            match = BEGIN.fullmatch(row['detail'])
            assert match, row
            address, flags = match.groups()
            row['lock_flags'] = flags
            key = lock_key(row, address)
            if flags != 'SPIN':
                assert key not in pending, 'Other lock class overlaps an exact SPIN wait'
                counts['other_lock_class_begin'] += 1
                continue
            if key in pending:
                duplicates.append({'previous': pending[key], 'next': row})
            pending[key] = row
            active[address] += 1
            by_lock[address]['begin'] += 1
            by_lock[address]['max_recorded_waiters'] = max(by_lock[address]['max_recorded_waiters'], active[address])
        elif row['kind'] == 'contention_end':
            pair = take_end(pending, row)
            if pair is None:
                counts['end_without_exact_spin_begin'] += 1
                continue
            address = pair['address']
            active[address] -= 1
            by_lock[address]['paired'] += 1
            by_lock[address]['elapsed_seconds_sum'] += pair['seconds']
            by_lock[address]['max_seconds'] = max(by_lock[address]['max_seconds'], pair['seconds'])
            keep('lock', pair['seconds'], pair)
            if row['clickhouse_identity_after']:
                keep('clickhouse_lock', pair['seconds'], pair)
        else:
            match = RUNTIME.fullmatch(row['detail'])
            assert match, row
            comm, pid, charge = match.groups()
            row.update(charged_tid=int(pid), charged_comm=comm, runtime_ns=int(charge))
            keep('runtime', int(charge), row)
            if int(pid) in target_tids:
                keep('clickhouse_runtime', int(charge), row)
    return {'case': str(root), 'counts': counts, 'by_lock': by_lock, 'top': {kind: [entry[2] for entry in sorted(rows, reverse=True)] for kind, rows in top.items()}, 'open_begins': list(pending.values()), 'duplicate_begins': duplicates, 'limits': ['Only exact SPIN begins are paired. Mutex and rwsem event nesting is outside this analysis.', 'PID 0 denotes separate per-CPU idle tasks; those keys include the CPU.', 'Lock end marks acquisition, not release. Pending-bit spin waits are not recorded.', 'The printed lock address is a hash and does not establish a lock name.', 'Guest mono intervals and scheduler charges are not physical CPU measurements.', 'The after snapshot alone cannot exclude PID reuse; verify lifecycle and start ticks separately.']}

if __name__ == '__main__':
    self_check()
    if sys.argv[1:] == ['--self-test']:
        print('lock/runtime parser self-check: PASS')
    else:
        if len(sys.argv) != 2:
            sys.exit(__doc__)
        print(json.dumps(analyze(Path(sys.argv[1])), indent=2))

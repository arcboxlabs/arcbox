"""Validate the four controlled scheduler and condition-variable cases."""

import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("results", type=Path, help="Directory containing all four completed cases")
root = parser.parse_args().results
summary = {}
for case in ('old-control', 'modern-control', 'old-pause', 'modern-pause'):
    rows = [json.loads(line) for line in (root / case / 'stdout.jsonl').read_text().splitlines()]
    completion = json.loads((root / case / 'completion.json').read_text())
    assert completion['exit_code'] == 0
    assert not (root / case / 'stderr.txt').read_text()
    result, = [row for row in rows if 'result' in row]
    expected_libc = '2.35' if case.startswith('old-') else '2.41'
    assert result['libc'] == expected_libc
    before = {row['candidate']: row for row in rows if row.get('stage') == 'before'}
    after = {row['candidate']: row for row in rows if row.get('stage') == 'after'}
    assert set(before) == set(after) == set(range(8))
    excess = {i: after[i]['runtime_ns'] - before[i]['runtime_ns']
              - (after[i]['raw_prof_ns'] - before[i]['raw_prof_ns']) for i in before}
    spans = [row['end_ns'] - row['begin_ns'] for row in rows if 'begin_ns' in row]
    assert max(spans) < 100_000_000
    report = dict(result=result, excess_ns=excess, max_snapshot_span_ns=max(spans))
    if case.endswith('pause'):
        victim = result['victim']
        assert [i for i in excess if excess[i] >= 8_000_000_000] == [victim]
        held = [row for row in rows if row.get('stage') in ('release', 'waiting')]
        peers = [row for row in rows if row.get('stage') in ('peer_release', 'peer_waiting')]
        assert len(held) >= 40 and len(held) == len(peers)
        assert all(row['state'] == 'R' and not row['handler_exited'] for row in held)
        assert len({(row['runtime_ns'], row['dispatches'], row['raw_prof_ns']) for row in held}) == 1
        assert all(b['runtime_ns'] > a['runtime_ns'] and b['dispatches'] > a['dispatches']
                   and b['raw_prof_ns'] > a['raw_prof_ns'] for a, b in zip(peers, peers[1:]))
        fast = case.startswith('modern-')
        assert all(row['signaler_returned'] == fast for row in held[1:])
        assert held[-1]['begin_ns'] - held[0]['end_ns'] > 40_000_000_000
        assert result['release_to_exit_seconds'] > 40
        assert (result['second_signal_seconds'] < 0.5) if fast else (result['second_signal_seconds'] > 40)
        assert result['returned_before_handler_exit'] == fast
        end_after_exit = result['signaler_end_ns'] - result['handler_exit_ns']
        assert end_after_exit < 0 if fast else 0 <= end_after_exit < 500_000_000
        interior = [row for row in peers if row['begin_ns'] > held[0]['end_ns']
                    and row['end_ns'] < held[-1]['begin_ns']]
        assert len(interior) >= 2
        assert interior[-1]['raw_prof_ns'] > interior[0]['raw_prof_ns']
        report.update(
            held_snapshots=len(held),
            held_snapshot_span_s=(held[-1]['begin_ns'] - held[0]['end_ns']) / 1e9,
            victim_runtime_ns=held[0]['runtime_ns'],
            victim_dispatches=held[0]['dispatches'],
            interior_peer_runtime_advance_s=(interior[-1]['runtime_ns'] - interior[0]['runtime_ns']) / 1e9,
            interior_peer_prof_advance_s=(interior[-1]['raw_prof_ns'] - interior[0]['raw_prof_ns']) / 1e9,
            resume_to_release_s=(result['release_ns'] - result['resumed_ns']) / 1e9,
            notifier_minus_handler_exit_s=(result['signaler_end_ns'] - result['handler_exit_ns']) / 1e9,
        )
    else:
        assert max(excess.values()) < 8_000_000_000
        assert result['release_to_exit_seconds'] < 0.5 and result['second_signal_seconds'] < 0.5
    summary[case] = report
(root / 'analysis.json').write_text(json.dumps(summary, indent=2) + '\n')
for case, report in summary.items():
    print(case, json.dumps(report))

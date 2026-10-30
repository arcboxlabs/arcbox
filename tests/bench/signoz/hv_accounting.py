"""Run the signed cpu_accounting example and suspend only children created here."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--resume', type=int)
    args = parser.parse_args()
    if args.resume is not None:
        if args.resume <= 1:
            parser.error('--resume must identify one child process')
        time.sleep(1)
        os.kill(args.resume, signal.SIGCONT)
        return
    if args.binary is None or args.output is None:
        parser.error('--binary and --output are required')

    with args.output.open('x') as output:
        for repeat in range(3):
            for case in ['busy', 'wfi', 'host-sleep', 'busy-paused', 'wfi-paused', 'busy-contended']:
                pause = case.endswith('-paused')
                mode = case.removesuffix('-paused').removesuffix('-contended')
                burners = []
                process = subprocess.Popen([str(args.binary.resolve()), mode],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                watchdog = None
                record = {'repeat': repeat, 'case': case, 'pause': pause,
                          'utc': datetime.now(timezone.utc).isoformat()}
                try:
                    if not select.select([process.stdout], [], [], 10)[0]:
                        raise RuntimeError('Probe did not become ready')
                    ready = json.loads(process.stdout.readline())
                    assert ready['ready'] and ready['pid'] == process.pid, ready
                    record['ready'] = ready
                    if case == 'busy-contended':
                        for _ in range(2 * os.cpu_count()):
                            burners.append(subprocess.Popen(['/usr/bin/yes'], stdout=subprocess.DEVNULL))
                        record['host_load_processes'] = len(burners)
                    if pause:
                        time.sleep(0.5)
                        # Keep the child unreaped until the watchdog exits, preventing PID reuse.
                        watchdog = subprocess.Popen([sys.executable, __file__, '--resume', str(process.pid)],
                            start_new_session=True, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                        started = time.monotonic()
                        os.kill(process.pid, signal.SIGSTOP)
                        state = subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'state='], text=True).strip()
                        assert 'T' in state, state
                        if watchdog.wait(timeout=5) != 0:
                            raise RuntimeError('Resume watchdog failed')
                        record['stop_resume_window_seconds'] = time.monotonic() - started
                    stdout, stderr = process.communicate(timeout=7)
                    if process.returncode != 0:
                        raise RuntimeError(f'Probe failed: {stderr}')
                    record['measurement'] = json.loads(stdout)
                    assert record['measurement']['wall_seconds'] >= 2.9, record
                    output.write(json.dumps(record) + '\n')
                    output.flush()
                    print(json.dumps(record), flush=True)
                finally:
                    try:
                        if watchdog is not None:
                            watchdog.wait(timeout=5)
                    finally:
                        if process.poll() is None:
                            os.kill(process.pid, signal.SIGCONT)
                            process.terminate()
                            process.wait(timeout=5)
                        for burner in burners:
                            burner.terminate()
                        for burner in burners:
                            burner.wait(timeout=5)


if __name__ == '__main__':
    main()

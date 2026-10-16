#!/usr/bin/env python3
"""Offline host watcher integration tests. No Steam, account, display or game."""
import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/release/steam-tui').resolve(strict=True)
with tempfile.TemporaryDirectory(prefix='steam-tui-host-lifecycle-') as directory:
    path = Path(directory) / 'lease.lock'
    record = {'root': '/fixture/Steam', 'account': {'name': 'fixture', 'steam_id': 76561198000000000}, 'process': {'pid': 2147483647, 'start': 1}, 'token': None}
    with path.open('w+') as owner:
        os.chmod(path, 0o600)
        json.dump(record, owner)
        owner.flush()
        fcntl.flock(owner, fcntl.LOCK_SH)
        game = subprocess.Popen(['/usr/bin/sleep', '0.4'])
        watcher = None
        try:
            start = Path(f'/proc/{game.pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
            watcher = subprocess.Popen([str(binary), '--internal-host-watch', str(path), str(game.pid), start, 'offline-fixture-token'], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            assert select.select([watcher.stdout], [], [], 5)[0], 'watcher readiness timeout'
            assert watcher.stdout.read(1) == b'\x01'
            contender = path.open('r+')
            try:
                try:
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    raise AssertionError('watcher failed to hold its shared lease')
                except BlockingIOError:
                    pass
                # Dropping the launching process's lease must not end the watcher.
                fcntl.flock(owner, fcntl.LOCK_UN)
                assert watcher.poll() is None
                game.wait(timeout=5)
                stdout, stderr = watcher.communicate(timeout=8)
                assert watcher.returncode == 0, stderr.decode(errors='replace')
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                assert path.read_bytes() == b'', 'last watcher did not clear the borrowed-host lease'
            finally:
                contender.close()
        finally:
            if game.poll() is None:
                game.terminate()
                game.wait(timeout=5)
            if watcher is not None and watcher.poll() is None:
                watcher.terminate()
                watcher.wait(timeout=5)
print('PASS: isolated watcher readiness, lifetime across parent lease release, final cleanup; no Steam started')

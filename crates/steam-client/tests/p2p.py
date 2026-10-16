#!/usr/bin/env python3
"""Live CM/ICE echo between two authenticated accounts, without Steam or a game.

Build steam-tui + steam-client first. Accounts must already have saved login tokens.
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import select
import time
import os
from pathlib import Path


def identity(path, app):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(15)
        stream.connect(path)
        request = json.dumps({'version': 1, 'app_id': app, 'operation': 'Identity'}).encode()
        stream.sendall(struct.pack('<I', len(request)) + request)
        def read(size):
            data = b''
            while len(data) < size:
                part = stream.recv(size - len(data))
                if not part:
                    raise RuntimeError('Backend IPC closed')
                data += part
            return data
        reply = json.loads(read(struct.unpack('<I', read(4))[0]))['response']['Identity']
        assert reply['logged_on']
        return reply['steam_id']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('api', type=Path)
    parser.add_argument('--server-account', required=True)
    parser.add_argument('--client-account', required=True)
    parser.add_argument('--app', type=int, default=480)
    parser.add_argument('--logs', type=Path)
    parser.add_argument('--messages', action='store_true', help='Exercise SteamNetworkingMessages002 and SDK session callbacks')
    parser.add_argument('--force-relay', action='store_true', help='disable ICE and require a real SDR route')
    parser.add_argument('--message-bytes', type=int, default=32)
    parser.add_argument('--drop-every', type=int, default=0, help='drop every Nth outgoing SDR data packet to test retransmission')
    args = parser.parse_args()
    assert 1 <= args.message_bytes <= 512 * 1024
    assert args.drop_every == 0 or 2 <= args.drop_every <= 10000
    root = Path(__file__).resolve().parents[3]
    launcher = root / 'target/debug/steam-tui'
    probe = root / 'crates/steam-client/probe.py'
    processes = []
    import contextlib
    folder = args.logs or root / 'target/p2p-probe'
    folder.mkdir(parents=True, exist_ok=True)
    with contextlib.nullcontext(str(folder)) as scratch:
        logs = []
        try:
            backends = []
            for role, account in [('server', args.server_account), ('client', args.client_account)]:
                log = open(Path(scratch) / f'{role}-backend.log', 'w+')
                logs.append(log)
                backend = subprocess.Popen([str(launcher), '--account', account, 'backend', str(args.app)],
                                           stdout=subprocess.PIPE, stderr=log, text=True)
                processes.append(backend)
                assert select.select([backend.stdout], [], [], 20)[0], 'Backend startup timed out'
                path = backend.stdout.readline().strip()
                assert path.startswith('/'), 'Backend did not provide a socket'
                backends.append((path, identity(path, args.app)))
            assert backends[0][1] != backends[1][1], 'Use two different accounts'
            peers = []
            for index, (path, _) in enumerate(backends):
                log = open(Path(scratch) / f'peer-{index}.log', 'w+')
                logs.append(log)
                command = [sys.executable, str(probe), str(args.api.resolve()), str(args.app), '--socket', path]
                command += ['--p2p-peer', str(backends[0][1])] if index else ['--p2p-listen']
                env = os.environ.copy()
                if args.messages:
                    env['STEAM_TUI_PROBE_MESSAGES'] = '1'
                if args.force_relay:
                    env['STEAM_TUI_FORCE_RELAY'] = '1'
                env['STEAM_TUI_PROBE_P2P_BYTES'] = str(args.message_bytes)
                env['STEAM_TUI_SDR_TEST_DROP_EVERY'] = str(args.drop_every)
                peer = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env)
                peers.append(peer)
                processes.append(peer)
                if index == 0:
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline:
                        log.flush(); log.seek(0)
                        if 'P2P LISTEN READY' in log.read():
                            break
                        assert peer.poll() is None, 'Listener exited before ready'
                        time.sleep(.1)
                    else:
                        raise AssertionError('P2P listener did not become ready')
            for peer in reversed(peers):
                assert peer.wait(timeout=50) == 0, 'P2P peer failed'
            print('PASS: two accounts exchanged reliable data through CM signaling and ' + ('SDR' if args.force_relay else 'ICE/SDR'))
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill(); process.wait()
            for log in logs:
                log.flush(); log.seek(0)
                for line in log.read().splitlines():
                    if not any(noise in line for noise in ('input.RunFrame', 'controller.RunFrame')):
                        print(line)
                log.close()


if __name__ == '__main__':
    main()

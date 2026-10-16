#!/usr/bin/env python3
"""ISteamUserStats through the real shared library against a mock backend.

Build: cargo build -p steam-client
Run: python3 crates/steam-client/tests/userstats.py
No account or CM traffic: a thread answers the Stats IPC operations.
"""
import ctypes
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

APP = 480
ME = 76561198000000000
CLIENT = Path(os.environ.get('STEAM_TUI_TEST_CLIENT',
                             Path(__file__).resolve().parents[3] / 'target/debug/libsteamclient.so'))


def kv_schema():
    def sub(n): return b'\0' + n.encode() + b'\0'
    def text(n, v): return b'\1' + n.encode() + b'\0' + v.encode() + b'\0'
    def num(n, v): return b'\2' + n.encode() + b'\0' + struct.pack('<i', v)
    end = b'\x08'
    out = sub(str(APP)) + sub('stats')
    out += sub('1') + text('type', 'INT') + text('name', 'Games') + num('default', 2) + end
    out += sub('2') + text('type', 'FLOAT') + text('name', 'Speed') + text('default', '1.5') + end
    out += sub('3') + num('type', 4) + text('name', 'Ach') + sub('bits') + sub('0')
    out += text('name', 'ACH_WIN') + sub('display') + sub('name') + text('english', 'Winner') + end
    out += text('desc', 'Win once') + num('hidden', 1) + end + end + end + end
    out += end + end + end
    return out


def ffi_child():
    import faulthandler; faulthandler.enable()
    lib = ctypes.CDLL(str(CLIENT))
    lib.CreateInterface.argtypes = [ctypes.c_char_p, ctypes.c_void_p]
    lib.CreateInterface.restype = ctypes.c_void_p
    client = lib.CreateInterface(b'SteamClient020', None)

    def call(obj, slot, result, types=(), args=()):
        table = ctypes.cast(obj, ctypes.POINTER(ctypes.POINTER(ctypes.c_void_p)))[0]
        method = ctypes.CFUNCTYPE(result, ctypes.c_void_p, *types)(table[slot])
        return method(obj, *args)

    pipe = call(client, 0, ctypes.c_int)
    user = call(client, 2, ctypes.c_int, (ctypes.c_int,), (pipe,))
    gtypes = (ctypes.c_int, ctypes.c_int, ctypes.c_char_p)
    stats = call(client, 13, ctypes.c_void_p, gtypes, (user, pipe, b'STEAMUSERSTATS_INTERFACE_VERSION012'))
    utils = call(client, 9, ctypes.c_void_p, (ctypes.c_int, ctypes.c_char_p), (pipe, b'SteamUtils010'))
    assert stats and utils

    def result(call_id, size, expected):
        for _ in range(100):
            if call(utils, 11, ctypes.c_bool, (ctypes.c_uint64, ctypes.c_void_p), (call_id, ctypes.byref(ctypes.c_bool()))):
                break
            time.sleep(0.02)
        else:
            raise AssertionError('call never completed')
        buffer = ctypes.create_string_buffer(size)
        failed = ctypes.c_bool()
        ok = call(utils, 13, ctypes.c_bool,
                  (ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_void_p),
                  (call_id, buffer, size, expected, ctypes.byref(failed)))
        assert ok and not failed.value
        return buffer.raw

    out = {}
    # Leaderboards: find, download, read an entry, upload.
    c = call(stats, 22, ctypes.c_uint64, (ctypes.c_char_p, ctypes.c_int, ctypes.c_int), (b'Ghosts', 2, 1))
    board, found = struct.unpack_from('<QB', result(c, 12, 1104))
    assert found == 1 and board == 7, (board, found)
    out['name'] = call(stats, 24, ctypes.c_char_p, (ctypes.c_uint64,), (board,)).decode()
    out['count'] = call(stats, 25, ctypes.c_int, (ctypes.c_uint64,), (board,))
    c = call(stats, 28, ctypes.c_uint64, (ctypes.c_uint64, ctypes.c_int, ctypes.c_int, ctypes.c_int), (board, 0, 1, 10))
    _, handle, n = struct.unpack_from('<QQi', result(c, 20, 1105))
    out['entries'] = n
    entry = ctypes.create_string_buffer(28)
    details = (ctypes.c_int32 * 4)()
    ok = call(stats, 30, ctypes.c_bool,
              (ctypes.c_uint64, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int),
              (handle, 1, entry, details, 4))
    steam_id, rank, score, cdetails, ugc = struct.unpack('<QiiiQ', entry.raw)
    out['entry'] = [ok, steam_id, rank, score, cdetails, list(details)[:cdetails]]
    dets = (ctypes.c_int32 * 2)(11, 22)
    c = call(stats, 31, ctypes.c_uint64,
             (ctypes.c_uint64, ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int), (board, 1, 99, dets, 2))
    success, b, s, changed, new, prev = struct.unpack_from('<BQiBii', result(c, 28, 1106))
    out['upload'] = [success, s, changed, new, prev]
    # Stats and achievements: load, read, edit, store.
    assert call(stats, 0, ctypes.c_bool)
    for _ in range(100):
        value = ctypes.c_int()
        if call(stats, 1, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_void_p), (b'Games', ctypes.byref(value))):
            break
        time.sleep(0.02)
    out['games'] = value.value
    speed = ctypes.c_float()
    assert call(stats, 2, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_void_p), (b'Speed', ctypes.byref(speed)))
    out['speed'] = speed.value
    assert call(stats, 3, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_int), (b'Games', 9))
    assert not call(stats, 3, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_int), (b'Nope', 9))
    got = ctypes.c_bool()
    assert call(stats, 6, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_void_p), (b'ACH_WIN', ctypes.byref(got)))
    out['ach_before'] = got.value
    assert call(stats, 7, ctypes.c_bool, (ctypes.c_char_p,), (b'ACH_WIN',))
    assert call(stats, 6, ctypes.c_bool, (ctypes.c_char_p, ctypes.c_void_p), (b'ACH_WIN', ctypes.byref(got)))
    out['ach_after'] = got.value
    out['display'] = [call(stats, 12, ctypes.c_char_p, (ctypes.c_char_p, ctypes.c_char_p), (b'ACH_WIN', k)).decode()
                      for k in (b'name', b'desc', b'hidden')]
    out['num_ach'] = call(stats, 14, ctypes.c_uint32)
    assert call(stats, 10, ctypes.c_bool)
    time.sleep(0.4)
    # UGC: details by published file id with metadata (UGC 020, SteamUGCDetails_t 9772 bytes).
    ugc = call(client, 12, ctypes.c_void_p, gtypes, (user, pipe, b'STEAMUGC_INTERFACE_VERSION020'))
    assert ugc
    ids = (ctypes.c_uint64 * 1)(555)
    query = call(ugc, 3, ctypes.c_uint64, (ctypes.c_void_p, ctypes.c_uint32), (ids, 1))
    assert query
    assert call(ugc, 26, ctypes.c_bool, (ctypes.c_uint64, ctypes.c_bool), (query, True))
    c = call(ugc, 4, ctypes.c_uint64, (ctypes.c_uint64,), (query,))
    handle, status, count, total = struct.unpack_from('<QiII', result(c, 280, 3401))
    details = ctypes.create_string_buffer(9772)
    ok = call(ugc, 5, ctypes.c_bool, (ctypes.c_uint64, ctypes.c_uint32, ctypes.c_void_p), (query, 0, details))
    raw = details.raw
    meta = ctypes.create_string_buffer(64)
    got_meta = call(ugc, 10, ctypes.c_bool, (ctypes.c_uint64, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32), (query, 0, meta, 64))
    out['ugc'] = [status, count, ok, struct.unpack_from('<Qi', raw, 0), raw[24:29].decode(),
                  struct.unpack_from('<Q', raw, 8156)[0], struct.unpack_from('<Q', raw, 9764)[0],
                  got_meta, meta.value.decode()]
    assert call(ugc, 21, ctypes.c_bool, (ctypes.c_uint64,), (query,))
    print(json.dumps(out))


class Tests(unittest.TestCase):
    def test_leaderboards_stats_and_achievements(self):
        with tempfile.TemporaryDirectory(prefix='steam-tui-stats-test-') as directory:
            path = str(Path(directory) / 'client.sock')
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            listener.listen()
            listener.settimeout(0.1)
            stop = threading.Event()
            stored = []
            errors = []

            def serve():
                while not stop.is_set():
                    try:
                        peer, _ = listener.accept()
                    except socket.timeout:
                        continue
                    with peer:
                        peer.settimeout(2)
                        raw = b''
                        while len(raw) < 4:
                            raw += peer.recv(4 - len(raw))
                        size, = struct.unpack('<I', raw)
                        body = b''
                        while len(body) < size:
                            body += peer.recv(size - len(body))
                        try:
                            request = json.loads(body)
                            op = request['operation']
                            response = self.answer(op, stored)
                        except Exception as error:  # noqa: BLE001 - reported below
                            errors.append(repr(error))
                            response = {'Error': {'message': 'mock failure'}}
                        data = json.dumps({'version': 1, 'app_id': APP, 'response': response}).encode()
                        peer.sendall(struct.pack('<I', len(data)) + data)

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            env = dict(os.environ, STEAM_TUI_BACKEND_SOCKET=path, SteamAppId=str(APP))
            done = subprocess.run([sys.executable, __file__, '--child'], env=env,
                                  capture_output=True, text=True, timeout=30)
            stop.set()
            thread.join()
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(errors, [])
            out = json.loads(done.stdout.strip().splitlines()[-1])
        self.assertEqual(out['name'], 'Ghosts')
        self.assertEqual(out['count'], 3)
        self.assertEqual(out['entries'], 2)
        self.assertEqual(out['entry'], [True, 1234, 2, 80, 2, [5, 6]])
        self.assertEqual(out['upload'], [1, 99, 1, 1, 3])
        self.assertEqual(out['games'], 4)
        self.assertAlmostEqual(out['speed'], 2.5)
        self.assertFalse(out['ach_before'])
        self.assertTrue(out['ach_after'])
        self.assertEqual(out['display'], ['Winner', 'Win once', '1'])
        self.assertEqual(out['num_ach'], 1)
        self.assertEqual(out['ugc'], [1, 1, True, [555, 1], 'Run 7', 4242, 99, True, '{"run":1}'])
        # The store sent the edited Games stat and the achievement bit.
        self.assertEqual(sorted(map(tuple, stored[0]['stats'])), [(1, 9), (3, 1)])

    @staticmethod
    def answer(op, stored):
        if op == 'Identity':
            return {'Identity': {'steam_id': ME, 'logged_on': True}}
        if 'Ugc' in op:
            return {'UgcDetails': {'result': 1, 'items': [{'id': 555, 'result': 1, 'title': 'Run 7', 'owner': 4242, 'file_size': 99, 'metadata': list(b'{"run":1}')}]}}
        stats = op['Stats']
        if 'FindLeaderboard' in stats:
            assert stats['FindLeaderboard']['name'] == 'Ghosts'
            return {'Board': {'result': 1, 'found': True, 'id': 7, 'name': 'Ghosts', 'entries': 3, 'sort': 2, 'display': 1}}
        if 'LeaderboardEntries' in stats:
            assert stats['LeaderboardEntries']['board'] == 7
            entries = [
                {'steam_id': 99, 'rank': 1, 'score': 100, 'details': [], 'ugc': 0},
                {'steam_id': 1234, 'rank': 2, 'score': 80,
                 'details': list(struct.pack('<ii', 5, 6)), 'ugc': 0},
            ]
            return {'BoardEntries': {'result': 1, 'total': 3, 'entries': entries}}
        if 'UploadScore' in stats:
            assert stats['UploadScore']['score'] == 99
            assert bytes(stats['UploadScore']['details']) == struct.pack('<ii', 11, 22)
            return {'ScoreUploaded': {'success': True, 'changed': True, 'total': 4, 'rank_new': 1, 'rank_previous': 3}}
        if 'GetStats' in stats:
            return {'StatsData': {'result': 1, 'crc': 5, 'schema': kv_schema().hex(),
                                  'stats': [[1, 4], [2, struct.unpack('<I', struct.pack('<f', 2.5))[0]]],
                                  'unlocks': []}}
        if 'Ugc' in op:
            return {'UgcDetails': {'result': 1, 'items': [{'id': 555, 'result': 1, 'title': 'Run 7', 'owner': 4242, 'file_size': 99, 'metadata': list(b'{"run":1}')}]}}
        if 'StoreStats' in stats:
            stored.append(stats['StoreStats'])
            return {'StatsStored': {'result': 1, 'crc': 6, 'failed': [], 'out_of_date': False}}
        raise AssertionError(op)


if __name__ == '__main__':
    if '--child' in sys.argv:
        ffi_child()
    else:
        unittest.main()

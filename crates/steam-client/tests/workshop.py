#!/usr/bin/env python3
"""ISteamUGC queries, subscriptions, install state and Workshop writes through the
real shared library against a mock backend.

Build: cargo build -p steam-client
Run: python3 crates/steam-client/tests/workshop.py
No account or CM traffic: a thread answers the Ugc IPC operations.
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

APP = 4000
CLIENT = Path(os.environ.get('STEAM_TUI_TEST_CLIENT',
                             Path(__file__).resolve().parents[3] / 'target/debug/libsteamclient.so'))
U64, U32, I32, BOOL, PTR, STR = ctypes.c_uint64, ctypes.c_uint32, ctypes.c_int, ctypes.c_bool, ctypes.c_void_p, ctypes.c_char_p


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

    pipe = call(client, 0, I32)
    user = call(client, 2, I32, (I32,), (pipe,))
    gtypes = (I32, I32, STR)
    ugc = call(client, 12, PTR, gtypes, (user, pipe, b'STEAMUGC_INTERFACE_VERSION017'))
    utils = call(client, 9, PTR, (I32, STR), (pipe, b'SteamUtils010'))
    assert ugc and utils

    def result(call_id, size, expected):
        for _ in range(100):
            if call(utils, 11, BOOL, (U64, PTR), (call_id, ctypes.byref(ctypes.c_bool()))):
                break
            time.sleep(0.02)
        else:
            raise AssertionError('call never completed')
        buffer = ctypes.create_string_buffer(size)
        failed = ctypes.c_bool()
        ok = call(utils, 13, BOOL, (U64, PTR, I32, I32, PTR), (call_id, buffer, size, expected, ctypes.byref(failed)))
        assert ok and not failed.value
        return buffer.raw

    out = {}
    # A user-list query with filters, then every per-item getter.
    q = call(ugc, 0, U64, (U32, U32, I32, U32, U32, U32, U32), (0, 6, 0, 4, APP, APP, 2))
    assert q
    assert call(ugc, 20, BOOL, (U64, STR), (q, b'Map'))
    assert call(ugc, 22, BOOL, (U64, STR), (q, b'NSFW'))
    assert call(ugc, 24, BOOL, (U64, BOOL), (q, True))
    assert call(ugc, 26, BOOL, (U64, BOOL), (q, True))
    assert call(ugc, 35, BOOL, (U64, STR), (q, b'tank'))
    assert call(ugc, 39, BOOL, (U64, STR, STR), (q, b'mode', b'ctf'))
    assert not call(ugc, 20, BOOL, (U64, STR), (999, b'x'))
    c = call(ugc, 4, U64, (U64,), (q,))
    _, status, count, total, cached = struct.unpack_from('<QiIIB', result(c, 280, 3401))
    cursor = result(call(ugc, 4, U64, (U64,), (q,)), 280, 3401)[21:].split(b'\0')[0].decode()
    out['query'] = [status, count, total, cursor]
    buf = ctypes.create_string_buffer(64)
    out['tags'] = [call(ugc, 6, U32, (U64, U32), (q, 0)),
                   call(ugc, 7, BOOL, (U64, U32, U32, PTR, U32), (q, 0, 1, buf, 64)), buf.value.decode(),
                   call(ugc, 8, BOOL, (U64, U32, U32, PTR, U32), (q, 0, 1, buf, 64)), buf.value.decode()]
    out['preview_url'] = [call(ugc, 9, BOOL, (U64, U32, PTR, U32), (q, 0, buf, 64)), buf.value.decode()]
    kids = (ctypes.c_uint64 * 4)()
    assert call(ugc, 11, BOOL, (U64, U32, PTR, U32), (q, 0, kids, 4))
    out['children'] = list(kids)[:2]
    stat = ctypes.c_uint64()
    assert call(ugc, 12, BOOL, (U64, U32, U32, PTR), (q, 0, 0, ctypes.byref(stat)))
    out['subs'] = stat.value
    out['previews'] = [call(ugc, 13, U32, (U64, U32), (q, 0))]
    url, orig, kind = ctypes.create_string_buffer(64), ctypes.create_string_buffer(64), ctypes.c_uint32()
    assert call(ugc, 14, BOOL, (U64, U32, U32, PTR, U32, PTR, U32, PTR), (q, 0, 0, url, 64, orig, 64, ctypes.byref(kind)))
    out['previews'] += [url.value.decode(), orig.value.decode(), kind.value]
    key, value = ctypes.create_string_buffer(32), ctypes.create_string_buffer(32)
    out['kv'] = [call(ugc, 15, U32, (U64, U32), (q, 0)),
                 call(ugc, 16, BOOL, (U64, U32, U32, PTR, U32, PTR, U32), (q, 0, 0, key, 32, value, 32)), key.value.decode(), value.value.decode(),
                 call(ugc, 17, BOOL, (U64, U32, STR, PTR, U32), (q, 0, b'mode', value, 32)), value.value.decode(),
                 call(ugc, 17, BOOL, (U64, U32, STR, PTR, U32), (q, 0, b'nope', value, 32))]
    assert call(ugc, 19, BOOL, (U64,), (q,))

    # Total-only query with a cursor overload.
    q2 = call(ugc, 2, U64, (U32, I32, U32, U32, STR), (3, 0, APP, APP, b'*'))
    assert q2 and call(ugc, 29, BOOL, (U64, BOOL), (q2, True))
    c = call(ugc, 4, U64, (U64,), (q2,))
    _, status, count, total, _ = struct.unpack_from('<QiIIB', result(c, 280, 3401))
    out['total_only'] = [status, count, total]

    # Subscriptions and install state (snapshot served by the mock).
    out['num_sub'] = call(ugc, 70, U32)
    ids = (ctypes.c_uint64 * 4)()
    n = call(ugc, 71, U32, (PTR, U32), (ids, 4))
    out['subscribed'] = list(ids)[:n]
    out['state'] = [call(ugc, 72, U32, (U64,), (111,)), call(ugc, 72, U32, (U64,), (222,)), call(ugc, 72, U32, (U64,), (333,)), call(ugc, 72, U32, (U64,), (9,))]
    size, stamp, folder = ctypes.c_uint64(), ctypes.c_uint32(), ctypes.create_string_buffer(128)
    ok = call(ugc, 73, BOOL, (U64, PTR, PTR, U32, PTR), (111, ctypes.byref(size), folder, 128, ctypes.byref(stamp)))
    out['install'] = [ok, size.value, stamp.value, folder.value.decode(),
                      call(ugc, 73, BOOL, (U64, PTR, PTR, U32, PTR), (222, ctypes.byref(size), folder, 128, ctypes.byref(stamp))),
                      call(ugc, 73, BOOL, (U64, PTR, PTR, U32, PTR), (111, ctypes.byref(size), folder, 3, ctypes.byref(stamp)))]
    done, whole = ctypes.c_uint64(), ctypes.c_uint64()
    out['download'] = [call(ugc, 74, BOOL, (U64, PTR, PTR), (222, ctypes.byref(done), ctypes.byref(whole))), done.value, whole.value,
                       call(ugc, 74, BOOL, (U64, PTR, PTR), (111, ctypes.byref(done), ctypes.byref(whole)))]
    out['download_item'] = [call(ugc, 75, BOOL, (U64, BOOL), (444, True))]
    call(ugc, 77, None, (BOOL,), (True,))
    c = call(ugc, 68, U64, (U64,), (555,))
    r, sub_id = struct.unpack_from('<iQ', result(c, 12, 1313)[:12] if False else result(c, 12, 1313))
    out['subscribe'] = [r, sub_id]
    c = call(ugc, 69, U64, (U64,), (555,))
    out['unsubscribe'] = list(struct.unpack_from('<iQ', result(c, 12, 1315)))

    # Votes, dependencies, playtime, deletion.
    c = call(ugc, 64, U64, (U64, BOOL), (555, True))
    out['vote'] = list(struct.unpack_from('<QiB', result(c, 16, 3408)))
    c = call(ugc, 65, U64, (U64,), (555,))
    out['get_vote'] = list(struct.unpack_from('<QiBBB', result(c, 16, 3409)))
    c = call(ugc, 66, U64, (U32, U64), (APP, 555))
    out['favorite'] = list(struct.unpack_from('<Qi', result(c, 16, 3407)))
    c = call(ugc, 81, U64, (U64, U64), (1, 2))
    out['dependency'] = list(struct.unpack_from('<iQQ', result(c, 20, 3412)))
    c = call(ugc, 83, U64, (U64, U32), (1, 480))
    out['app_dependency'] = list(struct.unpack_from('<iQI', result(c, 16, 3414)))
    c = call(ugc, 85, U64, (U64,), (1,))
    raw = result(c, 148, 3416)
    out['app_deps'] = [struct.unpack_from('<iQ', raw), list(struct.unpack_from('<2I', raw, 12)), struct.unpack_from('<II', raw, 140)]
    pids = (ctypes.c_uint64 * 2)(7, 8)
    c = call(ugc, 78, U64, (PTR, U32), (pids, 2))
    out['playtime'] = struct.unpack_from('<i', result(c, 4, 3410))[0]
    c = call(ugc, 80, U64)
    out['stop_all'] = struct.unpack_from('<i', result(c, 4, 3411))[0]
    c = call(ugc, 86, U64, (U64,), (555,))
    out['delete'] = list(struct.unpack_from('<iQ', result(c, 12, 3417)))

    # Publishing is not supported and says so.
    c = call(ugc, 41, U64, (U32, I32), (APP, 0))
    out['create'] = struct.unpack_from('<i', result(c, 16, 3403))[0]
    assert call(ugc, 42, U64, (U32, U64), (APP, 0)) == 0
    c = call(ugc, 62, U64, (U64, STR), (1, b'note'))
    out['submit'] = struct.unpack_from('<i', result(c, 16, 3404))[0]
    print(json.dumps(out))


ITEMS = [
    {'id': 111, 'subscribed': True, 'installed': True, 'size': 1234, 'time_updated': 77, 'path': '/lib/workshop/content/4000/111'},
    {'id': 222, 'subscribed': True, 'needs_update': True, 'downloading': True, 'done': 10, 'total': 100},
    {'id': 333, 'subscribed': False, 'installed': True, 'legacy': True, 'pending': True},
]


class Tests(unittest.TestCase):
    def test_workshop(self):
        with tempfile.TemporaryDirectory(prefix='steam-tui-workshop-test-') as directory:
            path = str(Path(directory) / 'client.sock')
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            listener.listen()
            listener.settimeout(0.1)
            stop = threading.Event()
            seen = []
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
                            op = json.loads(body)['operation']
                            response = self.answer(op, seen)
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
        self.assertEqual(out['query'], [1, 1, 250, 'NEXT'])
        self.assertEqual(out['tags'], [2, True, 'Map', True, 'Maps'])
        self.assertEqual(out['preview_url'], [True, 'https://img/preview.jpg'])
        self.assertEqual(out['children'], [5, 6])
        self.assertEqual(out['subs'], 42)
        self.assertEqual(out['previews'], [1, 'abc123', 'shot.png', 1])
        self.assertEqual(out['kv'], [1, True, 'mode', 'ctf', True, 'ctf', False])
        self.assertEqual(out['total_only'], [1, 0, 99])
        self.assertEqual(out['num_sub'], 2)
        self.assertEqual(out['subscribed'], [111, 222])
        self.assertEqual(out['state'], [1 | 4, 1 | 8 | 16, 2 | 4 | 32, 0])
        self.assertEqual(out['install'], [True, 1234, 77, '/lib/workshop/content/4000/111', False, False])
        self.assertEqual(out['download'], [True, 10, 100, False])
        self.assertEqual(out['download_item'], [True])
        self.assertEqual(out['subscribe'], [1, 555])
        self.assertEqual(out['unsubscribe'], [1, 555])
        self.assertEqual(out['vote'], [555, 1, 1])
        self.assertEqual(out['get_vote'], [555, 1, 1, 0, 0])
        self.assertEqual(out['favorite'], [555, 2])
        self.assertEqual(out['dependency'], [1, 1, 2])
        self.assertEqual(out['app_dependency'], [1, 1, 480])
        self.assertEqual(out['app_deps'], [[1, 1], [480, 570], [2, 2]])
        self.assertEqual(out['playtime'], 1)
        self.assertEqual(out['stop_all'], 1)
        self.assertEqual(out['delete'], [1, 555])
        self.assertEqual((out['create'], out['submit']), (2, 2))
        # The first query reached the backend with everything the game set.
        search = next(op for op in seen if isinstance(op, dict) and 'Query' in op)['Query']
        self.assertEqual(search['user_list'], 6)
        self.assertEqual(search['sort_order'], 4)
        self.assertEqual(search['page'], 2)
        self.assertEqual(search['required_tags'], ['Map'])
        self.assertEqual(search['excluded_tags'], ['NSFW'])
        self.assertEqual(search['search'], 'tank')
        self.assertEqual(search['kv_tags'], [['mode', 'ctf']])
        self.assertTrue(search['key_value_tags'] and search['metadata'])
        self.assertIn({'Download': {'id': 444, 'high_priority': True}}, seen)
        self.assertIn({'Suspend': {'on': True}}, seen)
        self.assertIn({'Playtime': {'app': 0, 'ids': [7, 8], 'start': True}}, seen)

    @staticmethod
    def answer(op, seen):
        if op == 'Identity':
            return {'Identity': {'steam_id': 1, 'logged_on': True}}
        if op == 'PollCallbacks':
            return {'Callbacks': []}
        ugc = op['Ugc']
        seen.append(ugc)
        if ugc == 'Snapshot':
            return {'UgcSnapshot': {'ready': True, 'items': ITEMS}}
        if 'Query' in ugc:
            q = ugc['Query']
            if q['total_only']:
                return {'UgcQuery': {'result': 1, 'total': 99, 'cursor': '', 'items': []}}
            item = {'id': 9, 'result': 1, 'title': 'Tank', 'tag_list': [['Mod', 'Mods'], ['Map', 'Maps']],
                    'preview_url': 'https://img/preview.jpg', 'child_ids': [5, 6], 'stats': [[0, 42]],
                    'previews': [{'url': 'abc123', 'original': 'shot.png', 'kind': 1}],
                    'kv_tags': [['mode', 'ctf']]}
            return {'UgcQuery': {'result': 1, 'total': 250, 'cursor': 'NEXT', 'items': [item]}}
        if 'Vote' in ugc:
            return {'UgcResult': {'result': 1}}
        if 'GetVote' in ugc:
            return {'UgcVote': {'result': 1, 'voted_up': True, 'voted_down': False, 'skipped': False}}
        if 'Favorite' in ugc:
            return {'UgcResult': {'result': 2}}
        if 'GetAppDependencies' in ugc:
            return {'UgcAppDependencies': {'result': 1, 'apps': [480, 570]}}
        if any(key in ugc for key in ('Subscribe', 'Playtime', 'Dependency', 'AppDependency', 'Delete', 'Download', 'Suspend')):
            return {'UgcResult': {'result': 1}}
        raise AssertionError(ugc)


if __name__ == '__main__':
    if '--child' in sys.argv:
        ffi_child()
    else:
        unittest.main()

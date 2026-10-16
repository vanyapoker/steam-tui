#!/usr/bin/env python3
"""Subprocess FFI tests with a mock local service; no account or CM traffic.

Build: cargo build -p steam-client
Run: python3 crates/steam-client/tests/bridge.py
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

TEST_ID = 76561198000000000
CLIENT = Path(os.environ.get('STEAM_TUI_TEST_CLIENT', Path(__file__).resolve().parents[3] / 'target/debug/libsteamclient.so'))


def ffi_child():
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
    args = (user, pipe, b'SteamUser021')
    types = (ctypes.c_int, ctypes.c_int, ctypes.c_char_p)
    identity = call(client, 5, ctypes.c_void_p, types, args)
    logged_on = call(identity, 1, ctypes.c_bool)
    steam_id = call(identity, 2, ctypes.c_uint64)
    identity23 = call(client, 5, ctypes.c_void_p, types, (user, pipe, b'SteamUser023'))
    assert identity23 and identity23 != identity
    assert call(identity23, 2, ctypes.c_uint64) == steam_id
    apps = call(client, 15, ctypes.c_void_p, types,
                (user, pipe, b'STEAMAPPS_INTERFACE_VERSION008'))
    owned = call(apps, 0, ctypes.c_bool)
    assert not call(apps, 6, ctypes.c_bool, (ctypes.c_uint32,), (570,))
    result = [steam_id, logged_on, owned]
    if os.environ.get('STEAM_TUI_TEST_PERSONA'):
        friends = call(client, 8, ctypes.c_void_p, types, (user, pipe, b'SteamFriends017'))
        result.extend([call(friends, 0, ctypes.c_char_p).decode(), call(friends, 2, ctypes.c_int)])
        utils = call(client, 9, ctypes.c_void_p, (ctypes.c_int, ctypes.c_char_p), (pipe, b'SteamUtils010'))
        result.append(call(utils, 4, ctypes.c_char_p).decode())
    if os.environ.get('STEAM_TUI_TEST_REFRESH'):
        time.sleep(0.3)
        result.extend([call(identity, 2, ctypes.c_uint64), call(identity, 1, ctypes.c_bool),
                       call(apps, 0, ctypes.c_bool)])
    print(json.dumps(result))


def read_exact(stream, size):
    data = b''
    while len(data) < size:
        part = stream.recv(size - len(data))
        if not part:
            raise EOFError()
        data += part
    return data


class BridgeTests(unittest.TestCase):
    def exercise(self, transform=lambda reply: reply, owned=True, refresh=False, real_api=None, tickets=False, persona=False, web_tickets=False):
        with tempfile.TemporaryDirectory(prefix='steam-tui-ipc-test-') as directory:
            path = str(Path(directory) / 'client.sock')
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            listener.listen()
            listener.settimeout(0.1)
            stop = threading.Event()
            errors = []
            active_tickets = set()
            cancellations = []
            callbacks = []
            next_web = 1000

            def serve():
                nonlocal next_web
                while not stop.is_set():
                    try:
                        peer, _ = listener.accept()
                    except socket.timeout:
                        continue
                    try:
                        with peer:
                            peer.settimeout(2)
                            size, = struct.unpack('<I', read_exact(peer, 4))
                            request = json.loads(read_exact(peer, size))
                            assert request['version'] == 1 and request['app_id'] == 440
                            operation = request['operation']
                            if operation == 'Identity':
                                response = {'Identity': {'steam_id': TEST_ID, 'logged_on': True}}
                            elif operation == 'Ownership':
                                response = {'Ownership': {'owned': owned}}
                            elif operation == 'Persona':
                                response = {'Persona': {'steam_id': TEST_ID, 'name': 'Test Persona', 'state': 1}}
                            elif operation == 'Country':
                                response = {'Country': {'code': 'DE'}}
                            elif operation == 'GcRecv':
                                response = 'GcNone'
                            elif operation == 'PollCallbacks':
                                response = {'Callbacks': list(callbacks)}
                                callbacks.clear()
                            elif 'WebAuthTicket' in operation:
                                identity = bytes(operation['WebAuthTicket']['identity'])
                                assert len(identity) <= 31 and b'\0' not in identity
                                next_web += 1
                                active_tickets.add(next_web)
                                failed = identity == b'steam-tui-probe-failure'
                                ticket = b'' if failed else b'fixture-not-a-real-Steam-ticket'
                                payload = struct.pack('<Iii', next_web, 2 if failed else 1, len(ticket)) + ticket
                                payload += bytes(2572 - len(payload))
                                callbacks.append([168, payload.hex()])
                                response = {'AuthTicketPending': {'handle': next_web}}
                            elif 'AuthSessionTicket' in operation:
                                active_tickets.add(123)
                                response = {'AuthTicket': {'handle': 123, 'ticket': [1, 2, 3, 4]}}
                            elif 'CancelAuthTicket' in operation:
                                handle = operation['CancelAuthTicket']['handle']
                                assert handle in active_tickets
                                active_tickets.remove(handle)
                                cancellations.append(handle)
                                response = 'Cancelled'
                            else:
                                raise AssertionError('Unexpected operation')
                            reply = transform({'version': 1, 'app_id': 440, 'response': response})
                            data = json.dumps(reply).encode()
                            peer.sendall(struct.pack('<I', len(data)) + data)
                    except Exception as error:
                        errors.append(error)

            worker = threading.Thread(target=serve)
            worker.start()
            try:
                env = dict(os.environ, SteamAppId='440', STEAM_TUI_BACKEND_SOCKET=path)
                if refresh:
                    env['STEAM_TUI_TEST_REFRESH'] = '1'
                if web_tickets:
                    env['STEAM_TUI_WEB_AUTH_FIXTURE'] = '1'
                if persona:
                    env['STEAM_TUI_TEST_PERSONA'] = '1'
                command = ([sys.executable, str(Path(__file__).resolve().parents[1] / 'probe.py'),
                            real_api, '440', str(CLIENT), '--socket', path] if real_api else
                           [sys.executable, __file__, '--child'])
                if web_tickets:
                    command.append('--web-tickets')
                if tickets:
                    command.append('--tickets')
                result = subprocess.run(command, env=env,
                                        capture_output=True, text=True, check=True, timeout=15)
                if tickets:
                    assert not active_tickets and len(cancellations) == 2
                if web_tickets:
                    assert not active_tickets and len(cancellations) == 10
                return result.stdout if real_api else json.loads(result.stdout)
            finally:
                stop.set()
                worker.join(timeout=3)
                listener.close()
                assert not worker.is_alive()
                assert not errors, errors

    def test_identity_and_ownership_reach_cpp_vtables(self):
        self.assertEqual(self.exercise(), [TEST_ID, True, True])

    def test_ownership_denial_is_preserved(self):
        self.assertEqual(self.exercise(owned=False), [TEST_ID, True, False])

    def test_friends_persona_comes_from_backend(self):
        self.assertEqual(self.exercise(persona=True), [TEST_ID, True, True, 'Test Persona', 1, 'DE'])

    def test_country_rejects_malformed_cm_metadata(self):
        def invalid_country(reply):
            return dict(reply, response={'Country': {'code': 'BAD\0COUNTRY'}}) if 'Country' in reply['response'] else reply
        self.assertEqual(self.exercise(invalid_country, persona=True), [TEST_ID, True, True, 'Test Persona', 1, ''])

    def test_wrong_app_and_protocol_do_not_leak_identity_or_grant_ownership(self):
        for field, wrong in [('app_id', 570), ('version', 99)]:
            with self.subTest(field=field):
                self.assertEqual(self.exercise(lambda r: dict(r, **{field: wrong})), [0, False, False])

    def test_backend_error_fails_closed(self):
        self.assertEqual(self.exercise(lambda r: dict(r, response={'Error': {'message': 'disconnected'}})),
                         [0, False, False])

    def test_refresh_failure_clears_identity_and_ownership(self):
        calls = 0

        def disconnect(reply):
            nonlocal calls
            calls += 1
            return reply if calls <= 2 else dict(reply, response={'Error': {'message': 'disconnected'}})

        self.assertEqual(self.exercise(disconnect, refresh=True), [TEST_ID, True, True, 0, False, False])

    @unittest.skipUnless(os.environ.get('STEAM_TUI_TEST_STEAM_API'), 'optional real Steam API path not set')
    def test_unmodified_sdk_reads_mock_identity_and_ownership(self):
        output = self.exercise(real_api=os.environ['STEAM_TUI_TEST_STEAM_API'])
        self.assertIn('PASS: real SteamAPI_Init / identity / ownership', output)

    @unittest.skipUnless(os.environ.get('STEAM_TUI_TEST_STEAM_API'), 'optional real Steam API path not set')
    def test_sdk_delivers_auth_callback_and_cancels_tickets(self):
        output = self.exercise(real_api=os.environ['STEAM_TUI_TEST_STEAM_API'], tickets=True)
        self.assertEqual(output.count('SDK callback delivered'), 2)

    @unittest.skipUnless(os.environ.get('STEAM_TUI_TEST_STEAM_API'), 'optional real Steam API path not set')
    def test_sdk_delivers_web_callback_and_discards_canceled_completions(self):
        output = self.exercise(real_api=os.environ['STEAM_TUI_TEST_STEAM_API'], web_tickets=True)
        self.assertEqual(output.count('web ticket callback 168 through SDK'), 2)


if __name__ == '__main__':
    if sys.argv[1:] == ['--child']:
        ffi_child()
    else:
        unittest.main()

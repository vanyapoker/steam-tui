#!/usr/bin/env python3
"""Exercise Valve's unmodified libsteam_api.so with our client in a private HOME.

Build first: cargo build -p steam-client
Run: python3 crates/steam-client/probe.py /path/to/libsteam_api.so APPID
Never invokes SteamAPI_RestartAppIfNecessary, Steam, a game, or a login.
"""
import ctypes
import argparse
import os
import resource
from pathlib import Path
import subprocess
import sys
import tempfile


def interface_cycle():
    """Exercise both host C++ factory ABIs, including Proton's typed getters."""
    vp, i32, text = ctypes.c_void_p, ctypes.c_int, ctypes.c_char_p
    client = ctypes.CDLL(os.environ['STEAM_TUI_PROBE_CLIENT'])
    client.CreateInterface.argtypes = [text, vp]
    client.CreateInterface.restype = vp
    common = [
        (5, b'SteamUser021'), (5, b'SteamUser023'),
        (8, b'SteamFriends017'), (10, b'SteamMatchMaking009'),
        (11, b'SteamMatchMakingServers002'),
        (13, b'STEAMUSERSTATS_INTERFACE_VERSION012'),
        (15, b'STEAMAPPS_INTERFACE_VERSION008'), (16, b'SteamNetworking006'),
        (17, b'STEAMREMOTESTORAGE_INTERFACE_VERSION016'),
        (18, b'STEAMSCREENSHOTS_INTERFACE_VERSION003'),
    ]
    tail = [
        b'STEAMHTTP_INTERFACE_VERSION003', None, b'SteamController008',
        b'STEAMUGC_INTERFACE_VERSION017', b'STEAMAPPLIST_INTERFACE_VERSION001',
        b'STEAMMUSIC_INTERFACE_VERSION001', b'STEAMMUSICREMOTE_INTERFACE_VERSION001',
        b'STEAMHTMLSURFACE_INTERFACE_VERSION_005', None, None, None,
        b'STEAMINVENTORY_INTERFACE_V003', b'STEAMVIDEO_INTERFACE_V007',
        b'STEAMPARENTALSETTINGS_INTERFACE_VERSION001',
    ]
    for version, offset in [(b'SteamClient017', 23), (b'SteamClient020', 24)]:
        obj = client.CreateInterface(version, None)
        assert obj, version
        vtable = ctypes.cast(obj, ctypes.POINTER(ctypes.POINTER(vp))).contents
        def method(slot, result, *args):
            return ctypes.CFUNCTYPE(result, vp, *args)(vtable[slot])
        create = method(0, i32)
        release = method(1, ctypes.c_bool, i32)
        connect = method(2, i32, i32)
        release_user = method(4, None, i32, i32)
        generic = method(12, vp, i32, i32, text)
        pipe = create(obj)
        assert pipe > 0 and connect(obj, pipe) == 1
        try:
            entries = common + [(offset + n, name) for n, name in enumerate(tail) if name]
            if version == b'SteamClient020':
                entries += [(19, b'SteamMatchGameSearch001'), (38, b'SteamInput006'),
                            (40, b'STEAMREMOTEPLAY_INTERFACE_VERSION001'), (39, b'SteamParties002')]
            entries += [(offset + 3, b'STEAMUGC_INTERFACE_VERSION016'),
                        (offset + 12, b'STEAMVIDEO_INTERFACE_V002')]
            for slot, name in entries:
                getter = method(slot, vp, i32, i32, text)
                expected = generic(obj, 1, pipe, name)
                assert expected and getter(obj, 1, pipe, name) == expected, (version, slot, name)
                assert not getter(obj, 1, pipe, name + b'_unsupported')
                assert not getter(obj, 1, pipe, b'SteamUtils010'), (version, slot, 'wrong family')
                assert not getter(obj, 1, -1, name), (version, slot, 'invalid pipe')
                assert not getter(obj, 0, pipe, name), (version, slot, 'invalid user')
            utils = method(9, vp, i32, text)
            for name in [b'SteamUtils009', b'SteamUtils010']:
                assert utils(obj, pipe, name) == generic(obj, 0, pipe, name)
                assert not utils(obj, -1, name)
            # ABI landmarks on both sides of UGC016's omitted descriptor slots.
            old = generic(obj, 1, pipe, b'STEAMUGC_INTERFACE_VERSION016')
            new = generic(obj, 1, pipe, b'STEAMUGC_INTERFACE_VERSION017')
            old_table = ctypes.cast(old, ctypes.POINTER(ctypes.POINTER(vp))).contents
            new_table = ctypes.cast(new, ctypes.POINTER(ctypes.POINTER(vp))).contents
            for old_slot, new_slot in [(17, 17), (18, 19), (58, 59), (59, 62), (67, 70), (85, 88)]:
                assert old_table[old_slot] == new_table[new_slot], ('UGC016 ABI', old_slot)
            browser = generic(obj, 1, pipe, b'SteamMatchMakingServers002')
            server_table = ctypes.cast(browser, ctypes.POINTER(ctypes.POINTER(vp))).contents
            for slot in (13, 14, 15):
                query = ctypes.CFUNCTYPE(i32, vp, ctypes.c_uint32, ctypes.c_uint16, vp)(server_table[slot])
                assert query(browser, 0, 0, None) == -1, ('invalid HServerQuery', slot)
            util = utils(obj, pipe, b'SteamUtils010')
            util_table = ctypes.cast(util, ctypes.POINTER(ctypes.POINTER(vp))).contents
            image_size = ctypes.CFUNCTYPE(ctypes.c_bool, vp, i32, vp, vp)(util_table[5])
            image_rgba = ctypes.CFUNCTYPE(ctypes.c_bool, vp, i32, vp, i32)(util_table[6])
            for handle in (0, -1, 123):
                width, height = ctypes.c_uint32(123), ctypes.c_uint32(456)
                assert not image_size(util, handle, ctypes.byref(width), ctypes.byref(height))
                assert (width.value, height.value) == (0, 0)
                assert not image_size(util, handle, None, None)
                assert not image_rgba(util, handle, None, 0)
            release_user(obj, pipe, 1)
            for slot, name in entries:
                assert not method(slot, vp, i32, i32, text)(obj, 1, pipe, name)
        finally:
            assert release(obj, pipe)
    print('PASS: SteamClient017/020 typed factories, generic parity, version/family/session checks', flush=True)


def ticket_cycle(api, identity):
    # CCallbackBase: three virtual methods, flags byte, then callback identifier.
    received = []
    callback = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_void_p)
    callback_result = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_void_p,
                                      ctypes.c_bool, ctypes.c_uint64)
    get_size = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p)

    def receive(_self, data):
        result = ctypes.cast(data, ctypes.POINTER(ctypes.c_uint32))
        received.append((result[0], result[1]))

    run = callback(receive)
    run_result = callback_result(lambda self, data, _failed, _call: receive(self, data))
    size = get_size(lambda _self: 8)
    table = (ctypes.c_void_p * 3)(*[ctypes.cast(fn, ctypes.c_void_p).value
                                  for fn in (run, run_result, size)])

    class Listener(ctypes.Structure):
        _fields_ = [('vtable', ctypes.POINTER(ctypes.c_void_p)),
                    ('flags', ctypes.c_uint8), ('callback', ctypes.c_int)]

    listener = Listener(table, 0, 0)
    api.SteamAPI_RegisterCallback.argtypes = [ctypes.c_void_p, ctypes.c_int]
    api.SteamAPI_RegisterCallback.restype = None
    api.SteamAPI_UnregisterCallback.argtypes = [ctypes.c_void_p]
    api.SteamAPI_UnregisterCallback.restype = None
    api.SteamAPI_ISteamUser_GetAuthSessionTicket.argtypes = [ctypes.c_void_p, ctypes.c_void_p,
                                                          ctypes.c_int, ctypes.POINTER(ctypes.c_uint32)]
    api.SteamAPI_ISteamUser_GetAuthSessionTicket.restype = ctypes.c_uint32
    api.SteamAPI_ISteamUser_CancelAuthTicket.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    api.SteamAPI_ISteamUser_CancelAuthTicket.restype = None
    api.SteamAPI_RegisterCallback(ctypes.byref(listener), 163)
    handle = 0
    try:
        buffer = ctypes.create_string_buffer(4096)
        length = ctypes.c_uint32(0)
        handle = api.SteamAPI_ISteamUser_GetAuthSessionTicket(identity, buffer, len(buffer), ctypes.byref(length))
        assert handle != 0 and 0 < length.value <= len(buffer), 'CM ticket issuance failed'
        api.SteamAPI_RunCallbacks()
        assert received == [(handle, 1)], f'Auth callback mismatch: {received}'
        print(f'PASS: CM-acknowledged auth ticket ({length.value} bytes), SDK callback delivered; bytes omitted')
    finally:
        if handle:
            api.SteamAPI_ISteamUser_CancelAuthTicket(identity, handle)
        api.SteamAPI_UnregisterCallback(ctypes.byref(listener))


def web_ticket_cycle(api):
    """Exercise User023 and SDK callback 168. Works against the offline fixture too."""
    import time
    vp = ctypes.c_void_p
    user = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamUser023')
    assert user
    table = ctypes.cast(user, ctypes.POINTER(ctypes.POINTER(vp)))[0]
    request = ctypes.CFUNCTYPE(ctypes.c_uint32, vp, ctypes.c_char_p)(table[14])
    cancel = ctypes.CFUNCTYPE(None, vp, ctypes.c_uint32)(table[17])
    received = []
    class Result(ctypes.Structure):
        _fields_ = [('handle', ctypes.c_uint32), ('result', ctypes.c_int32),
                    ('length', ctypes.c_int32), ('ticket', ctypes.c_uint8 * 2560)]
    assert ctypes.sizeof(Result) == 2572
    callback = ctypes.CFUNCTYPE(None, vp, vp)
    callback_result = ctypes.CFUNCTYPE(None, vp, vp, ctypes.c_bool, ctypes.c_uint64)
    get_size = ctypes.CFUNCTYPE(ctypes.c_int, vp)
    def receive(_self, data):
        result = ctypes.cast(data, ctypes.POINTER(Result)).contents
        assert 0 <= result.length <= 2560
        # No ticket bytes are printed or persisted.
        received.append((result.handle, result.result, result.length))
    run = callback(receive)
    run_result = callback_result(lambda self, data, _failed, _call: receive(self, data))
    size = get_size(lambda _self: ctypes.sizeof(Result))
    vtable = (vp * 3)(*[ctypes.cast(fn, vp).value for fn in (run, run_result, size)])
    class Listener(ctypes.Structure):
        _fields_ = [('vtable', ctypes.POINTER(vp)), ('flags', ctypes.c_uint8), ('callback', ctypes.c_int)]
    listener = Listener(vtable, 0, 0)
    api.SteamAPI_RegisterCallback.argtypes = [vp, ctypes.c_int]
    api.SteamAPI_UnregisterCallback.argtypes = [vp]
    api.SteamAPI_RegisterCallback(ctypes.byref(listener), 168)
    pending = set()
    def issue(identity):
        start = time.monotonic()
        handle = request(user, identity)
        assert handle and time.monotonic() - start < 1, 'web request blocked on ticket issuance'
        pending.add(handle)
        return handle
    def wait(handle, expected=1):
        end = time.monotonic() + 15
        while time.monotonic() < end and not any(h == handle for h, _, _ in received):
            api.SteamAPI_RunCallbacks()
            time.sleep(.01)
        matches = [item for item in received if item[0] == handle]
        assert len(matches) == 1 and matches[0][1] == expected, matches
        assert (matches[0][2] > 0) == (expected == 1)
        cancel(user, handle)
        pending.remove(handle)
    try:
        assert request(user, b'X' * 32) == 0
        wait(issue(b'steam-tui-probe'))
        canceled = issue(b'steam-tui-probe')
        cancel(user, canceled)
        pending.remove(canceled)
        # Allow a late backend completion to be polled after cancellation.
        for _ in range(12):
            api.SteamAPI_RunCallbacks()
            time.sleep(.01)
        assert not any(h == canceled for h, _, _ in received)
        wait(issue(None))
        wait(issue(b'X' * 31))
        if os.environ.get('STEAM_TUI_WEB_AUTH_FIXTURE'):
            wait(issue(b'steam-tui-probe-failure'), 2)
        print('PASS: web ticket callback 168 through SDK; identity bounds, cancellation and late completions; bytes omitted')
    finally:
        for handle in pending:
            cancel(user, handle)
        api.SteamAPI_UnregisterCallback(ctypes.byref(listener))


def lobby_cycle(api):
    """Create, edit, search and leave a real lobby through our backend (needs app 480)."""
    import time
    vp = ctypes.c_void_p
    mm = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamMatchMaking009')
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamUtils010')
    assert mm and utils
    def sig(name, args, res):
        fn = getattr(api, name); fn.argtypes = args; fn.restype = res; return fn
    create = sig('SteamAPI_ISteamMatchmaking_CreateLobby', [vp, ctypes.c_int, ctypes.c_int], ctypes.c_uint64)
    set_data = sig('SteamAPI_ISteamMatchmaking_SetLobbyData', [vp, ctypes.c_uint64, ctypes.c_char_p, ctypes.c_char_p], ctypes.c_bool)
    get_data = sig('SteamAPI_ISteamMatchmaking_GetLobbyData', [vp, ctypes.c_uint64, ctypes.c_char_p], ctypes.c_char_p)
    set_member = sig('SteamAPI_ISteamMatchmaking_SetLobbyMemberData', [vp, ctypes.c_uint64, ctypes.c_char_p, ctypes.c_char_p], None)
    get_member = sig('SteamAPI_ISteamMatchmaking_GetLobbyMemberData', [vp, ctypes.c_uint64, ctypes.c_uint64, ctypes.c_char_p], ctypes.c_char_p)
    owner = sig('SteamAPI_ISteamMatchmaking_GetLobbyOwner', [vp, ctypes.c_uint64], ctypes.c_uint64)
    members = sig('SteamAPI_ISteamMatchmaking_GetNumLobbyMembers', [vp, ctypes.c_uint64], ctypes.c_int)
    leave = sig('SteamAPI_ISteamMatchmaking_LeaveLobby', [vp, ctypes.c_uint64], None)
    request = sig('SteamAPI_ISteamMatchmaking_RequestLobbyList', [vp], ctypes.c_uint64)
    f_string = sig('SteamAPI_ISteamMatchmaking_AddRequestLobbyListStringFilter', [vp, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int], None)
    f_dist = sig('SteamAPI_ISteamMatchmaking_AddRequestLobbyListDistanceFilter', [vp, ctypes.c_int], None)
    by_index = sig('SteamAPI_ISteamMatchmaking_GetLobbyByIndex', [vp, ctypes.c_int], ctypes.c_uint64)
    completed = sig('SteamAPI_ISteamUtils_IsAPICallCompleted', [vp, ctypes.c_uint64, ctypes.POINTER(ctypes.c_bool)], ctypes.c_bool)
    result = sig('SteamAPI_ISteamUtils_GetAPICallResult', [vp, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.POINTER(ctypes.c_bool)], ctypes.c_bool)
    me = api.SteamAPI_ISteamUser_GetSteamID(api.SteamInternal_FindOrCreateUserInterface(1, b'SteamUser021'))

    def wait(call, size, expected):
        failed = ctypes.c_bool(False)
        for _ in range(100):
            api.SteamAPI_RunCallbacks()
            if completed(utils, call, ctypes.byref(failed)):
                buf = ctypes.create_string_buffer(size)
                ok = result(utils, call, buf, size, expected, ctypes.byref(failed))
                assert ok and not failed.value, 'call result failed'
                return buf.raw
            time.sleep(0.1)
        raise AssertionError('call did not complete')

    call = create(mm, 2, 4)  # public, 4 members
    raw = wait(call, 12, 513)
    res, lobby = ctypes.c_int32.from_buffer_copy(raw[:4]).value, ctypes.c_uint64.from_buffer_copy(raw[4:12]).value
    assert res == 1 and lobby, f'CreateLobby result {res} lobby {lobby}'
    print(f'PASS: lobby created ({lobby})')
    assert owner(mm, lobby) == me and members(mm, lobby) == 1
    started = time.monotonic()
    assert set_data(mm, lobby, b'name', b'steam-tui-probe')
    assert set_data(mm, lobby, b'mode', b'test')
    for index in range(40):
        assert set_data(mm, lobby, ('probe_%02d' % index).encode(), str(index).encode())
    elapsed = time.monotonic() - started
    assert elapsed < 2, 'Metadata setters blocked the game online-task thread: %.3fs' % elapsed
    print('PASS: 42 metadata setters admitted in %.3fs' % elapsed, flush=True)
    assert get_data(mm, lobby, b'name') == b'steam-tui-probe'
    set_member(mm, lobby, b'ready', b'yes')
    assert get_member(mm, lobby, me, b'ready') == b'yes'
    for _ in range(10):  # let the server echo settle
        api.SteamAPI_RunCallbacks(); time.sleep(0.1)
    assert get_data(mm, lobby, b'mode') == b'test', 'lobby data lost after the server echo'
    print('PASS: local lobby/member edits survive the server echo')
    found = []
    for attempt in range(6):
        f_string(mm, b'name', b'steam-tui-probe', 0)
        f_string(mm, b'probe_39', b'39', 0)
        f_dist(mm, 3)  # worldwide
        raw = wait(request(mm), 4, 510)
        count = ctypes.c_uint32.from_buffer_copy(raw).value
        found = [by_index(mm, i) for i in range(count)]
        if lobby in found:
            break
        time.sleep(1.5)
    assert lobby in found, f'own lobby not in search results {found}'
    print(f'PASS: lobby search with a string filter found it ({len(found)} result(s))')
    started = time.monotonic()
    leave(mm, lobby)
    elapsed = time.monotonic() - started
    assert elapsed < 2, 'LeaveLobby blocked on a server acknowledgement: %.3fs' % elapsed
    assert members(mm, lobby) == 0
    print('PASS: lobby left in %.3fs, local membership removed' % elapsed, flush=True)


def ping_codec_cycle(api):
    """Valve-compatible markers through the unmodified Steam SDK flat ABI."""
    vp, i32 = ctypes.c_void_p, ctypes.c_int
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamNetworkingUtils004')
    def sig(name, args, res):
        fn = getattr(api, name); fn.argtypes = args; fn.restype = res; return fn
    parse = sig('SteamAPI_ISteamNetworkingUtils_ParsePingLocationString', [vp, ctypes.c_char_p, vp], ctypes.c_bool)
    text = sig('SteamAPI_ISteamNetworkingUtils_ConvertPingLocationToString', [vp, vp, vp, i32], None)
    estimate = sig('SteamAPI_ISteamNetworkingUtils_EstimatePingTimeBetweenTwoLocations', [vp, vp, vp], i32)
    a, b, back = (ctypes.create_string_buffer(512) for _ in range(3))
    assert parse(utils, b'ams=21+3,fra=40+9/19+2,sto2=/35+4', a)
    assert parse(utils, b'ams=5+0,fra=8+1,sto2=10+0', b)
    assert estimate(utils, a, b) == 26 and estimate(utils, b, a) == 26
    output = ctypes.create_string_buffer(1024)
    text(utils, a, output, len(output))
    assert parse(utils, output.value, back) and back.raw == a.raw
    for invalid in [b'', b'ams=-1', b'ams=5001', b'ams=10+10000', b'ams=3/4/5', b'a_s=3']:
        assert not parse(utils, invalid, back), invalid
        assert back.raw == bytes(512)
    print('PASS: Valve ping marker forms, variance route selection, serialization, invalid input')


def ping_lobbies_cycle(api):
    """Read public lobby markers and estimate their pings. No join or metadata writes."""
    import time
    vp, i32, u64 = ctypes.c_void_p, ctypes.c_int, ctypes.c_uint64
    mm = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamMatchMaking009')
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamUtils010')
    net = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamNetworkingUtils004')
    def sig(name, args, res):
        fn = getattr(api, name); fn.argtypes = args; fn.restype = res; return fn
    distance = sig('SteamAPI_ISteamMatchmaking_AddRequestLobbyListDistanceFilter', [vp, i32], None)
    request = sig('SteamAPI_ISteamMatchmaking_RequestLobbyList', [vp], u64)
    result = sig('SteamAPI_ISteamUtils_GetAPICallResult', [vp, u64, vp, i32, i32, vp], ctypes.c_bool)
    by_index = sig('SteamAPI_ISteamMatchmaking_GetLobbyByIndex', [vp, i32], u64)
    count = sig('SteamAPI_ISteamMatchmaking_GetLobbyDataCount', [vp, u64], i32)
    data = sig('SteamAPI_ISteamMatchmaking_GetLobbyDataByIndex', [vp, u64, i32, vp, i32, vp, i32], ctypes.c_bool)
    parse = sig('SteamAPI_ISteamNetworkingUtils_ParsePingLocationString', [vp, ctypes.c_char_p, vp], ctypes.c_bool)
    estimate = sig('SteamAPI_ISteamNetworkingUtils_EstimatePingTimeFromLocalHost', [vp, vp], i32)
    local = sig('SteamAPI_ISteamNetworkingUtils_GetLocalPingLocation', [vp, vp], ctypes.c_float)
    location = ctypes.create_string_buffer(512)
    for _ in range(100):
        api.SteamAPI_RunCallbacks()
        if local(net, location) >= 0: break
        time.sleep(.1)
    else: raise AssertionError('No relay measurements')
    distance(mm, 3)
    call = request(mm)
    output, failed = ctypes.c_uint32(), ctypes.c_bool()
    for _ in range(100):
        api.SteamAPI_RunCallbacks()
        if result(utils, call, ctypes.byref(output), 4, 510, ctypes.byref(failed)):
            assert not failed.value
            break
        time.sleep(.1)
    else: raise AssertionError('Lobby listing timed out')
    found = measured = 0
    for i in range(output.value):
        lobby = by_index(mm, i)
        for j in range(count(mm, lobby)):
            key, value = ctypes.create_string_buffer(256), ctypes.create_string_buffer(8192)
            assert data(mm, lobby, j, key, len(key), value, len(value))
            if b'ping' not in key.value.lower(): continue
            found += 1
            assert parse(net, value.value, location), ('unrecognized live marker', key.value, value.value)
            ping = estimate(net, location)
            if ping >= 0: measured += 1
            # Location strings contain only public POP codes/timings, no player ids.
            print('Public lobby ping=%d ms (%d marker bytes)' % (ping, len(value.value)), flush=True)
    assert found and measured, (output.value, found, measured)
    print('PASS: read-only public lobby listing: %d lobbies, %d parsed locations, %d estimates' % (output.value, found, measured), flush=True)


def network_cycle(api):
    """Ping locations, address/identity text forms and message allocation via Valve's flat API."""
    import time
    vp = ctypes.c_void_p
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamNetworkingUtils004')
    assert utils
    def sig(name, args, res):
        fn = getattr(api, name); fn.argtypes = args; fn.restype = res; return fn
    local = sig('SteamAPI_ISteamNetworkingUtils_GetLocalPingLocation', [vp, vp], ctypes.c_float)
    to_text = sig('SteamAPI_ISteamNetworkingUtils_ConvertPingLocationToString', [vp, vp, ctypes.c_char_p, ctypes.c_int], None)
    parse = sig('SteamAPI_ISteamNetworkingUtils_ParsePingLocationString', [vp, ctypes.c_char_p, vp], ctypes.c_bool)
    estimate = sig('SteamAPI_ISteamNetworkingUtils_EstimatePingTimeFromLocalHost', [vp, vp], ctypes.c_int)
    estimate2 = sig('SteamAPI_ISteamNetworkingUtils_EstimatePingTimeBetweenTwoLocations', [vp, vp, vp], ctypes.c_int)
    ip_text = sig('SteamAPI_SteamNetworkingIPAddr_ToString', [vp, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_bool], None)
    ip_parse = sig('SteamAPI_SteamNetworkingIPAddr_ParseString', [vp, ctypes.c_char_p], ctypes.c_bool)
    id_text = sig('SteamAPI_SteamNetworkingIdentity_ToString', [vp, ctypes.c_char_p, ctypes.c_size_t], None)
    id_parse = sig('SteamAPI_SteamNetworkingIdentity_ParseString', [vp, ctypes.c_char_p], ctypes.c_bool)
    location = ctypes.create_string_buffer(512)
    for _ in range(60):
        if local(utils, location) >= 0:
            break
        time.sleep(0.5)
    else:
        raise AssertionError('no local ping location (backend pings never arrived)')
    text = ctypes.create_string_buffer(1024)
    to_text(utils, location, text, 1024)
    assert b'=' in text.value, text.value
    back = ctypes.create_string_buffer(512)
    assert parse(utils, text.value, back) and back.raw == location.raw
    assert estimate(utils, location) >= 0 and estimate2(utils, location, back) >= 0
    print('PASS: ping location -> text -> location (%d chars), estimates available' % len(text.value))
    addr = ctypes.create_string_buffer(18)
    assert ip_parse(addr, b'203.0.113.9:27015')
    out = ctypes.create_string_buffer(64)
    ip_text(addr, out, 64, True)
    assert out.value == b'203.0.113.9:27015', out.value
    ident = ctypes.create_string_buffer(136)
    assert id_parse(ident, b'steamid:76561198000000000')
    id_text(ident, out, 64)
    assert out.value == b'steamid:76561198000000000', out.value
    print('PASS: IP address and identity text forms round-trip')


def sockets_cycle(api, legacy_callbacks=False):
    """Real UDP loopback through Valve's flat API and our 012 adapter."""
    import time
    vp, u32 = ctypes.c_void_p, ctypes.c_uint32
    sock = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamNetworkingSockets012')
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamNetworkingUtils004')
    assert sock and utils
    def sig(name, args, result):
        fn = getattr(api, 'SteamAPI_ISteamNetworking' + name)
        fn.argtypes, fn.restype = args, result
        return fn
    listen = sig('Sockets_CreateListenSocketIP', [vp, vp, ctypes.c_int, vp], u32)
    address = sig('Sockets_GetListenSocketAddress', [vp, u32, vp], ctypes.c_bool)
    connect = sig('Sockets_ConnectByIPAddress', [vp, vp, ctypes.c_int, vp], u32)
    accept = sig('Sockets_AcceptConnection', [vp, u32], ctypes.c_int)
    close = sig('Sockets_CloseConnection', [vp, u32, ctypes.c_int, ctypes.c_char_p, ctypes.c_bool], ctypes.c_bool)
    close_listen = sig('Sockets_CloseListenSocket', [vp, u32], ctypes.c_bool)
    send = sig('Sockets_SendMessageToConnection', [vp, u32, vp, u32, ctypes.c_int, vp], ctypes.c_int)
    send_many = sig('Sockets_SendMessages', [vp, ctypes.c_int, vp, vp], None)
    receive = sig('Sockets_ReceiveMessagesOnPollGroup', [vp, u32, vp, ctypes.c_int], ctypes.c_int)
    group = sig('Sockets_CreatePollGroup', [vp], u32)
    assign = sig('Sockets_SetConnectionPollGroup', [vp, u32, u32], ctypes.c_bool)
    destroy = sig('Sockets_DestroyPollGroup', [vp, u32], ctypes.c_bool)
    config = sig('Utils_SetConfigValue', [vp, ctypes.c_int, ctypes.c_int, ctypes.c_size_t, ctypes.c_int, vp], ctypes.c_bool)
    allocate = sig('Utils_AllocateMessage', [vp, ctypes.c_int], vp)
    info = sig('Sockets_GetConnectionInfo', [vp, u32, vp], ctypes.c_bool)
    relay = sig('Utils_GetRelayNetworkStatus', [vp, vp], ctypes.c_int)
    assert relay(utils, None) == -102, 'unimplemented relay must remain gated'
    # Only this loopback test opts into unsigned localhost connections.
    allow = ctypes.c_int(1)
    assert config(utils, 23, 1, 0, 1, ctypes.byref(allow))
    state, accepted = {}, []
    callback_type = ctypes.CFUNCTYPE(None, vp)
    errors = []
    def on_status(data):
        raw = ctypes.string_at(data, 704)
        conn = int.from_bytes(raw[:4], 'little')
        status = int.from_bytes(raw[180:184], 'little', signed=True)
        state[conn] = status
        if status == 1 and conn not in accepted:
            incoming = ctypes.create_string_buffer(696)
            if info(sock, conn, incoming) and int.from_bytes(incoming.raw[144:148], 'little'):
                accepted.append(conn)
                if accept(sock, conn) != 1: errors.append('accept failed')
                if not assign(sock, conn, poll): errors.append('poll assignment failed')
    callback = callback_type(on_status)
    callback_ptr = vp(ctypes.cast(callback, vp).value)
    listener = None
    if legacy_callbacks:
        # Verify the SDK callback queue, in addition to the native function-pointer path.
        run_type = ctypes.CFUNCTYPE(None, vp, vp)
        result_type = ctypes.CFUNCTYPE(None, vp, vp, ctypes.c_bool, ctypes.c_uint64)
        size_type = ctypes.CFUNCTYPE(ctypes.c_int, vp)
        run = run_type(lambda _self, data: on_status(data))
        result = result_type(lambda _self, data, _failed, _call: on_status(data))
        size = size_type(lambda _self: 704)
        vtable = (vp * 3)(*[ctypes.cast(fn, vp).value for fn in (run, result, size)])
        class Listener(ctypes.Structure):
            _fields_ = [('vtable', ctypes.POINTER(vp)), ('flags', ctypes.c_uint8), ('callback', ctypes.c_int)]
        listener = Listener(vtable, 0, 0)
        api.SteamAPI_RegisterCallback.argtypes = [vp, ctypes.c_int]
        api.SteamAPI_RegisterCallback.restype = None
        api.SteamAPI_UnregisterCallback.argtypes = [vp]
        api.SteamAPI_UnregisterCallback.restype = None
        api.SteamAPI_RegisterCallback(ctypes.byref(listener), 1221)
    else:
        assert config(utils, 201, 1, 0, 5, ctypes.byref(callback_ptr))
    addr = ctypes.create_string_buffer(18)
    addr[10], addr[11], addr[12], addr[15] = b'\xff', b'\xff', b'\x7f', b'\x01'
    server = client = poll = 0
    try:
        import socket as socket_module
        with socket_module.socket(socket_module.AF_INET, socket_module.SOCK_DGRAM) as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        ctypes.c_uint16.from_buffer(addr, 16).value = port
        server = listen(sock, addr, 0, None)
        assert server and address(sock, server, addr)
        poll = group(sock)
        assert poll
        client = connect(sock, addr, 0, None)
        assert client
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            api.SteamAPI_RunCallbacks()
            if accepted and state.get(client) == 3 and state.get(accepted[0]) == 3: break
            time.sleep(.01)
        assert accepted and state.get(client) == 3 and state.get(accepted[0]) == 3, (state, errors)
        assert not errors, errors
        payload = b'steam-tui real UDP loopback'
        assert send(sock, client, payload, len(payload), 8, None) == 1
        def get_message(expected):
            deadline = time.monotonic() + 5
            out = vp()
            while time.monotonic() < deadline:
                api.SteamAPI_RunCallbacks()
                count = receive(sock, poll, ctypes.byref(out), 1)
                assert count >= 0
                if count:
                    message = ctypes.string_at(out, 216)
                    ptr = int.from_bytes(message[:8], 'little')
                    size = int.from_bytes(message[8:12], 'little')
                    release = ctypes.CFUNCTYPE(None, vp)(int.from_bytes(message[184:192], 'little'))
                    try: assert ctypes.string_at(ptr, size) == expected
                    finally: release(out)
                    return
                time.sleep(.01)
            raise AssertionError('UDP message receive timed out')
        get_message(payload)
        # Exercise SDK 1.57's four-argument SendMessages, allocation and native release.
        message = allocate(utils, len(payload))
        assert message
        ctypes.memmove(vp.from_address(message).value, payload, len(payload))
        u32.from_address(message + 12).value = client
        ctypes.c_int.from_address(message + 196).value = 8
        messages = (vp * 1)(message)
        result = ctypes.c_int64()
        send_many(sock, 1, messages, ctypes.byref(result))
        assert result.value > 0, result.value
        get_message(payload)
        print('PASS: real UDP connect/accept, state callbacks, reliable messages, SendMessages and poll group', flush=True)
    finally:
        for conn in [client] + accepted:
            if conn: close(sock, conn, 1000, b'probe complete', False)
        if server: close_listen(sock, server)
        if poll: destroy(sock, poll)
        # Dispatch final native callbacks before their Python function objects go away.
        time.sleep(.02)
        api.SteamAPI_RunCallbacks()
        empty = vp()
        config(utils, 201, 1, 0, 5, ctypes.byref(empty))
        if listener is not None:
            api.SteamAPI_UnregisterCallback(ctypes.byref(listener))



def messages_cycle(api):
    """Two-account Messages002 echo with SDK session callbacks and native SNP."""
    import time
    vp, i32, u32 = ctypes.c_void_p, ctypes.c_int, ctypes.c_uint32
    obj = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamNetworkingMessages002')
    assert obj
    def sig(name, args, result):
        fn = getattr(api, 'SteamAPI_ISteamNetworkingMessages_' + name)
        fn.argtypes, fn.restype = args, result
        return fn
    send = sig('SendMessageToUser', [vp, vp, vp, u32, i32, i32], i32)
    receive = sig('ReceiveMessagesOnChannel', [vp, i32, vp, i32], i32)
    accept = sig('AcceptSessionWithUser', [vp, vp], ctypes.c_bool)
    close = sig('CloseSessionWithUser', [vp, vp], ctypes.c_bool)
    close_channel = sig('CloseChannelWithUser', [vp, vp, i32], ctypes.c_bool)
    info = sig('GetSessionConnectionInfo', [vp, vp, vp, vp], i32)
    run_type = ctypes.CFUNCTYPE(None, vp, vp)
    result_type = ctypes.CFUNCTYPE(None, vp, vp, ctypes.c_bool, ctypes.c_uint64)
    size_type = ctypes.CFUNCTYPE(i32, vp)
    class Listener(ctypes.Structure):
        _fields_ = [('vtable', ctypes.POINTER(vp)), ('flags', ctypes.c_uint8), ('callback', i32)]
    api.SteamAPI_RegisterCallback.argtypes = [vp, i32]
    api.SteamAPI_UnregisterCallback.argtypes = [vp]
    callbacks, remotes, failures = [], [], []
    def requested(data):
        identity = ctypes.string_at(data, 136)
        if identity not in remotes: remotes.append(identity)
        if not accept(obj, data): failures.append(b'AcceptSessionWithUser failed')
    def failed(data):
        failures.append(ctypes.string_at(data, 696)[184:312].split(b'\0')[0])
    for callback_id, size, handler in [(1251, 136, requested), (1252, 696, failed)]:
        run = run_type(lambda _self, data, handler=handler: handler(data))
        result = result_type(lambda _self, data, _failed, _call, handler=handler: handler(data))
        get_size = size_type(lambda _self, size=size: size)
        table = (vp * 3)(*[ctypes.cast(fn, vp).value for fn in (run, result, get_size)])
        listener = Listener(table, 0, 0)
        callbacks.append((listener, table, run, result, get_size))
        api.SteamAPI_RegisterCallback(ctypes.byref(listener), callback_id)
    peer = int(os.environ.get('STEAM_TUI_PROBE_P2P_PEER', '0'))
    identity = ctypes.create_string_buffer(136)
    if peer:
        ctypes.c_int32.from_buffer(identity).value = 16
        ctypes.c_int32.from_buffer(identity, 4).value = 8
        ctypes.c_uint64.from_buffer(identity, 8).value = peer
    payload = bytes(i % 251 for i in range(int(os.environ.get('STEAM_TUI_PROBE_P2P_BYTES', '32'))))
    completion = b'messages echo received'
    sent = False
    try:
        if peer:
            assert send(obj, identity, payload, len(payload), 8, 7) == 1
        else:
            print('P2P LISTEN READY', flush=True)
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            api.SteamAPI_RunCallbacks()
            assert not failures, failures
            out = vp()
            count = receive(obj, 7, ctypes.byref(out), 1)
            assert count >= 0
            if not count:
                time.sleep(.01); continue
            raw = ctypes.string_at(out, 216)
            size = int.from_bytes(raw[8:12], 'little')
            data = ctypes.string_at(int.from_bytes(raw[:8], 'little'), size)
            remote = ctypes.create_string_buffer(raw[16:152], 136)
            release = ctypes.CFUNCTYPE(None, vp)(int.from_bytes(raw[184:192], 'little'))
            release(out)
            assert data == (completion if not peer and sent else payload)
            state = ctypes.create_string_buffer(696)
            assert info(obj, remote, state, None) == 3
            flags = int.from_bytes(state.raw[440:444], 'little')
            assert flags & 3 == 0, ('Messages must be authenticated and encrypted', flags)
            if os.environ.get('STEAM_TUI_FORCE_RELAY') == '1': assert flags & 16
            if not peer and not sent:
                assert remotes, 'SDK session request callback was not delivered'
                assert send(obj, remote, payload, len(payload), 8, 7) == 1
                sent = True
                continue
            if peer:
                assert send(obj, remote, completion, len(completion), 9, 7) == 1
                until = time.monotonic() + 2
                while time.monotonic() < until:
                    api.SteamAPI_RunCallbacks(); time.sleep(.01)
            print('PASS: Messages002 authenticated reliable echo, SDK session callback, channel and session APIs', flush=True)
            break
        else: raise AssertionError('Messages002 echo timed out')
    finally:
        for raw in remotes + ([identity.raw] if peer else []):
            remote = ctypes.create_string_buffer(raw, 136)
            close_channel(obj, remote, 7)
            close(obj, remote)
        for listener, *_ in callbacks:
            api.SteamAPI_UnregisterCallback(ctypes.byref(listener))


def p2p_cycle(api):
    """Two processes/accounts: Steam CM signaling, ICE and reliable echo."""
    import time
    vp, u32 = ctypes.c_void_p, ctypes.c_uint32
    sock = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamNetworkingSockets012')
    utils = api.SteamInternal_FindOrCreateUserInterface(0, b'SteamNetworkingUtils004')
    assert sock and utils
    def sig(name, args, result):
        fn = getattr(api, 'SteamAPI_ISteamNetworking' + name)
        fn.argtypes, fn.restype = args, result
        return fn
    listen = sig('Sockets_CreateListenSocketP2P', [vp, ctypes.c_int, ctypes.c_int, vp], u32)
    connect = sig('Sockets_ConnectP2P', [vp, vp, ctypes.c_int, ctypes.c_int, vp], u32)
    accept = sig('Sockets_AcceptConnection', [vp, u32], ctypes.c_int)
    close = sig('Sockets_CloseConnection', [vp, u32, ctypes.c_int, ctypes.c_char_p, ctypes.c_bool], ctypes.c_bool)
    close_listen = sig('Sockets_CloseListenSocket', [vp, u32], ctypes.c_bool)
    send = sig('Sockets_SendMessageToConnection', [vp, u32, vp, u32, ctypes.c_int, vp], ctypes.c_int)
    receive = sig('Sockets_ReceiveMessagesOnConnection', [vp, u32, vp, ctypes.c_int], ctypes.c_int)
    info = sig('Sockets_GetConnectionInfo', [vp, u32, vp], ctypes.c_bool)
    details = sig('Sockets_GetDetailedConnectionStatus', [vp, u32, vp, ctypes.c_int], ctypes.c_int)
    config = sig('Utils_SetConfigValue', [vp, ctypes.c_int, ctypes.c_int, ctypes.c_size_t, ctypes.c_int, vp], ctypes.c_bool)
    state, connections, errors = {}, [], []
    callback_type = ctypes.CFUNCTYPE(None, vp)
    def on_status(data):
        raw = ctypes.string_at(data, 704)
        conn = int.from_bytes(raw[:4], 'little')
        status = int.from_bytes(raw[180:184], 'little', signed=True)
        state[conn] = status
        if conn not in connections: connections.append(conn)
        if status == 1:
            incoming = ctypes.create_string_buffer(696)
            if info(sock, conn, incoming) and int.from_bytes(incoming.raw[144:148], 'little'):
                if accept(sock, conn) != 1: errors.append('accept failed')
        if status in (4, 5):
            errors.append(raw[188:316].split(b'\0')[0].decode(errors='replace'))
    callback = callback_type(on_status)
    callback_ptr = vp(ctypes.cast(callback, vp).value)
    assert config(utils, 201, 1, 0, 5, ctypes.byref(callback_ptr))
    peer = int(os.environ.get('STEAM_TUI_PROBE_P2P_PEER', '0'))
    server = 0
    try:
        if not peer:
            server = listen(sock, 7777, 0, None)
            assert server, 'P2P listen failed'
            print('P2P LISTEN READY', flush=True)
        else:
            identity = ctypes.create_string_buffer(136)
            ctypes.c_int32.from_buffer(identity).value = 16
            ctypes.c_int32.from_buffer(identity, 4).value = 8
            ctypes.c_uint64.from_buffer(identity, 8).value = peer
            conn = connect(sock, identity, 7777, 0, None)
            assert conn, 'P2P connect returned invalid handle'
            connections.append(conn)
        deadline = time.monotonic() + 35
        sent = received = False
        size = int(os.environ.get('STEAM_TUI_PROBE_P2P_BYTES', '32'))
        assert 1 <= size <= 512 * 1024
        payload = bytes(index % 251 for index in range(size))
        completion = b'steam-tui probe echo received'
        while time.monotonic() < deadline:
            api.SteamAPI_RunCallbacks()
            assert not errors, (state, errors)
            for conn in connections:
                if state.get(conn) != 3: continue
                connection_info = ctypes.create_string_buffer(696)
                assert info(sock, conn, connection_info)
                flags = int.from_bytes(connection_info.raw[440:444], 'little')
                assert flags & 3 == 0, ('P2P must be authenticated and encrypted', flags)
                if os.environ.get('STEAM_TUI_FORCE_RELAY') == '1':
                    assert flags & 16, ('Forced relay test selected a direct route', flags)
                if peer and not sent:
                    assert send(sock, conn, payload, len(payload), 8, None) == 1
                    sent = True
                out = vp()
                count = receive(sock, conn, ctypes.byref(out), 1)
                assert count >= 0
                if not count: continue
                message = ctypes.string_at(out, 216)
                ptr = int.from_bytes(message[:8], 'little')
                size = int.from_bytes(message[8:12], 'little')
                release = ctypes.CFUNCTYPE(None, vp)(int.from_bytes(message[184:192], 'little'))
                expected = completion if not peer and sent else payload
                try: assert ctypes.string_at(ptr, size) == expected
                finally: release(out)
                if not peer:
                    if not sent:
                        assert send(sock, conn, payload, len(payload), 8, None) == 1
                        sent = True
                        continue
                else:
                    assert send(sock, conn, completion, len(completion), 9, None) == 1
                    # Keep the connection alive while the receiver acknowledges
                    # completion. The host must not exit with a lossy echo in flight.
                    for _ in range(200):
                        api.SteamAPI_RunCallbacks(); time.sleep(.01)
                received = True
                break
            if received: break
            time.sleep(.01)
        if not received:
            for conn in connections:
                diagnostic = ctypes.create_string_buffer(8192)
                details(sock, conn, diagnostic, len(diagnostic))
                print(diagnostic.value.decode(errors='replace'), flush=True)
        assert received, ('P2P timeout', dict(state), list(errors))
        route = 'SDR relay' if os.environ.get('STEAM_TUI_FORCE_RELAY') == '1' else 'ICE/SDR'
        print(f'PASS: Steam CM P2P signaling, {route}, authenticated reliable echo', flush=True)
    finally:
        for conn in connections: close(sock, conn, 1000, b'probe complete', False)
        if server: close_listen(sock, server)
        time.sleep(.02)
        api.SteamAPI_RunCallbacks()
        empty = vp()
        config(utils, 201, 1, 0, 5, ctypes.byref(empty))


def storage_cycle(api):
    """File operations and async payloads through Valve's unmodified flat SDK ABI."""
    import time
    vp, i32, u32, u64 = ctypes.c_void_p, ctypes.c_int32, ctypes.c_uint32, ctypes.c_uint64
    obj = api.SteamInternal_FindOrCreateUserInterface(1, b'STEAMREMOTESTORAGE_INTERFACE_VERSION016')
    utils = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamUtils010')
    assert obj and utils
    def bind(label, result, *args):
        fn = getattr(api, 'SteamAPI_ISteamRemoteStorage_' + label)
        fn.argtypes = [vp, *args]; fn.restype = result
        return lambda *values: fn(obj, *values)
    write = bind('FileWrite', ctypes.c_bool, ctypes.c_char_p, vp, i32)
    read = bind('FileRead', i32, ctypes.c_char_p, vp, i32)
    assert write(b'sdk/save.dat', b'SDK save', 8)
    buffer = ctypes.create_string_buffer(32)
    assert read(b'sdk/save.dat', buffer, 32) == 8 and buffer.raw[:8] == b'SDK save'
    call = bind('FileWriteAsync', u64, ctypes.c_char_p, vp, u32)(b'sdk/async.dat', b'async SDK save', 14)
    getresult = api.SteamAPI_ISteamUtils_GetAPICallResult
    getresult.argtypes = [vp, u64, vp, i32, i32, vp]; getresult.restype = ctypes.c_bool
    def result(call, callback, size):
        assert call
        output, failed = ctypes.create_string_buffer(size), ctypes.c_bool()
        end = time.monotonic() + 10
        while time.monotonic() < end:
            api.SteamAPI_RunCallbacks()
            if getresult(utils, call, output, size, callback, ctypes.byref(failed)):
                assert not failed.value
                return output.raw
            time.sleep(.005)
        raise AssertionError('RemoteStorage SDK async result missing')
    assert int.from_bytes(result(call, 1331, 4), 'little') == 1
    call = bind('FileReadAsync', u64, ctypes.c_char_p, u32, u32)(b'sdk/async.dat', 6, 8)
    payload = result(call, 1332, 20)
    assert int.from_bytes(payload[:8], 'little') == call
    assert int.from_bytes(payload[8:12], 'little') == 1
    assert int.from_bytes(payload[16:20], 'little') == 8
    assert bind('FileReadAsyncComplete', ctypes.c_bool, u64, vp, u32)(call, buffer, 32)
    assert buffer.raw[:8] == b'SDK save'
    assert bind('FileDelete', ctypes.c_bool, ctypes.c_char_p)(b'sdk/async.dat')
    print('PASS: Valve SDK RemoteStorage sync/async files and exact callback payloads', flush=True)


def child(api_path):
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    if os.environ.get('STEAM_TUI_PROBE_RUNTIME'):
        release = Path('/etc/os-release').read_text()
        assert 'ID=steamrt' in release, 'Probe is not running inside Steam Runtime'
        libc = ctypes.CDLL(None)
        libc.gnu_get_libc_version.restype = ctypes.c_char_p
        print('Runtime verified; glibc=' + libc.gnu_get_libc_version().decode(), flush=True)
    api = ctypes.CDLL(api_path)
    api.SteamAPI_Init.restype = ctypes.c_bool
    api.SteamAPI_GetHSteamPipe.restype = ctypes.c_int
    api.SteamAPI_GetHSteamUser.restype = ctypes.c_int
    api.SteamAPI_Shutdown.restype = None
    api.SteamAPI_RunCallbacks.restype = None
    api.SteamInternal_FindOrCreateUserInterface.argtypes = [ctypes.c_int, ctypes.c_char_p]
    api.SteamInternal_FindOrCreateUserInterface.restype = ctypes.c_void_p
    api.SteamAPI_ISteamUser_GetSteamID.argtypes = [ctypes.c_void_p]
    api.SteamAPI_ISteamUser_GetSteamID.restype = ctypes.c_uint64
    api.SteamAPI_ISteamUser_BLoggedOn.argtypes = [ctypes.c_void_p]
    api.SteamAPI_ISteamUser_BLoggedOn.restype = ctypes.c_bool
    api.SteamAPI_ISteamApps_BIsSubscribed.argtypes = [ctypes.c_void_p]
    api.SteamAPI_ISteamApps_BIsSubscribed.restype = ctypes.c_bool
    for cycle in range(1 if os.environ.get("STEAM_TUI_PROBE_P2P") else 2):
        if not api.SteamAPI_Init():
            raise RuntimeError("SteamAPI_Init failed; inspect the interface trace")
        assert api.SteamAPI_GetHSteamPipe() > 0
        assert api.SteamAPI_GetHSteamUser() == 1
        # Ensure we really loaded the prototype, not an installed Valve client.
        maps = Path('/proc/self/maps').read_text()
        clients = {line.split()[-1] for line in maps.splitlines()
                   if 'steamclient' in line and '.so' in line}
        assert clients == {os.environ['STEAM_TUI_PROBE_CLIENT']}, clients
        identity = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamUser021')
        apps = api.SteamInternal_FindOrCreateUserInterface(1, b'STEAMAPPS_INTERFACE_VERSION008')
        assert identity and apps
        steam_id = api.SteamAPI_ISteamUser_GetSteamID(identity)
        logged_on = api.SteamAPI_ISteamUser_BLoggedOn(identity)
        owned = api.SteamAPI_ISteamApps_BIsSubscribed(apps)
        if os.environ.get('STEAM_TUI_BACKEND_SOCKET'):
            assert steam_id > 0 and logged_on and owned, (steam_id, logged_on, owned)
        else:
            assert (steam_id, logged_on, owned) == (0, False, False)
        if os.environ.get('STEAM_TUI_PROBE_STORAGE'):
            storage_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_INTERFACES'):
            interface_cycle()
        if os.environ.get('STEAM_TUI_PROBE_TICKETS'):
            ticket_cycle(api, identity)
        if os.environ.get('STEAM_TUI_PROBE_WEB_TICKETS'):
            web_ticket_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_PERSONA'):
            friends = api.SteamInternal_FindOrCreateUserInterface(1, b'SteamFriends017')
            api.SteamAPI_ISteamFriends_GetPersonaName.argtypes = [ctypes.c_void_p]
            api.SteamAPI_ISteamFriends_GetPersonaName.restype = ctypes.c_char_p
            assert friends and api.SteamAPI_ISteamFriends_GetPersonaName(friends)
            print('PASS: persona name received from CM (name omitted)')
        if os.environ.get('STEAM_TUI_PROBE_LOBBIES'):
            lobby_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_PING_CODEC'):
            ping_codec_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_PING_LOBBIES'):
            ping_lobbies_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_NETWORK'):
            network_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_P2P'):
            if os.environ.get('STEAM_TUI_PROBE_MESSAGES', 'STEAM_TUI_WEB_AUTH_FIXTURE'):
                messages_cycle(api)
            else:
                p2p_cycle(api)
        if os.environ.get('STEAM_TUI_PROBE_SOCKETS'):
            sockets_cycle(api, legacy_callbacks=cycle == 1)
        api.SteamAPI_RunCallbacks()
        api.SteamAPI_Shutdown()
        assert api.SteamAPI_GetHSteamPipe() == 0
        assert api.SteamAPI_GetHSteamUser() == 0
    print('PASS: real SteamAPI_Init / identity / ownership / callbacks / shutdown / reinit, no Steam host')


def main():
    if len(sys.argv) == 3 and sys.argv[1] == '--child':
        child(sys.argv[2])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('api', type=Path)
    parser.add_argument('app_id', type=int)
    parser.add_argument('client', type=Path, nargs='?')
    parser.add_argument('--socket', type=Path, help='explicit experimental CM backend socket')
    parser.add_argument('--web-tickets', action='store_true', help='request/cancel CM web API tickets and verify SDK callback 168')
    parser.add_argument('--tickets', action='store_true', help='request/cancel a CM auth ticket and verify its SDK callback')
    parser.add_argument('--runtime', type=Path, help='run inside this installed Steam Linux Runtime via pressure-vessel')
    parser.add_argument('--lobbies', action='store_true', help='create/edit/search/leave a real lobby (app 480, needs --socket)')
    parser.add_argument('--p2p-listen', action='store_true', help='listen for a CM/ICE P2P probe on virtual port 7777')
    parser.add_argument('--p2p-peer', type=int, help='connect to the peer SteamID and exchange a reliable echo')
    parser.add_argument('--storage', action='store_true', help='isolated RemoteStorage cache and async results; no account required')
    parser.add_argument('--interfaces', action='store_true', help='Verify typed interface factory coverage')
    parser.add_argument('--sockets', action='store_true', help='real UDP loopback and callbacks; no backend required')
    parser.add_argument('--ping-codec', action='store_true', help='Valve ping marker ABI fixtures; no backend required')
    parser.add_argument('--ping-lobbies', action='store_true', help='read-only public lobby ping estimates (needs --socket)')
    parser.add_argument('--network', action='store_true', help='ping locations and address/identity strings (needs --socket)')
    parser.add_argument('--persona', action='store_true', help='verify a nonempty CM persona name through SteamFriends017')
    args = parser.parse_args()
    if (args.web_tickets or args.tickets or args.persona or args.lobbies or args.network or args.ping_lobbies or args.p2p_listen or args.p2p_peer) and not args.socket:
        parser.error('--tickets, --persona and --lobbies require --socket')
    api = args.api.resolve(strict=True)
    app_id = args.app_id
    if not 0 < app_id <= 0xFFFFFFFF:
        sys.exit('APPID must be a nonzero u32')
    workspace = Path(__file__).resolve().parents[2]
    client = (args.client if args.client else
              workspace / 'target/debug/libsteamclient.so').resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix='steam-tui-client-probe-') as scratch:
        home = Path(scratch)
        sdk = home / '.steam/sdk64'
        sdk.mkdir(parents=True)
        (sdk / 'steamclient.so').symlink_to(client)
        (home / 'steamclient.so').symlink_to(client)
        # Whitelist environment: no account tokens, preload hooks, or real HOME.
        env = {
            'HOME': scratch, 'PATH': '/usr/bin:/bin',
            'SteamAppId': str(app_id), 'SteamGameId': str(app_id),
            'STEAM_TUI_PROBE_CLIENT': str(client),
            'LD_LIBRARY_PATH': scratch,
        }
        if os.environ.get('STEAM_TUI_CRASH_TRACE'):
            env['STEAM_TUI_CRASH_TRACE'] = '1'
        if os.environ.get('STEAM_TUI_FORCE_RELAY') == '1':
            env['STEAM_TUI_FORCE_RELAY'] = '1'
        for key in ('STEAM_TUI_PROBE_P2P_BYTES', 'STEAM_TUI_SDR_TEST_DROP_EVERY', 'STEAM_TUI_PROBE_MESSAGES', 'STEAM_TUI_WEB_AUTH_FIXTURE'):
            if key in os.environ:
                env[key] = os.environ[key]
        if args.socket:
            env['STEAM_TUI_BACKEND_SOCKET'] = str(args.socket.resolve(strict=True))
        if args.web_tickets:
            env['STEAM_TUI_PROBE_WEB_TICKETS'] = '1'
        if args.tickets:
            env['STEAM_TUI_PROBE_TICKETS'] = '1'
        if args.persona:
            env['STEAM_TUI_PROBE_PERSONA'] = '1'
        if args.lobbies:
            env['STEAM_TUI_PROBE_LOBBIES'] = '1'
        if args.p2p_listen or args.p2p_peer:
            env['STEAM_TUI_PROBE_P2P'] = '1'
            env['STEAM_TUI_PROBE_P2P_PEER'] = str(args.p2p_peer or 0)
        if args.storage:
            env['STEAM_TUI_PROBE_STORAGE'] = '1'
            env['STEAM_TUI_REMOTE_STORAGE'] = str(home / 'remote')
        if args.interfaces:
            env['STEAM_TUI_PROBE_INTERFACES'] = '1'
        if args.sockets:
            env['STEAM_TUI_PROBE_SOCKETS'] = '1'
        if args.ping_codec:
            env['STEAM_TUI_PROBE_PING_CODEC'] = '1'
        if args.ping_lobbies:
            env['STEAM_TUI_PROBE_PING_LOBBIES'] = '1'
        if args.network:
            env['STEAM_TUI_PROBE_NETWORK'] = '1'
        command = [sys.executable, str(Path(__file__).resolve()), '--child', str(api)]
        if args.runtime:
            runtime = args.runtime.resolve(strict=True)
            exposed = [scratch, str(workspace), str(api.parent)]
            if args.socket:
                exposed.append(str(args.socket.resolve(strict=True).parent))
            env.update({
                'STEAM_TUI_PROBE_RUNTIME': '1',
                'PRESSURE_VESSEL_FILESYSTEMS_RW': ':'.join(exposed),
                'STEAM_COMPAT_CLIENT_INSTALL_PATH': scratch,
                'STEAM_COMPAT_INSTALL_PATH': str(api.parent),
                'STEAM_COMPAT_APP_ID': str(app_id),
            })
            if os.environ.get('XDG_RUNTIME_DIR'):
                env['XDG_RUNTIME_DIR'] = os.environ['XDG_RUNTIME_DIR']
            command = [str(runtime / 'run'), '--', '/usr/bin/python3',
                       str(Path(__file__).resolve()), '--child', str(api)]
        subprocess.run(command,
                       cwd=scratch, env=env, check=True, timeout=60 if (args.web_tickets or args.tickets or args.lobbies or args.network or args.sockets or args.p2p_listen or args.p2p_peer) else 20)


if __name__ == '__main__':
    main()

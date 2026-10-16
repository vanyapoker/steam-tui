#!/usr/bin/env python3
"""RemoteStorage ABI regression, isolated saves only; no Steam, login or game.
Usage: python3 crates/steam-client/tests/remotestorage.py target/debug/libsteamclient.so
"""
import ctypes as C
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def child(library):
    lib = C.CDLL(library)
    vp, i32, u32, u64, boolean, text = C.c_void_p, C.c_int32, C.c_uint32, C.c_uint64, C.c_bool, C.c_char_p
    lib.CreateInterface.argtypes = [text, vp]
    lib.CreateInterface.restype = vp
    client = lib.CreateInterface(b'SteamClient020', None)
    def method(obj, slot, result, *args):
        table = C.cast(obj, C.POINTER(C.POINTER(vp))).contents
        fn = C.CFUNCTYPE(result, vp, *args)(table[slot])
        return lambda *values: fn(obj, *values)
    pipe = method(client, 0, i32)()
    assert method(client, 2, i32, i32)(pipe) == 1
    obj = method(client, 17, vp, i32, i32, text)(1, pipe, b'STEAMREMOTESTORAGE_INTERFACE_VERSION016')
    assert obj
    f = lambda slot, result, *args: method(obj, slot, result, *args)
    write, read = f(0, boolean, text, vp, i32), f(1, i32, text, vp, i32)
    exists, persisted = f(13, boolean, text), f(14, boolean, text)
    count, getname = f(18, i32), f(19, text, i32, vp)
    root = Path(os.environ['STEAM_TUI_REMOTE_STORAGE'])
    assert write(b'Saves\\Slot.DAT', b'progress', 8)
    assert (root / 'saves/slot.dat').read_bytes() == b'progress'
    assert exists(b'SAVES/Slot.dat') and not persisted(b'saves/slot.dat')
    buffer = C.create_string_buffer(16)
    assert read(b'saves/slot.dat', buffer, 3) == 3 and buffer.raw[:3] == b'pro'
    assert read(b'missing', buffer, 16) == 0
    assert not write(b'../outside', b'bad', 3)
    assert not write(b'/absolute', b'bad', 3)
    assert not write(b'x:y', b'bad', 3)
    assert not write(b'x', None, 3) and not write(None, b'x', 1)
    outside = root.parent / 'outside'
    outside.mkdir()
    (outside / 'victim').write_bytes(b'original')
    (root / 'escape').symlink_to(outside, target_is_directory=True)
    assert not write(b'escape/victim', b'bad', 3)
    assert read(b'escape/victim', buffer, 16) == 0
    assert (outside / 'victim').read_bytes() == b'original'
    assert count() == 1
    length = i32(-1)
    assert getname(0, C.byref(length)) == b'saves/slot.dat' and length.value == 8
    assert getname(-1, C.byref(length)) == b'' and length.value == 0
    total, free = u64(), u64()
    assert f(20, boolean, vp, vp)(C.byref(total), C.byref(free))
    assert total.value - free.value == 8
    assert f(8, boolean, text, u32)(b'saves/slot.dat', 9)
    assert f(17, u32, text)(b'saves/slot.dat') == 9
    assert f(21, boolean)() and f(22, boolean)()
    f(23, None, boolean)(False)
    assert not f(22, boolean)()
    assert write(b'offline.dat', b'offline', 7)  # Disabling cloud must not disable local saves.
    f(23, None, boolean)(True)

    open_stream, append = f(9, u64, text), f(10, boolean, u64, vp, i32)
    close_stream, cancel_stream = f(11, boolean, u64), f(12, boolean, u64)
    handle = open_stream(b'saves/slot.dat')
    assert handle != (1 << 64) - 1
    assert append(handle, b'new', 3)
    assert read(b'saves/slot.dat', buffer, 16) == 8  # Uncommitted bytes are invisible.
    assert cancel_stream(handle) and not close_stream(handle)
    assert (root / 'saves/slot.dat').read_bytes() == b'progress'
    handle = open_stream(b'large.dat')
    chunk = b'A' * (4 * 1024 * 1024)
    for _ in range(17):
        assert append(handle, chunk, len(chunk))
    assert close_stream(handle)
    assert (root / 'large.dat').stat().st_size == 68 * 1024 * 1024
    assert not close_stream(handle)
    # Quota replacement credits the old file, new files cannot exceed quota.
    old_quota = os.environ.get('STEAM_TUI_REMOTE_QUOTA')
    os.environ['STEAM_TUI_REMOTE_QUOTA'] = str(sum(size for _, size in [(p, p.stat().st_size) for p in root.rglob('*') if p.is_file() and not p.name.startswith('.steam-tui-cloud-')]))
    assert not write(b'quota.dat', b'X', 1)
    if old_quota is None: del os.environ['STEAM_TUI_REMOTE_QUOTA']
    else: os.environ['STEAM_TUI_REMOTE_QUOTA'] = old_quota

    lib.Steam_GetAPICallResult.argtypes = [i32, u64, vp, i32, i32, vp]
    lib.Steam_GetAPICallResult.restype = boolean
    lib.Steam_GetAPICallResult.restype = C.c_uint8
    missing_result, missing_failed = C.create_string_buffer(4), boolean()
    raw = lib.Steam_GetAPICallResult(pipe, 0, missing_result, 4, 1331, C.byref(missing_failed))
    assert raw == 0 and missing_failed.value, ('noncanonical SDK boolean for missing call', raw)
    lib.Steam_GetAPICallResult.restype = boolean
    def result(call, callback, size):
        assert call
        output = C.create_string_buffer(size)
        failed = boolean()
        end = time.monotonic() + 10
        while time.monotonic() < end:
            if lib.Steam_GetAPICallResult(pipe, call, output, size, callback, C.byref(failed)):
                assert not failed.value
                return output.raw
            time.sleep(.005)
        raise AssertionError('async completion missing')
    call = f(2, u64, text, vp, u32)(b'async.dat', b'async progress', 14)
    assert int.from_bytes(result(call, 1331, 4), 'little') == 1
    read_call = f(3, u64, text, u32, u32)(b'async.dat', 6, 8)
    payload = result(read_call, 1332, 20)
    assert int.from_bytes(payload[:8], 'little') == read_call
    assert int.from_bytes(payload[8:12], 'little') == 1
    assert int.from_bytes(payload[16:20], 'little') == 8
    complete = f(4, boolean, u64, vp, u32)
    assert not complete(read_call, buffer, 1)  # Caller may retry with enough space.
    assert complete(read_call, buffer, 16) and buffer.raw[:8] == b'progress'
    assert not complete(read_call, buffer, 16)
    bad = f(3, u64, text, u32, u32)(b'async.dat', 99999, 1)
    assert int.from_bytes(result(bad, 1332, 20)[8:12], 'little') == 2
    assert f(5, boolean, text)(b'async.dat') and exists(b'async.dat') and not persisted(b'async.dat')
    assert f(6, boolean, text)(b'async.dat') and not exists(b'async.dat')
    meta = json.loads((root / '.steam-tui-cloud-metadata').read_text())
    assert 'async.dat' in meta['deleted']
    assert f(55, i32)() == 0
    change, kind = i32(99), i32(99)
    assert f(56, text, i32, vp, vp)(0, C.byref(change), C.byref(kind)) == b''
    assert change.value == 0 and kind.value == 0
    assert f(57, boolean)() and f(58, boolean)()
    abandoned = open_stream(b'abandoned.dat')
    assert append(abandoned, b'x', 1)
    assert method(client, 1, boolean, i32)(pipe)
    assert not list(root.rglob('.steam-tui-cloud-[0-9]*'))
    pipe = method(client, 0, i32)()
    assert method(client, 2, i32, i32)(pipe) == 1
    assert read(b'saves/slot.dat', buffer, 16) == 8
    assert method(client, 1, boolean, i32)(pipe)
    print('PASS: RemoteStorage ABI, durable files, case normalization, quotas, 68 MiB stream, cancellation, async results, masks, delete/forget, shutdown/reinit, containment')

if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--child': child(sys.argv[2])
    else:
        library = str(Path(sys.argv[1]).resolve(strict=True))
        with tempfile.TemporaryDirectory(prefix='steam-tui-storage-probe-') as directory:
            root = Path(directory) / 'remote'
            env = {'PATH': '/usr/bin:/bin', 'HOME': directory, 'SteamAppId': '480', 'STEAM_TUI_REMOTE_STORAGE': str(root)}
            subprocess.run([sys.executable, __file__, '--child', library], env=env, check=True, timeout=60)
